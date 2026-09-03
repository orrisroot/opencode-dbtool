//! Data-access layer: every SQL statement that reads or writes
//! session/project tables lives here, so commands stay SQL-free
//! and testable against a fake schema.

use crate::error::{AppError, Result};
use crate::models::{ProjectRow, SessionMeta, SessionRow};
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::Path;

/// Column list shared by `load_sessions` and `load_session`: the 12
/// session columns plus per-session message/part/event counts and sizes.
const SESSION_COLS: &str = "s.id, s.title, s.directory, s.parent_id, s.time_updated, s.cost, \
     (SELECT COUNT(*) FROM message m WHERE m.session_id = s.id), \
     (SELECT COALESCE(SUM(length(CAST(m.data AS BLOB))),0) FROM message m WHERE m.session_id = s.id), \
     (SELECT COUNT(*) FROM part p WHERE p.session_id = s.id), \
     (SELECT COALESCE(SUM(length(CAST(p.data AS BLOB))),0) FROM part p WHERE p.session_id = s.id), \
     (SELECT COUNT(*) FROM event e WHERE e.aggregate_id = s.id), \
     (SELECT COALESCE(SUM(length(CAST(e.data AS BLOB))),0) FROM event e WHERE e.aggregate_id = s.id)";

/// Session row columns as read by SQLite (per-session aggregates
/// included), before `diff_bytes` is attached.
struct SessionAgg {
    id: String,
    title: String,
    directory: String,
    parent_id: Option<String>,
    updated: i64,
    cost: f64,
    msgs: i64,
    msg_bytes: i64,
    parts: i64,
    part_bytes: i64,
    events: i64,
    event_bytes: i64,
}

fn read_session_row(r: &rusqlite::Row) -> rusqlite::Result<SessionAgg> {
    Ok(SessionAgg {
        id: r.get(0)?,
        title: r.get(1)?,
        directory: r.get(2)?,
        parent_id: r.get(3)?,
        updated: r.get(4)?,
        cost: r.get(5)?,
        msgs: r.get(6)?,
        msg_bytes: r.get(7)?,
        parts: r.get(8)?,
        part_bytes: r.get(9)?,
        events: r.get(10)?,
        event_bytes: r.get(11)?,
    })
}

fn diff_bytes_for(dir: Option<&Path>, id: &str) -> i64 {
    dir.map(|d| {
        std::fs::metadata(d.join(format!("{id}.json")))
            .map(|m| m.len() as i64)
            .unwrap_or(0)
    })
    .unwrap_or(0)
}

fn make_session_row(a: SessionAgg, diff_dir: Option<&Path>) -> SessionRow {
    let diff_bytes = diff_bytes_for(diff_dir, &a.id);
    SessionRow {
        id: a.id,
        title: a.title,
        directory: a.directory,
        parent_id: a.parent_id,
        updated: a.updated,
        msgs: a.msgs,
        msg_bytes: a.msg_bytes,
        parts: a.parts,
        part_bytes: a.part_bytes,
        events: a.events,
        event_bytes: a.event_bytes,
        diff_bytes,
        cost: a.cost,
    }
}

