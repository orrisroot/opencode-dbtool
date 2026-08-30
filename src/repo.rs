//! Data-access layer: every SQL statement that reads or writes
//! session/project tables lives here, so commands stay SQL-free
//! and testable against a fake schema.

use crate::error::{AppError, Result};
use crate::models::{ProjectRow, SessionRow};
use rusqlite::{params, Connection};

pub fn session_exists(con: &Connection, id: &str) -> Result<bool> {
    let n: i64 = con.query_row(
        "SELECT COUNT(*) FROM session WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
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
pub fn load_sessions(
    con: &Connection,
    diff_dir: Option<&std::path::Path>,
) -> Result<Vec<SessionRow>> {
    let mut stmt = con.prepare(
        "SELECT s.id, s.title, s.directory, s.parent_id, s.time_updated, s.cost, \
         (SELECT COUNT(*) FROM message m WHERE m.session_id = s.id), \
         (SELECT COALESCE(SUM(length(CAST(m.data AS BLOB))),0) FROM message m WHERE m.session_id = s.id), \
         (SELECT COUNT(*) FROM part p WHERE p.session_id = s.id), \
         (SELECT COALESCE(SUM(length(CAST(p.data AS BLOB))),0) FROM part p WHERE p.session_id = s.id), \
         (SELECT COUNT(*) FROM event e WHERE e.aggregate_id = s.id), \
         (SELECT COALESCE(SUM(length(CAST(e.data AS BLOB))),0) FROM event e WHERE e.aggregate_id = s.id) \
         FROM session s ORDER BY s.time_updated DESC, s.id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, f64>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, i64>(8)?,
            r.get::<_, i64>(9)?,
            r.get::<_, i64>(10)?,
            r.get::<_, i64>(11)?,
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (
            id,
            title,
            directory,
            parent_id,
            updated,
            cost,
            msgs,
            msg_bytes,
            parts,
            part_bytes,
            events,
            event_bytes,
        ) = r?;
        let diff_bytes = match diff_dir {
            Some(dir) => std::fs::metadata(dir.join(format!("{id}.json")))
                .map(|m| m.len() as i64)
                .unwrap_or(0),
            None => 0,
        };
        out.push(SessionRow {
            id,
            title,
            directory,
            parent_id,
            updated,
            msgs,
            msg_bytes,
            parts,
            part_bytes,
            events,
            event_bytes,
            diff_bytes,
            cost,
        });
    }
    Ok(out)
}

/// All projects with aggregated session stats.
pub fn load_projects(con: &Connection) -> Result<Vec<ProjectRow>> {
    let mut stmt = con.prepare(
        "SELECT p.id, p.worktree, COALESCE(p.name,''), \
         (SELECT COUNT(*) FROM session s WHERE s.project_id = p.id), \
         (SELECT COALESCE(SUM((SELECT COUNT(*) FROM message m WHERE m.session_id = s.id)),0) FROM session s WHERE s.project_id = p.id), \
          (SELECT COALESCE(SUM((SELECT COALESCE(SUM(length(CAST(m.data AS BLOB))),0) FROM message m WHERE m.session_id = s.id)),0) FROM session s WHERE s.project_id = p.id), \
         (SELECT COALESCE(SUM((SELECT COUNT(*) FROM part p2 WHERE p2.session_id = s.id)),0) FROM session s WHERE s.project_id = p.id), \
          (SELECT COALESCE(SUM((SELECT COALESCE(SUM(length(CAST(p2.data AS BLOB))),0) FROM part p2 WHERE p2.session_id = s.id)),0) FROM session s WHERE s.project_id = p.id), \
         (SELECT COALESCE(SUM((SELECT COUNT(*) FROM event e WHERE e.aggregate_id = s.id)),0) FROM session s WHERE s.project_id = p.id), \
          (SELECT COALESCE(SUM((SELECT COALESCE(SUM(length(CAST(e.data AS BLOB))),0) FROM event e WHERE e.aggregate_id = s.id)),0) FROM session s WHERE s.project_id = p.id), \
         (SELECT COALESCE(SUM(s.cost),0) FROM session s WHERE s.project_id = p.id), \
         (SELECT COALESCE(MAX(s.time_updated),0) FROM session s WHERE s.project_id = p.id) \
         FROM project p ORDER BY p.worktree",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, i64>(8)?,
            r.get::<_, i64>(9)?,
            r.get::<_, f64>(10)?,
            r.get::<_, i64>(11)?,
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (
            id,
            worktree,
            name,
            sessions,
            msgs,
            msg_bytes,
            parts,
            part_bytes,
            events,
            event_bytes,
            cost,
            updated,
        ) = r?;
        out.push(ProjectRow {
            id,
            worktree,
            name,
            sessions,
            msgs,
            msg_bytes,
            parts,
            part_bytes,
            events,
            event_bytes,
            cost,
            updated,
        });
    }
    Ok(out)
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
pub fn reasoning_messages_left(con: &Connection, ids: &[String]) -> Result<i64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let sql = format!(
        "SELECT COUNT(*) FROM session_message WHERE session_id IN ({}) \
         AND type = 'assistant' AND data LIKE '%\"type\":\"reasoning\"%'",
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