/// Light session fields for filter selection, ordered by
/// `time_updated` desc (id as tiebreaker), the same order `load_sessions`
/// uses.
pub fn load_session_meta(con: &Connection) -> Result<Vec<SessionMeta>> {
    let mut stmt = con.prepare(
        "SELECT id, directory, parent_id, time_updated FROM session \
         ORDER BY time_updated DESC, id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(SessionMeta {
            id: r.get(0)?,
            directory: r.get(1)?,
            parent_id: r.get(2)?,
            updated: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Own size (msg+part+event bytes) per session for the given ids, using
/// one batched GROUP BY query per table (chunked to stay under SQLite's
/// variable limit) instead of per-session scans. Sessions without rows
/// in a table contribute 0.
pub fn session_sizes(con: &Connection, ids: &[String]) -> Result<HashMap<String, i64>> {
    let mut map: HashMap<String, i64> = HashMap::new();
    if ids.is_empty() {
        return Ok(map);
    }
    for (table, id_col) in [
        ("message", "session_id"),
        ("part", "session_id"),
        ("event", "aggregate_id"),
    ] {
        for chunk in ids.chunks(crate::util::SQL_VAR_CHUNK) {
            let sql = format!(
                "SELECT {id_col}, COALESCE(SUM(length(CAST(data AS BLOB))),0) \
                 FROM {table} WHERE {id_col} IN ({}) GROUP BY {id_col}",
                in_clause(chunk)
            );
            let mut stmt = con.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            for r in rows {
                let (id, bytes) = r?;
                *map.entry(id).or_insert(0) += bytes;
            }
        }
    }
    Ok(map)
}

/// Direct children of a session (recursive subagent sessions).
pub fn child_session_ids(con: &Connection, id: &str) -> Result<Vec<String>> {
    let mut stmt = con.prepare("SELECT id FROM session WHERE parent_id = ?1")?;
    let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Look up session ids by exact id; errors when any id is unknown.
pub fn resolve_session_ids(con: &Connection, id_args: &[&str]) -> Result<Vec<String>> {
    let mut resolved: Vec<String> = Vec::new();
    for id in id_args {
        let ids_found: Vec<String> = {
            let mut stmt = con.prepare("SELECT id FROM session WHERE id = ?1")?;
            let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        if ids_found.is_empty() {
            return Err(AppError::usage(format!("session not found: {id}")));
        }
        resolved.push(ids_found.into_iter().next().unwrap());
    }
    Ok(resolved)
}

/// All sessions with per-session message/part/event counts and sizes.
/// When `diff_dir` is given, each session's `diff_bytes` is read from
/// `<diff_dir>/<id>.json` (0 when missing).
pub fn load_sessions(con: &Connection, diff_dir: Option<&Path>) -> Result<Vec<SessionRow>> {
    let sql = format!("SELECT {SESSION_COLS} FROM session s ORDER BY s.time_updated DESC, s.id");
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map([], read_session_row)?;
    let mut out = Vec::new();
    for r in rows {
        let a = r?;
        out.push(make_session_row(a, diff_dir));
    }
    Ok(out)
}

/// A single session by exact id, with the same aggregate columns as
/// `load_sessions`; `None` when the id is unknown.
pub fn load_session(
    con: &Connection,
    id: &str,
    diff_dir: Option<&Path>,
) -> Result<Option<SessionRow>> {
    let sql = format!("SELECT {SESSION_COLS} FROM session s WHERE s.id = ?1");
    match con.query_row(&sql, params![id], read_session_row) {
        Ok(a) => Ok(Some(make_session_row(a, diff_dir))),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(AppError::db(e.to_string())),
    }
}

/// All projects with aggregated session stats. The aggregation subqueries
/// scan each table once (GROUP BY project) instead of running correlated
/// counts per session/project.
pub fn load_projects(con: &Connection) -> Result<Vec<ProjectRow>> {
    let sql = format!(
        "SELECT p.id, p.worktree, COALESCE(p.name,''), \
         COALESCE(s.sessions,0), \
         COALESCE(m.msgs,0), COALESCE(m.msg_bytes,0), \
         COALESCE(parts.parts,0), COALESCE(parts.part_bytes,0), \
         COALESCE(e.events,0), COALESCE(e.event_bytes,0), \
         COALESCE(s.cost,0), COALESCE(s.updated,0) \
         {FROM_PROJECTS} ORDER BY p.worktree",
        FROM_PROJECTS = from_projects()
    );
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map([], read_project_row)?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// A single project by exact id with the same aggregated stats;
/// `None` when the id is unknown.
pub fn load_project(con: &Connection, id: &str) -> Result<Option<ProjectRow>> {
    let sql = format!(
        "SELECT p.id, p.worktree, COALESCE(p.name,''), \
         COALESCE(s.sessions,0), \
         COALESCE(m.msgs,0), COALESCE(m.msg_bytes,0), \
         COALESCE(parts.parts,0), COALESCE(parts.part_bytes,0), \
         COALESCE(e.events,0), COALESCE(e.event_bytes,0), \
         COALESCE(s.cost,0), COALESCE(s.updated,0) \
         {FROM_PROJECTS} WHERE p.id = ?1",
        FROM_PROJECTS = from_projects()
    );
    match con.query_row(&sql, params![id], read_project_row) {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(AppError::db(e.to_string())),
    }
}

/// Shared FROM/JOIN clause for project aggregation queries: session
/// stats (count/cost/latest activity) plus per-table message/part/event
/// aggregates, one grouping subquery per table.
fn from_projects() -> &'static str {
    "FROM project p \
     LEFT JOIN (SELECT project_id, COUNT(*) sessions, COALESCE(SUM(cost),0) cost, \
                MAX(time_updated) updated \
                FROM session GROUP BY project_id) s ON s.project_id = p.id \
     LEFT JOIN (SELECT s2.project_id, COUNT(*) msgs, \
                COALESCE(SUM(length(CAST(m.data AS BLOB))),0) msg_bytes \
                FROM session s2 JOIN message m ON m.session_id = s2.id \
                GROUP BY s2.project_id) m ON m.project_id = p.id \
     LEFT JOIN (SELECT s2.project_id, COUNT(*) parts, \
                COALESCE(SUM(length(CAST(p2.data AS BLOB))),0) part_bytes \
                FROM session s2 JOIN part p2 ON p2.session_id = s2.id \
                GROUP BY s2.project_id) parts ON parts.project_id = p.id \
     LEFT JOIN (SELECT s2.project_id, COUNT(*) events, \
                COALESCE(SUM(length(CAST(e2.data AS BLOB))),0) event_bytes \
                FROM session s2 JOIN event e2 ON e2.aggregate_id = s2.id \
                GROUP BY s2.project_id) e ON e.project_id = p.id"
}

fn read_project_row(r: &rusqlite::Row) -> rusqlite::Result<ProjectRow> {
    Ok(ProjectRow {
        id: r.get(0)?,
        worktree: r.get(1)?,
        name: r.get(2)?,
        sessions: r.get(3)?,
        msgs: r.get(4)?,
        msg_bytes: r.get(5)?,
        parts: r.get(6)?,
        part_bytes: r.get(7)?,
        events: r.get(8)?,
        event_bytes: r.get(9)?,
        cost: r.get(10)?,
        updated: r.get(11)?,
    })
}

fn project_row_from_row(r: &rusqlite::Row) -> rusqlite::Result<ProjectRow> {
    Ok(ProjectRow {
        id: r.get(0)?,
        worktree: r.get(1)?,
        name: r.get(2)?,
        sessions: 0,
        msgs: 0,
        msg_bytes: 0,
        parts: 0,
        part_bytes: 0,
        events: 0,
        event_bytes: 0,
        cost: 0.0,
        updated: 0,
    })
}

/// Look up a single project by id or worktree (aggregated fields zeroed).
pub fn lookup_project(
    con: &Connection,
    key: &str,
    by_worktree: bool,
) -> Result<Option<ProjectRow>> {
    let sql = if by_worktree {
        "SELECT id, worktree, COALESCE(name,'') FROM project WHERE worktree = ?1"
    } else {
        "SELECT id, worktree, COALESCE(name,'') FROM project WHERE id = ?1"
    };
    match con.query_row(sql, params![key], project_row_from_row) {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(AppError::db(e.to_string())),
    }
}

fn in_clause(ids: &[String]) -> String {
    vec!["?"; ids.len()].join(",")
}

/// Per-session reasoning part counts and bytes for the given ids.
/// Sessions without reasoning parts are omitted.
pub fn reasoning_counts(con: &Connection, ids: &[String]) -> Result<Vec<(String, i64, i64)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT session_id, COUNT(*), COALESCE(SUM(length(CAST(data AS BLOB))),0) \
         FROM part WHERE session_id IN ({}) AND json_extract(data, '$.type') = 'reasoning' \
         GROUP BY session_id",
        in_clause(ids)
    );
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(ids), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Delete reasoning parts for the given session ids; returns rows removed.
pub fn strip_reasoning(con: &Connection, ids: &[String]) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let sql = format!(
        "DELETE FROM part WHERE session_id IN ({}) AND json_extract(data, '$.type') = 'reasoning'",
        in_clause(ids)
    );
    Ok(con.execute(&sql, rusqlite::params_from_iter(ids))?)
}

/// Reasoning parts remaining for the given ids (post-strip verification).
pub fn reasoning_left(con: &Connection, ids: &[String]) -> Result<i64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let sql = format!(
        "SELECT COUNT(*) FROM part WHERE session_id IN ({}) \
         AND json_extract(data, '$.type') = 'reasoning'",
        in_clause(ids)
    );
    Ok(con.query_row(&sql, rusqlite::params_from_iter(ids), |r| r.get(0))?)
}

/// Durable reasoning event types (`session.next.reasoning.delta` is
/// live-only and never persisted).
const REASONING_EVENT_TYPES: [&str; 2] = [
    "session.next.reasoning.started",
    "session.next.reasoning.ended",
];

/// Parameter list of session ids followed by the reasoning event types.
fn reasoning_event_params(ids: &[String]) -> Vec<rusqlite::types::Value> {
    let mut p: Vec<rusqlite::types::Value> = ids
        .iter()
        .map(|s| rusqlite::types::Value::Text(s.clone()))
        .collect();
    p.extend(
        REASONING_EVENT_TYPES
            .iter()
            .map(|t| rusqlite::types::Value::Text((*t).to_string())),
    );
    p
}

/// Per-session reasoning event counts and bytes for the given ids.
/// Sessions without matching events are omitted.
pub fn reasoning_event_counts(con: &Connection, ids: &[String]) -> Result<Vec<(String, i64, i64)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT aggregate_id, COUNT(*), COALESCE(SUM(length(CAST(data AS BLOB))),0) \
         FROM event WHERE aggregate_id IN ({}) AND type IN ({}) \
         GROUP BY aggregate_id",
        in_clause(ids),
        vec!["?"; REASONING_EVENT_TYPES.len()].join(",")
    );
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params_from_iter(reasoning_event_params(ids)),
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        },
    )?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Delete reasoning events for the given ids; returns rows removed.
pub fn strip_reasoning_events(con: &Connection, ids: &[String]) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let sql = format!(
        "DELETE FROM event WHERE aggregate_id IN ({}) AND type IN ({})",
        in_clause(ids),
        vec!["?"; REASONING_EVENT_TYPES.len()].join(",")
    );
    Ok(con.execute(
        &sql,
        rusqlite::params_from_iter(reasoning_event_params(ids)),
    )?)
}

/// Reasoning events remaining for the given ids (post-strip verification).
pub fn reasoning_events_left(con: &Connection, ids: &[String]) -> Result<i64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let sql = format!(
        "SELECT COUNT(*) FROM event WHERE aggregate_id IN ({}) AND type IN ({})",
        in_clause(ids),
        vec!["?"; REASONING_EVENT_TYPES.len()].join(",")
    );
    Ok(con.query_row(
        &sql,
        rusqlite::params_from_iter(reasoning_event_params(ids)),
        |r| r.get(0),
    )?)
}

/// Assistant `session_message` rows (id, session_id, data) for the
/// given ids, in seq order.
pub fn assistant_messages(
    con: &Connection,
    ids: &[String],
) -> Result<Vec<(String, String, String)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT id, session_id, data FROM session_message \
         WHERE session_id IN ({}) AND type = 'assistant' ORDER BY session_id, seq",
        in_clause(ids)
    );
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(ids), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Rewrite a session_message row's data payload.
pub fn rewrite_message(con: &Connection, id: &str, data: &str) -> Result<()> {
    con.execute(
        "UPDATE session_message SET data = ?1 WHERE id = ?2",
        params![data, id],
    )?;
    Ok(())
}

/// Assistant messages still containing reasoning content (post-strip
/// verification).
///
/// Valid JSON is checked structurally: `content[]` items with
/// `type: "reasoning"` via the JSON functions, mirroring
/// `sanitize_assistant_data`; whitespace in the stored JSON and literal
/// `"type":"reasoning"` text inside other items do not affect the
/// result, and `json_each` is never evaluated against a malformed
/// document (the `CASE` substitutes `[]`).
///
/// Rows that are not valid JSON cannot be stripped by
/// `sanitize_assistant_data`, so they are counted as "left" when they
/// still contain a reasoning marker — the strip then reports a leftover
/// instead of silently succeeding on corrupt data.
pub fn reasoning_messages_left(con: &Connection, ids: &[String]) -> Result<i64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let sql = format!(
        "SELECT COUNT(*) FROM session_message WHERE session_id IN ({}) \
         AND type = 'assistant' \
         AND (EXISTS (SELECT 1 FROM json_each( \
                        CASE WHEN json_valid(data) AND json_type(data, '$.content') = 'array' \
                             THEN data ELSE '[]' END, '$.content') \
                     WHERE json_extract(value, '$.type') = 'reasoning') \
              OR (NOT json_valid(data) AND data LIKE '%\"type\":\"reasoning\"%'))",
        in_clause(ids)
    );
    Ok(con.query_row(&sql, rusqlite::params_from_iter(ids), |r| r.get(0))?)
}

/// Per-table row counts for a project delete preview, including the
/// project row itself and every cascade-eligible table.
pub fn project_impact(con: &Connection, project_id: &str) -> Result<Vec<(String, i64)>> {
    let counts: Vec<(String, i64)> = {
        let mut stmt = con.prepare(
            "SELECT 'session', COUNT(*) FROM session WHERE project_id = ?1 \
             UNION ALL SELECT 'message', (SELECT COALESCE(SUM(c),0) FROM (SELECT COUNT(*) c FROM message m JOIN session s ON m.session_id = s.id WHERE s.project_id = ?1)) \
             UNION ALL SELECT 'part', (SELECT COALESCE(SUM(c),0) FROM (SELECT COUNT(*) c FROM part p JOIN session s ON p.session_id = s.id WHERE s.project_id = ?1)) \
             UNION ALL SELECT 'todo', (SELECT COALESCE(SUM(c),0) FROM (SELECT COUNT(*) c FROM todo t JOIN session s ON t.session_id = s.id WHERE s.project_id = ?1)) \
             UNION ALL SELECT 'session_message', (SELECT COALESCE(SUM(c),0) FROM (SELECT COUNT(*) c FROM session_message sm JOIN session s ON sm.session_id = s.id WHERE s.project_id = ?1)) \
             UNION ALL SELECT 'session_input', (SELECT COALESCE(SUM(c),0) FROM (SELECT COUNT(*) c FROM session_input si JOIN session s ON si.session_id = s.id WHERE s.project_id = ?1)) \
             UNION ALL SELECT 'session_share', (SELECT COALESCE(SUM(c),0) FROM (SELECT COUNT(*) c FROM session_share ss JOIN session s ON ss.session_id = s.id WHERE s.project_id = ?1)) \
             UNION ALL SELECT 'session_context_epoch', (SELECT COALESCE(SUM(c),0) FROM (SELECT COUNT(*) c FROM session_context_epoch sce JOIN session s ON sce.session_id = s.id WHERE s.project_id = ?1)) \
             UNION ALL SELECT 'permission', COUNT(*) FROM permission WHERE project_id = ?1 \
             UNION ALL SELECT 'project_directory', COUNT(*) FROM project_directory WHERE project_id = ?1 \
             UNION ALL SELECT 'workspace', COUNT(*) FROM workspace WHERE project_id = ?1 \
             UNION ALL SELECT 'event', (SELECT COALESCE(SUM(c),0) FROM (SELECT COUNT(*) c FROM event e WHERE e.aggregate_id IN (SELECT id FROM session WHERE project_id = ?1))) \
             UNION ALL SELECT 'event_sequence', (SELECT COALESCE(SUM(c),0) FROM (SELECT COUNT(*) c FROM event_sequence es WHERE es.aggregate_id IN (SELECT id FROM session WHERE project_id = ?1)))",
        )?;
        let rows = stmt.query_map(params![project_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut out = vec![("project".to_string(), 1)];
    out.extend(counts);
    Ok(out)
}
