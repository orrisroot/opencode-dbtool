//! Data-access layer: every SQL statement that reads or writes
//! session/project tables lives here, so commands stay SQL-free
//! and testable against a fake schema.
//!
//! V2-only (opencode 2.x): `session_v2`, `session_message`, `session_part`
//! (newer builds), `session_inbox`, `instruction_*`, `worktree`, plus the
//! newer `blob`, `automation`, `job`, `machine`, and optional
//! `session_search*` tables when present.

use crate::db::{table_exists, trigger_exists, SESSION_TABLE};
use crate::error::{AppError, Result};
use crate::models::{ProjectRow, SessionMeta, SessionRow};
use rusqlite::{params, Connection};
use std::collections::HashMap;

/// Per-session content tables: (table, id column, content columns).
/// `instruction_state` is one row per session at most; its two value
/// columns are summed per row. `session_part` is appended separately when
/// the database has it (newer builds store assistant content there).
const PER_SESSION_TABLES: &[(&str, &str, &[&str])] = &[
    ("event", "aggregate_id", &["data"]),
    ("session_message", "session_id", &["data"]),
    ("session_inbox", "session_id", &["payload"]),
    ("instruction_entry", "session_id", &["value"]),
    (
        "instruction_state",
        "session_id",
        &["initial_values", "current_values"],
    ),
];

const SESSION_PART: (&str, &str, &[&str]) = ("session_part", "session_id", &["data"]);

/// Column list shared by `load_sessions` and `load_session`: session base
/// columns plus per-table count/byte aggregates.
///
/// Order: id, title, directory, parent_id, time_updated, cost, archived,
/// event count/bytes, session_message count/bytes, inbox count/bytes,
/// instruction_entry count/bytes, instruction_state count/bytes,
/// session_part count/bytes, blob bytes (zero when the tables are absent).
fn session_cols(con: &Connection) -> Result<String> {
    let mut cols = vec![
        "s.id".to_string(),
        "COALESCE(s.title,'')".to_string(),
        "s.directory".to_string(),
        "s.parent_id".to_string(),
        "s.time_updated".to_string(),
        "s.cost".to_string(),
        "s.time_archived IS NOT NULL".to_string(),
    ];
    for (table, id_col, contents) in PER_SESSION_TABLES {
        let count = format!("(SELECT COUNT(*) FROM \"{table}\" t WHERE t.\"{id_col}\" = s.id)");
        cols.push(count);
        let per_row: Vec<String> = contents
            .iter()
            .map(|c| format!("COALESCE(length(CAST(t.\"{c}\" AS BLOB)),0)"))
            .collect();
        let sum = per_row.join("+");
        cols.push(format!(
            "(SELECT COALESCE(SUM({sum}),0) FROM \"{table}\" t WHERE t.\"{id_col}\" = s.id)"
        ));
    }
    if table_exists(con, SESSION_PART.0)? {
        let (table, id_col, contents) = SESSION_PART;
        let per_row: Vec<String> = contents
            .iter()
            .map(|c| format!("COALESCE(length(CAST(t.\"{c}\" AS BLOB)),0)"))
            .collect();
        cols.push(format!(
            "(SELECT COUNT(*) FROM \"{table}\" t WHERE t.\"{id_col}\" = s.id)"
        ));
        cols.push(format!(
            "(SELECT COALESCE(SUM({}),0) FROM \"{table}\" t WHERE t.\"{id_col}\" = s.id)",
            per_row.join("+")
        ));
    } else {
        cols.push("0".to_string());
        cols.push("0".to_string());
    }
    if has_blobs(con)? {
        cols.push(
            "(SELECT COALESCE(SUM(b.bytes),0) FROM \
              (SELECT DISTINCT hash FROM blob_ref WHERE session_id = s.id) r \
              JOIN blob b ON b.hash = r.hash)"
                .to_string(),
        );
    } else {
        cols.push("0".to_string());
    }
    Ok(cols.join(", "))
}

/// Whether the newer media blob tables exist.
fn has_blobs(con: &Connection) -> Result<bool> {
    Ok(table_exists(con, "blob")? && table_exists(con, "blob_ref")?)
}

/// Session row columns as read by SQLite.
struct SessionAgg {
    id: String,
    title: String,
    directory: String,
    parent_id: Option<String>,
    updated: i64,
    cost: f64,
    archived: bool,
    events: i64,
    event_bytes: i64,
    sm_msgs: i64,
    sm_bytes: i64,
    inbox_msgs: i64,
    inbox_bytes: i64,
    instr_entry_bytes: i64,
    instr_state_bytes: i64,
    part_msgs: i64,
    part_bytes: i64,
    blob_bytes: i64,
}

fn read_session_row(r: &rusqlite::Row) -> rusqlite::Result<SessionAgg> {
    Ok(SessionAgg {
        id: r.get(0)?,
        title: r.get(1)?,
        directory: r.get(2)?,
        parent_id: r.get(3)?,
        updated: r.get(4)?,
        cost: r.get(5)?,
        archived: r.get(6)?,
        events: r.get(7)?,
        event_bytes: r.get(8)?,
        sm_msgs: r.get(9)?,
        sm_bytes: r.get(10)?,
        inbox_msgs: r.get(11)?,
        inbox_bytes: r.get(12)?,
        instr_entry_bytes: r.get(14)?,
        instr_state_bytes: r.get(16)?,
        part_msgs: r.get(17)?,
        part_bytes: r.get(18)?,
        blob_bytes: r.get(19)?,
    })
}

fn make_session_row(a: SessionAgg) -> SessionRow {
    SessionRow {
        id: a.id,
        title: a.title,
        directory: a.directory,
        parent_id: a.parent_id,
        updated: a.updated,
        archived: a.archived,
        events: a.events,
        event_bytes: a.event_bytes,
        sm_msgs: a.sm_msgs,
        sm_bytes: a.sm_bytes,
        inbox_msgs: a.inbox_msgs,
        inbox_bytes: a.inbox_bytes,
        part_msgs: a.part_msgs,
        part_bytes: a.part_bytes,
        instr_bytes: a.instr_entry_bytes + a.instr_state_bytes,
        blob_bytes: a.blob_bytes,
        cost: a.cost,
    }
}

/// Light session fields for filter selection, ordered by
/// `time_updated` desc (id as tiebreaker), the same order `load_sessions`
/// uses.
pub fn load_session_meta(con: &Connection) -> Result<Vec<SessionMeta>> {
    let sql = format!(
        "SELECT id, COALESCE(title,''), directory, parent_id, time_updated, project_id, \
         time_archived IS NOT NULL, cost FROM \"{SESSION_TABLE}\" \
         ORDER BY time_updated DESC, id"
    );
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok(SessionMeta {
            id: r.get(0)?,
            title: r.get(1)?,
            directory: r.get(2)?,
            parent_id: r.get(3)?,
            updated: r.get(4)?,
            project_id: r.get(5)?,
            archived: r.get(6)?,
            cost: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Own size (all per-session content bytes) for the given ids, using one
/// batched GROUP BY query per table (chunked to stay under SQLite's
/// variable limit). Sessions without rows in a table contribute 0.
pub fn session_sizes(
    con: &Connection,
    ids: &[String],
) -> Result<std::collections::HashMap<String, i64>> {
    let mut map: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    if ids.is_empty() {
        return Ok(map);
    }
    for (table, id_col, contents) in content_tables(con)? {
        let per_row: Vec<String> = contents
            .iter()
            .map(|c| format!("COALESCE(length(CAST(\"{c}\" AS BLOB)),0)"))
            .collect();
        let sum_expr = per_row.join("+");
        for chunk in ids.chunks(crate::util::SQL_VAR_CHUNK) {
            let sql = format!(
                "SELECT \"{id_col}\", COALESCE(SUM({sum_expr}),0) \
                 FROM \"{table}\" WHERE \"{id_col}\" IN ({}) GROUP BY \"{id_col}\"",
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
    if has_blobs(con)? {
        for chunk in ids.chunks(crate::util::SQL_VAR_CHUNK) {
            let sql = format!(
                "SELECT r.session_id, COALESCE(SUM(b.bytes),0) FROM \
                 (SELECT DISTINCT session_id, hash FROM blob_ref WHERE session_id IN ({})) r \
                 JOIN blob b ON b.hash = r.hash GROUP BY r.session_id",
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

/// Per-session content tables present in this database, including
/// `session_part` on newer builds.
fn content_tables(
    con: &Connection,
) -> Result<Vec<(&'static str, &'static str, &'static [&'static str])>> {
    let mut tables = PER_SESSION_TABLES.to_vec();
    if table_exists(con, SESSION_PART.0)? {
        tables.push(SESSION_PART);
    }
    Ok(tables)
}

/// Direct children of a session (recursive subagent sessions).
pub fn child_session_ids(con: &Connection, id: &str) -> Result<Vec<String>> {
    let sql = format!("SELECT id FROM \"{SESSION_TABLE}\" WHERE parent_id = ?1");
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Escape `LIKE` wildcards so a user-supplied reference matches literally.
pub(crate) fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn like_prefix(reference: &str) -> String {
    format!("{}%", escape_like(reference))
}

fn like_contains(reference: &str) -> String {
    format!("%{}%", escape_like(reference))
}

/// Up to six ids whose id matches `pattern` (capped, so ambiguity can be
/// reported without scanning everything).
fn ids_like(con: &Connection, table: &str, pattern: &str) -> Result<Vec<String>> {
    let sql =
        format!("SELECT id FROM \"{table}\" WHERE id LIKE ?1 ESCAPE '\\' ORDER BY id LIMIT 6");
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map(params![pattern], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn format_candidates(ids: &[String]) -> String {
    let shown: Vec<&str> = ids.iter().take(5).map(String::as_str).collect();
    if ids.len() > 5 {
        format!("{}, ...", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

/// Resolve one session reference to its full id:
///
/// 1. exact id;
/// 2. a unique id prefix (the leading `ses_` may be omitted);
/// 3. otherwise an error, with substring suggestions when available.
pub fn resolve_session_id(con: &Connection, reference: &str) -> Result<String> {
    let reference = reference.trim();
    let sql = format!("SELECT id FROM \"{SESSION_TABLE}\" WHERE id = ?1");
    match con.query_row(&sql, params![reference], |r| r.get::<_, String>(0)) {
        Ok(id) => return Ok(id),
        Err(rusqlite::Error::QueryReturnedNoRows) => {}
        Err(e) => return Err(AppError::db(e.to_string())),
    }

    let mut patterns = vec![like_prefix(reference)];
    if !reference.starts_with("ses_") {
        patterns.push(like_prefix(&format!("ses_{reference}")));
    }
    for pattern in patterns {
        let matches = ids_like(con, SESSION_TABLE, &pattern)?;
        match matches.len() {
            0 => {}
            1 => return Ok(matches.into_iter().next().unwrap()),
            _ => {
                return Err(AppError::usage(format!(
                    "session id prefix is ambiguous: {reference} matches multiple sessions: {}",
                    format_candidates(&matches)
                )))
            }
        }
    }

    let suggestions = ids_like(con, SESSION_TABLE, &like_contains(reference))?;
    if suggestions.is_empty() {
        Err(AppError::usage(format!("session not found: {reference}")))
    } else {
        Err(AppError::usage(format!(
            "session not found: {reference} (did you mean: {})",
            format_candidates(&suggestions)
        )))
    }
}

/// Look up session ids by reference (exact id, unique prefix, or
/// `ses_`-optional prefix); errors when any reference is unknown or
/// ambiguous.
pub fn resolve_session_ids(con: &Connection, id_args: &[&str]) -> Result<Vec<String>> {
    id_args
        .iter()
        .map(|id| resolve_session_id(con, id))
        .collect()
}

/// All projects registered at a worktree (trailing slashes ignored).
fn projects_at_worktree(con: &Connection, worktree: &str) -> Result<Vec<ProjectRow>> {
    let sql = "SELECT id, worktree, COALESCE(name,'') FROM project \
               WHERE RTRIM(worktree, '/') = RTRIM(?1, '/') ORDER BY id";
    let mut stmt = con.prepare(sql)?;
    let rows = stmt.query_map(params![worktree], project_row_from_row)?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Resolve one project reference:
///
/// 1. exact id;
/// 2. a unique id prefix;
/// 3. an exact worktree path (must match exactly one project);
/// 4. otherwise an error, with substring suggestions when available.
pub fn resolve_project(con: &Connection, reference: &str) -> Result<ProjectRow> {
    let reference = reference.trim();
    if let Some(p) = lookup_project(con, reference, false)? {
        return Ok(p);
    }

    let matches = ids_like(con, "project", &like_prefix(reference))?;
    match matches.len() {
        1 => {
            let id = &matches[0];
            return lookup_project(con, id, false)?
                .ok_or_else(|| AppError::db(format!("project disappeared during lookup: {id}")));
        }
        n if n > 1 => {
            return Err(AppError::usage(format!(
                "project id prefix is ambiguous: {reference} matches multiple projects: {}",
                format_candidates(&matches)
            )))
        }
        _ => {}
    }

    let by_worktree = projects_at_worktree(con, reference)?;
    match by_worktree.len() {
        1 => return Ok(by_worktree.into_iter().next().unwrap()),
        n if n > 1 => {
            let ids: Vec<String> = by_worktree.iter().map(|p| p.id.clone()).collect();
            return Err(AppError::usage(format!(
                "project reference is ambiguous: {reference} is the worktree of multiple projects: {} - pass a project id",
                format_candidates(&ids)
            )));
        }
        _ => {}
    }

    let suggestions = ids_like(con, "project", &like_contains(reference))?;
    if suggestions.is_empty() {
        Err(AppError::usage(format!("project not found: {reference}")))
    } else {
        Err(AppError::usage(format!(
            "project not found: {reference} (did you mean: {})",
            format_candidates(&suggestions)
        )))
    }
}

/// All sessions with per-session counts and sizes.
pub fn load_sessions(con: &Connection) -> Result<Vec<SessionRow>> {
    let cols = session_cols(con)?;
    let sql =
        format!("SELECT {cols} FROM \"{SESSION_TABLE}\" s ORDER BY s.time_updated DESC, s.id");
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map([], read_session_row)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(make_session_row(r?));
    }
    Ok(out)
}

/// The given sessions, with the same aggregate columns as
/// `load_sessions`, returned in the order of `ids` (unknown ids are
/// skipped).
pub fn load_sessions_by_ids(con: &Connection, ids: &[String]) -> Result<Vec<SessionRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let cols = session_cols(con)?;
    let mut map: std::collections::HashMap<String, SessionRow> = std::collections::HashMap::new();
    for chunk in ids.chunks(crate::util::SQL_VAR_CHUNK) {
        let sql = format!(
            "SELECT {cols} FROM \"{SESSION_TABLE}\" s WHERE s.id IN ({})",
            in_clause(chunk)
        );
        let mut stmt = con.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk), read_session_row)?;
        for row in rows {
            let row = make_session_row(row?);
            map.insert(row.id.clone(), row);
        }
    }
    Ok(ids.iter().filter_map(|id| map.remove(id)).collect())
}

/// `session_message` row counts per session for the given ids.
pub fn message_counts(
    con: &Connection,
    ids: &[String],
) -> Result<std::collections::HashMap<String, i64>> {
    let mut map: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for chunk in ids.chunks(crate::util::SQL_VAR_CHUNK) {
        let sql = format!(
            "SELECT session_id, COUNT(*) FROM session_message \
             WHERE session_id IN ({}) GROUP BY session_id",
            in_clause(chunk)
        );
        let mut stmt = con.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (id, count) = row?;
            map.insert(id, count);
        }
    }
    Ok(map)
}

/// A single session by exact id, with the same aggregate columns as
/// `load_sessions`; `None` when the id is unknown.
pub fn load_session(con: &Connection, id: &str) -> Result<Option<SessionRow>> {
    let cols = session_cols(con)?;
    let sql = format!("SELECT {cols} FROM \"{SESSION_TABLE}\" s WHERE s.id = ?1");
    match con.query_row(&sql, params![id], read_session_row) {
        Ok(a) => Ok(Some(make_session_row(a))),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(AppError::db(e.to_string())),
    }
}

/// All projects with aggregated session stats.
pub fn load_projects(con: &Connection) -> Result<Vec<ProjectRow>> {
    let (part_msgs, part_bytes) = if table_exists(con, SESSION_PART.0)? {
        (
            format!(
                "(SELECT COUNT(*) FROM session_part t JOIN \"{SESSION_TABLE}\" s ON t.session_id = s.id WHERE s.project_id = p.id)"
            ),
            format!(
                "(SELECT COALESCE(SUM(COALESCE(length(CAST(t.data AS BLOB)),0)),0) FROM session_part t JOIN \"{SESSION_TABLE}\" s ON t.session_id = s.id WHERE s.project_id = p.id)"
            ),
        )
    } else {
        ("0".to_string(), "0".to_string())
    };
    let blob_bytes = if has_blobs(con)? {
        format!(
            "(SELECT COALESCE(SUM(b.bytes),0) FROM \
              (SELECT DISTINCT br.hash FROM blob_ref br JOIN \"{SESSION_TABLE}\" s ON br.session_id = s.id WHERE s.project_id = p.id) r \
              JOIN blob b ON b.hash = r.hash)"
        )
    } else {
        "0".to_string()
    };
    let sql = format!(
        "SELECT p.id, p.worktree, COALESCE(p.name,''), \
         (SELECT COUNT(*) FROM \"{SESSION_TABLE}\" s WHERE s.project_id = p.id), \
         (SELECT COUNT(*) FROM session_message t JOIN \"{SESSION_TABLE}\" s ON t.session_id = s.id WHERE s.project_id = p.id), \
         (SELECT COALESCE(SUM(COALESCE(length(CAST(t.data AS BLOB)),0)),0) FROM session_message t JOIN \"{SESSION_TABLE}\" s ON t.session_id = s.id WHERE s.project_id = p.id), \
         (SELECT COUNT(*) FROM event t JOIN \"{SESSION_TABLE}\" s ON t.aggregate_id = s.id WHERE s.project_id = p.id), \
         (SELECT COALESCE(SUM(COALESCE(length(CAST(t.data AS BLOB)),0)),0) FROM event t JOIN \"{SESSION_TABLE}\" s ON t.aggregate_id = s.id WHERE s.project_id = p.id), \
         (SELECT COALESCE(SUM(COALESCE(length(CAST(t.payload AS BLOB)),0)),0) FROM session_inbox t JOIN \"{SESSION_TABLE}\" s ON t.session_id = s.id WHERE s.project_id = p.id), \
         (SELECT COALESCE(SUM(COALESCE(length(CAST(t.value AS BLOB)),0)),0) FROM instruction_entry t JOIN \"{SESSION_TABLE}\" s ON t.session_id = s.id WHERE s.project_id = p.id) \
          + (SELECT COALESCE(SUM(COALESCE(length(CAST(t.initial_values AS BLOB)),0)+COALESCE(length(CAST(t.current_values AS BLOB)),0)),0) FROM instruction_state t JOIN \"{SESSION_TABLE}\" s ON t.session_id = s.id WHERE s.project_id = p.id), \
         {part_msgs}, {part_bytes}, {blob_bytes}, \
         (SELECT COALESCE(SUM(cost),0) FROM \"{SESSION_TABLE}\" s WHERE s.project_id = p.id), \
         (SELECT COALESCE(MAX(time_updated),0) FROM \"{SESSION_TABLE}\" s WHERE s.project_id = p.id) \
         FROM project p ORDER BY p.worktree"
    );
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok(ProjectRow {
            id: r.get(0)?,
            worktree: r.get(1)?,
            name: r.get(2)?,
            sessions: r.get(3)?,
            sm_msgs: r.get(4)?,
            sm_bytes: r.get(5)?,
            events: r.get(6)?,
            event_bytes: r.get(7)?,
            inbox_bytes: r.get(8)?,
            instr_bytes: r.get(9)?,
            part_msgs: r.get(10)?,
            part_bytes: r.get(11)?,
            blob_bytes: r.get(12)?,
            cost: r.get(13)?,
            updated: r.get(14)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// A single project by exact id with the same aggregated stats;
/// `None` when the id is unknown.
pub fn load_project(con: &Connection, id: &str) -> Result<Option<ProjectRow>> {
    for p in load_projects(con)? {
        if p.id == id {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

fn project_row_from_row(r: &rusqlite::Row) -> rusqlite::Result<ProjectRow> {
    Ok(ProjectRow {
        id: r.get(0)?,
        worktree: r.get(1)?,
        name: r.get(2)?,
        sessions: 0,
        sm_msgs: 0,
        sm_bytes: 0,
        events: 0,
        event_bytes: 0,
        inbox_bytes: 0,
        instr_bytes: 0,
        part_msgs: 0,
        part_bytes: 0,
        blob_bytes: 0,
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

/// Durable reasoning event types: current opencode builds use
/// `session.reasoning.*`; older 2.x builds used `session.next.reasoning.*`.
/// Both are stripped so a migrated database does not keep legacy copies.
/// `*.delta` is live-only and never persisted.
const REASONING_EVENT_TYPES: [&str; 4] = [
    "session.reasoning.started",
    "session.reasoning.ended",
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
    let mut out = Vec::new();
    for chunk in ids.chunks(crate::util::SQL_VAR_CHUNK) {
        let sql = format!(
            "SELECT aggregate_id, COUNT(*), COALESCE(SUM(length(CAST(data AS BLOB))),0) \
             FROM event WHERE aggregate_id IN ({}) AND type IN ({}) \
             GROUP BY aggregate_id",
            in_clause(chunk),
            vec!["?"; REASONING_EVENT_TYPES.len()].join(",")
        );
        let mut stmt = con.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(reasoning_event_params(chunk)),
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )?;
        for row in rows {
            out.push(row?);
        }
    }
    Ok(out)
}

/// Delete reasoning events for the given ids; returns rows removed.
pub fn strip_reasoning_events(con: &Connection, ids: &[String]) -> Result<usize> {
    let mut removed = 0;
    for chunk in ids.chunks(crate::util::SQL_VAR_CHUNK) {
        let sql = format!(
            "DELETE FROM event WHERE aggregate_id IN ({}) AND type IN ({})",
            in_clause(chunk),
            vec!["?"; REASONING_EVENT_TYPES.len()].join(",")
        );
        removed += con.execute(
            &sql,
            rusqlite::params_from_iter(reasoning_event_params(chunk)),
        )?;
    }
    Ok(removed)
}

/// Reasoning events remaining for the given ids (post-strip verification).
pub fn reasoning_events_left(con: &Connection, ids: &[String]) -> Result<i64> {
    let mut left = 0;
    for chunk in ids.chunks(crate::util::SQL_VAR_CHUNK) {
        let sql = format!(
            "SELECT COUNT(*) FROM event WHERE aggregate_id IN ({}) AND type IN ({})",
            in_clause(chunk),
            vec!["?"; REASONING_EVENT_TYPES.len()].join(",")
        );
        left += con.query_row(
            &sql,
            rusqlite::params_from_iter(reasoning_event_params(chunk)),
            |r| r.get::<_, i64>(0),
        )?;
    }
    Ok(left)
}

/// Per-session reasoning `session_part` counts and bytes for the given ids.
/// Sessions without reasoning parts are omitted. Empty when the database
/// predates `session_part` (all content is inline on the message there).
pub fn reasoning_part_counts(con: &Connection, ids: &[String]) -> Result<Vec<(String, i64, i64)>> {
    if ids.is_empty() || !table_exists(con, SESSION_PART.0)? {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for chunk in ids.chunks(crate::util::SQL_VAR_CHUNK) {
        let sql = format!(
            "SELECT session_id, COUNT(*), COALESCE(SUM(length(CAST(data AS BLOB))),0) \
             FROM session_part WHERE session_id IN ({}) AND type = 'reasoning' \
             GROUP BY session_id",
            in_clause(chunk)
        );
        let mut stmt = con.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            out.push(row?);
        }
    }
    Ok(out)
}

/// Delete reasoning `session_part` rows for the given sessions; returns
/// rows removed.
pub fn strip_reasoning_parts(con: &Connection, ids: &[String]) -> Result<usize> {
    if ids.is_empty() || !table_exists(con, SESSION_PART.0)? {
        return Ok(0);
    }
    let mut removed = 0;
    for chunk in ids.chunks(crate::util::SQL_VAR_CHUNK) {
        let sql = format!(
            "DELETE FROM session_part WHERE session_id IN ({}) AND type = 'reasoning'",
            in_clause(chunk)
        );
        removed += con.execute(&sql, rusqlite::params_from_iter(chunk))?;
    }
    Ok(removed)
}

/// Reasoning `session_part` rows remaining for the given ids (post-strip
/// verification).
pub fn reasoning_parts_left(con: &Connection, ids: &[String]) -> Result<i64> {
    if ids.is_empty() || !table_exists(con, SESSION_PART.0)? {
        return Ok(0);
    }
    let mut left = 0;
    for chunk in ids.chunks(crate::util::SQL_VAR_CHUNK) {
        let sql = format!(
            "SELECT COUNT(*) FROM session_part WHERE session_id IN ({}) AND type = 'reasoning'",
            in_clause(chunk)
        );
        left += con.query_row(&sql, rusqlite::params_from_iter(chunk), |r| {
            r.get::<_, i64>(0)
        })?;
    }
    Ok(left)
}

/// Assistant `session_message` rows (id, session_id, data) for the
/// given ids, in seq order.
pub fn assistant_messages(
    con: &Connection,
    ids: &[String],
) -> Result<Vec<(String, String, String)>> {
    let mut out = Vec::new();
    for chunk in ids.chunks(crate::util::SQL_VAR_CHUNK) {
        let sql = format!(
            "SELECT id, session_id, data FROM session_message \
             WHERE session_id IN ({}) AND type = 'assistant' ORDER BY session_id, seq",
            in_clause(chunk)
        );
        let mut stmt = con.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            out.push(row?);
        }
    }
    Ok(out)
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
    let mut left = 0;
    for chunk in ids.chunks(crate::util::SQL_VAR_CHUNK) {
        let sql = format!(
            "SELECT COUNT(*) FROM session_message WHERE session_id IN ({}) \
             AND type = 'assistant' \
             AND (EXISTS (SELECT 1 FROM json_each( \
                            CASE WHEN json_valid(data) AND json_type(data, '$.content') = 'array' \
                                 THEN data ELSE '[]' END, '$.content') \
                         WHERE json_extract(value, '$.type') = 'reasoning') \
                  OR (NOT json_valid(data) AND data LIKE '%\"type\":\"reasoning\"%'))",
            in_clause(chunk)
        );
        left += con.query_row(&sql, rusqlite::params_from_iter(chunk), |r| {
            r.get::<_, i64>(0)
        })?;
    }
    Ok(left)
}

/// Which content-addressed blob table an orphan came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobKind {
    /// `instruction_blob`: referenced by `instruction_state` JSON by hash.
    Instruction,
    /// `blob`: media referenced by `blob_ref`.
    Media,
}

impl BlobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BlobKind::Instruction => "instruction",
            BlobKind::Media => "media",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrphanBlob {
    pub hash: String,
    pub bytes: i64,
    pub kind: BlobKind,
}

/// `instruction_blob` rows whose hash is referenced by no
/// `instruction_state` row (the blobs leak when sessions are deleted:
/// states cascade but the content-addressed blobs have no FK).
/// Ordered by hash for stable output.
fn instruction_blob_orphans(con: &Connection) -> Result<Vec<(String, i64)>> {
    let hashes: Vec<String> = {
        let mut stmt = con.prepare("SELECT hash FROM instruction_blob ORDER BY hash")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut out = Vec::new();
    for h in hashes {
        let refs: i64 = con.query_row(
            "SELECT COUNT(*) FROM instruction_state \
             WHERE initial_values LIKE '%'||?1||'%' OR current_values LIKE '%'||?1||'%'",
            params![h],
            |r| r.get(0),
        )?;
        if refs == 0 {
            let bytes: i64 = con.query_row(
                "SELECT COALESCE(length(CAST(value AS BLOB)),0) FROM instruction_blob WHERE hash = ?1",
                params![h],
                |r| r.get(0),
            )?;
            out.push((h, bytes));
        }
    }
    Ok(out)
}

/// `blob` rows referenced by no `blob_ref`. opencode releases these when a
/// session or message is deleted; a direct SQL delete by this tool leaves
/// them behind for this cleanup.
fn media_blob_orphans(con: &Connection) -> Result<Vec<(String, i64)>> {
    if !table_exists(con, "blob")? || !table_exists(con, "blob_ref")? {
        return Ok(Vec::new());
    }
    let mut stmt = con.prepare(
        "SELECT hash, COALESCE(bytes, length(data), 0) FROM blob \
         WHERE hash NOT IN (SELECT hash FROM blob_ref) ORDER BY hash",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Orphans from both content-addressed blob tables (instruction and
/// media), ordered by kind then hash.
pub fn blob_orphans(con: &Connection) -> Result<Vec<OrphanBlob>> {
    let mut out: Vec<OrphanBlob> = instruction_blob_orphans(con)?
        .into_iter()
        .map(|(hash, bytes)| OrphanBlob {
            hash,
            bytes,
            kind: BlobKind::Instruction,
        })
        .collect();
    out.extend(
        media_blob_orphans(con)?
            .into_iter()
            .map(|(hash, bytes)| OrphanBlob {
                hash,
                bytes,
                kind: BlobKind::Media,
            }),
    );
    Ok(out)
}

/// Delete every orphan blob row; returns (rows, bytes).
pub fn delete_blob_orphans(con: &mut Connection) -> Result<(usize, u64)> {
    let orphans = blob_orphans(con)?;
    if orphans.is_empty() {
        return Ok((0, 0));
    }
    con.execute_batch("PRAGMA foreign_keys = ON;")?;
    let tx = con.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut rows = 0;
    let mut bytes = 0u64;
    for orphan in &orphans {
        // Re-check inside the transaction: a concurrent opencode could
        // have stored a reference after the scan.
        let refs: i64 = match orphan.kind {
            BlobKind::Instruction => tx.query_row(
                "SELECT COUNT(*) FROM instruction_state \
                 WHERE initial_values LIKE '%'||?1||'%' OR current_values LIKE '%'||?1||'%'",
                params![orphan.hash],
                |r| r.get(0),
            )?,
            BlobKind::Media => tx.query_row(
                "SELECT COUNT(*) FROM blob_ref WHERE hash = ?1",
                params![orphan.hash],
                |r| r.get(0),
            )?,
        };
        if refs > 0 {
            continue;
        }
        let table = match orphan.kind {
            BlobKind::Instruction => "instruction_blob",
            BlobKind::Media => "blob",
        };
        rows += tx.execute(
            &format!("DELETE FROM \"{table}\" WHERE hash = ?1"),
            params![orphan.hash],
        )?;
        bytes += orphan.bytes as u64;
    }
    tx.commit()?;
    Ok((rows, bytes))
}

/// `job` rows whose session (or child session) no longer exists. The table
/// has no foreign keys, so direct SQL deletes can leave rows behind.
pub fn job_orphans(con: &Connection) -> Result<Vec<String>> {
    if !table_exists(con, "job")? {
        return Ok(Vec::new());
    }
    let mut stmt = con.prepare(&format!(
        "SELECT notification_id FROM job j \
         WHERE NOT EXISTS (SELECT 1 FROM \"{SESSION_TABLE}\" s WHERE s.id = j.session_id) \
            OR (j.child_session_id IS NOT NULL AND j.child_session_id != '' \
                AND NOT EXISTS (SELECT 1 FROM \"{SESSION_TABLE}\" s WHERE s.id = j.child_session_id)) \
         ORDER BY j.notification_id"
    ))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Delete orphan `job` rows; returns rows removed.
pub fn delete_orphan_jobs(con: &mut Connection) -> Result<usize> {
    if !table_exists(con, "job")? {
        return Ok(0);
    }
    con.execute_batch("PRAGMA foreign_keys = ON;")?;
    let tx = con.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let rows = tx.execute(
        &format!(
            "DELETE FROM job \
             WHERE NOT EXISTS (SELECT 1 FROM \"{SESSION_TABLE}\" s WHERE s.id = job.session_id) \
                OR (child_session_id IS NOT NULL AND child_session_id != '' \
                    AND NOT EXISTS (SELECT 1 FROM \"{SESSION_TABLE}\" s WHERE s.id = job.child_session_id))"
        ),
        [],
    )?;
    tx.commit()?;
    Ok(rows)
}

/// `automation` rows whose `project_id` has no matching `project` row.
/// `automation` carries no foreign keys (`session_id` intentionally stays
/// unlinked so deleting a session preserves the automation, whereas a
/// missing `project_id` has no owner).
pub fn automation_orphans(con: &Connection) -> Result<Vec<String>> {
    if !table_exists(con, "automation")? {
        return Ok(Vec::new());
    }
    let mut stmt = con.prepare(
        "SELECT id FROM automation a \
         WHERE NOT EXISTS (SELECT 1 FROM project p WHERE p.id = a.project_id) \
         ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Delete orphan `automation` rows whose `project_id` has no `project`;
/// returns rows removed.
pub fn delete_orphan_automations(con: &mut Connection) -> Result<usize> {
    if !table_exists(con, "automation")? {
        return Ok(0);
    }
    con.execute_batch("PRAGMA foreign_keys = ON;")?;
    let tx = con.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let rows = tx.execute(
        "DELETE FROM automation \
         WHERE NOT EXISTS (SELECT 1 FROM project p WHERE p.id = automation.project_id)",
        [],
    )?;
    tx.commit()?;
    Ok(rows)
}

fn search_doc_orphan_where(alias: &str) -> String {
    format!(
        "(NOT EXISTS (SELECT 1 FROM \"{SESSION_TABLE}\" s WHERE s.id = {alias}.session_id) \
          OR ({alias}.source_id != {alias}.session_id \
              AND NOT EXISTS (SELECT 1 FROM session_message m WHERE m.id = {alias}.source_id)))"
    )
}

/// `session_search_document` `source_id` rows whose `session_id` (or
/// indexed `session_message`) no longer exists.
pub fn search_document_orphans(con: &Connection) -> Result<Vec<String>> {
    if !table_exists(con, "session_search_document")? {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT d.source_id FROM session_search_document d \
         WHERE {} ORDER BY d.id",
        search_doc_orphan_where("d")
    );
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn delete_search_docs_where(
    tx: &rusqlite::Transaction<'_>,
    has_fts: bool,
    has_delete_trigger: bool,
    where_sql: &str,
    params: &[&str],
) -> Result<usize> {
    // Compact layout (`content=''`, `contentless_delete=1`) has no
    // `AFTER DELETE` trigger on `session_search_document`, so remove FTS5
    // entries before deleting the document rows. Text layout (`external
    // content`) maintains `session_search` via `session_search_document_delete`.
    if has_fts && !has_delete_trigger {
        tx.execute(
            &format!(
                "DELETE FROM session_search WHERE rowid IN \
                 (SELECT id FROM session_search_document WHERE {where_sql})"
            ),
            rusqlite::params_from_iter(params.iter().copied()),
        )?;
    }
    let rows = tx.execute(
        &format!("DELETE FROM session_search_document WHERE {where_sql}"),
        rusqlite::params_from_iter(params.iter().copied()),
    )?;
    Ok(rows)
}

/// Delete `session_search_document` rows (and their `session_search` FTS5
/// entries) matching a custom predicate inside an existing transaction.
pub fn delete_search_documents_where(
    tx: &rusqlite::Transaction<'_>,
    has_fts: bool,
    has_delete_trigger: bool,
    where_sql: &str,
    params: &[&str],
) -> Result<usize> {
    delete_search_docs_where(tx, has_fts, has_delete_trigger, where_sql, params)
}

/// Delete orphaned `session_search_document` rows (and their `session_search`
/// FTS5 entries); returns document rows removed.
pub fn delete_orphan_search_documents(con: &mut Connection) -> Result<usize> {
    if !table_exists(con, "session_search_document")? {
        return Ok(0);
    }
    let has_fts = table_exists(con, "session_search")?;
    let has_delete_trigger = trigger_exists(con, "session_search_document_delete")?;
    con.execute_batch("PRAGMA foreign_keys = ON;")?;
    let tx = con.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let rows = delete_search_docs_where(
        &tx,
        has_fts,
        has_delete_trigger,
        &search_doc_orphan_where("session_search_document"),
        &[],
    )?;
    tx.commit()?;
    Ok(rows)
}

/// Per-table counts of session child rows whose `session_id` is missing
/// from `session_v2`. Only tables with > 0 orphaned rows are returned.
pub fn session_row_orphans(con: &Connection) -> Result<Vec<(String, i64)>> {
    let mut out = Vec::new();
    for (table, id_col) in [
        ("session_message", "session_id"),
        ("session_inbox", "session_id"),
        ("instruction_entry", "session_id"),
        ("instruction_state", "session_id"),
        ("blob_ref", "session_id"),
        // Legacy pre-automation table (kept optional for older 2.x DBs).
        ("schedule", "session_id"),
    ] {
        if !table_exists(con, table)? {
            continue;
        }
        let sql = format!(
            "SELECT COUNT(*) FROM \"{table}\" t \
             WHERE t.\"{id_col}\" IS NOT NULL AND t.\"{id_col}\" != '' \
               AND NOT EXISTS (SELECT 1 FROM \"{SESSION_TABLE}\" s WHERE s.id = t.\"{id_col}\")"
        );
        let n: i64 = con.query_row(&sql, [], |r| r.get(0))?;
        if n > 0 {
            out.push((table.to_string(), n));
        }
    }
    if table_exists(con, "session_part")? {
        // session_part references both session_v2 and session_message.
        let n: i64 = con.query_row(
            &format!(
                "SELECT COUNT(*) FROM session_part sp \
                 WHERE (sp.session_id IS NOT NULL AND sp.session_id != '' \
                        AND NOT EXISTS (SELECT 1 FROM \"{SESSION_TABLE}\" s WHERE s.id = sp.session_id)) \
                    OR (sp.message_id IS NOT NULL AND sp.message_id != '' \
                        AND NOT EXISTS (SELECT 1 FROM session_message m WHERE m.id = sp.message_id))"
            ),
            [],
            |r| r.get(0),
        )?;
        if n > 0 {
            out.push(("session_part".to_string(), n));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Delete `event`/`event_sequence` rows whose aggregate is neither a
/// session nor a project; returns (event rows, sequence rows). The event
/// rows are removed first because they reference the sequences.
pub fn delete_orphan_event_sequences(con: &mut Connection) -> Result<(usize, usize)> {
    con.execute_batch("PRAGMA foreign_keys = ON;")?;
    let tx = con.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let events = tx.execute(
        &format!(
            "DELETE FROM event WHERE \
             NOT EXISTS (SELECT 1 FROM \"{SESSION_TABLE}\" s WHERE s.id = event.aggregate_id) \
             AND NOT EXISTS (SELECT 1 FROM project p WHERE p.id = event.aggregate_id)"
        ),
        [],
    )?;
    let sequences = tx.execute(
        &format!(
            "DELETE FROM event_sequence WHERE \
             NOT EXISTS (SELECT 1 FROM \"{SESSION_TABLE}\" s WHERE s.id = event_sequence.aggregate_id) \
             AND NOT EXISTS (SELECT 1 FROM project p WHERE p.id = event_sequence.aggregate_id)"
        ),
        [],
    )?;
    tx.commit()?;
    Ok((events, sequences))
}

/// Number of `event` rows whose aggregate is neither a session nor a
/// project (the rows `delete_orphan_event_sequences` removes).
pub fn orphan_event_row_count(con: &Connection) -> Result<i64> {
    Ok(con.query_row(
        &format!(
            "SELECT COUNT(*) FROM event e WHERE \
             NOT EXISTS (SELECT 1 FROM \"{SESSION_TABLE}\" s WHERE s.id = e.aggregate_id) \
             AND NOT EXISTS (SELECT 1 FROM project p WHERE p.id = e.aggregate_id)"
        ),
        [],
        |r| r.get(0),
    )?)
}

/// Total number of `event_sequence` rows whose aggregate is neither a
/// session nor a project.
pub fn orphan_event_sequence_count(con: &Connection) -> Result<usize> {
    let n: i64 = con.query_row(
        &format!(
            "SELECT COUNT(*) FROM event_sequence es WHERE \
             NOT EXISTS (SELECT 1 FROM \"{SESSION_TABLE}\" s WHERE s.id = es.aggregate_id) \
             AND NOT EXISTS (SELECT 1 FROM project p WHERE p.id = es.aggregate_id)"
        ),
        [],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}

/// Clear `parent_id` of sessions whose parent is missing (they become
/// top-level sessions); returns the number of rows changed.
pub fn clear_missing_parents(con: &Connection) -> Result<usize> {
    Ok(con.execute(
        &format!(
            "UPDATE \"{SESSION_TABLE}\" SET parent_id = NULL \
             WHERE parent_id IS NOT NULL AND parent_id != '' \
             AND parent_id NOT IN (SELECT id FROM \"{SESSION_TABLE}\")"
        ),
        [],
    )?)
}

/// Clear `fork_session_id` of sessions whose fork source is missing.
pub fn clear_dangling_forks(con: &Connection) -> Result<usize> {
    Ok(con.execute(
        &format!(
            "UPDATE \"{SESSION_TABLE}\" SET fork_session_id = NULL \
             WHERE fork_session_id IS NOT NULL AND fork_session_id != '' \
             AND fork_session_id NOT IN (SELECT id FROM \"{SESSION_TABLE}\")"
        ),
        [],
    )?)
}

/// Clear `workspace_id` of sessions whose workspace row is missing.
pub fn clear_missing_workspaces(con: &Connection) -> Result<usize> {
    Ok(con.execute(
        &format!(
            "UPDATE \"{SESSION_TABLE}\" SET workspace_id = NULL \
             WHERE workspace_id IS NOT NULL AND workspace_id != '' \
             AND workspace_id NOT IN (SELECT id FROM workspace)"
        ),
        [],
    )?)
}

/// One `session_part` row stored for an assistant message.
pub struct ListedPart {
    pub bytes: i64,
    pub data: String,
}

/// One `session_message` row for content previews, with its
/// `session_part` rows (newer builds) attached.
pub struct ListedMessage {
    pub id: String,
    pub msg_type: String,
    pub seq: i64,
    pub created: i64,
    /// Stored bytes: the message envelope plus every attached part.
    pub bytes: i64,
    pub data: String,
    pub parts: Vec<ListedPart>,
}

impl ListedMessage {
    /// The message payload with `session_part` rows folded into
    /// `content`, parts first and any legacy inline content after them
    /// (the same order opencode reads them in).
    pub fn merged_data(&self) -> String {
        if self.parts.is_empty() {
            return self.data.clone();
        }
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&self.data) else {
            return self.data.clone();
        };
        let Some(obj) = value.as_object_mut() else {
            return self.data.clone();
        };
        let mut content: Vec<serde_json::Value> = self
            .parts
            .iter()
            .filter_map(|p| serde_json::from_str(&p.data).ok())
            .collect();
        if let Some(inline) = obj.get("content").and_then(|c| c.as_array()) {
            content.extend(inline.iter().cloned());
        }
        obj.insert("content".to_string(), serde_json::Value::Array(content));
        serde_json::to_string(&value).unwrap_or_else(|_| self.data.clone())
    }
}

/// Attach the `session_part` rows belonging to `rows` (one query per
/// chunk). Databases without the table leave the messages untouched.
fn attach_parts(con: &Connection, rows: &mut [ListedMessage]) -> Result<()> {
    if rows.is_empty() || !table_exists(con, SESSION_PART.0)? {
        return Ok(());
    }
    let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
    let mut parts: HashMap<String, Vec<ListedPart>> = HashMap::new();
    for chunk in ids.chunks(crate::util::SQL_VAR_CHUNK) {
        let sql = format!(
            "SELECT message_id, COALESCE(length(CAST(data AS BLOB)),0), data \
             FROM session_part WHERE message_id IN ({}) \
             ORDER BY message_id, ordinal",
            in_clause(chunk)
        );
        let mut stmt = con.prepare(&sql)?;
        let found = stmt.query_map(rusqlite::params_from_iter(chunk), |r| {
            Ok((
                r.get::<_, String>(0)?,
                ListedPart {
                    bytes: r.get(1)?,
                    data: r.get(2)?,
                },
            ))
        })?;
        for row in found {
            let (message_id, part) = row?;
            parts.entry(message_id).or_default().push(part);
        }
    }
    for row in rows {
        if let Some(found) = parts.remove(&row.id) {
            row.bytes += found.iter().map(|p| p.bytes).sum::<i64>();
            row.parts = found;
        }
    }
    Ok(())
}

/// Total `session_message` count plus the oldest `limit` rows (conversation
/// order) for a session.
pub fn list_messages(
    con: &Connection,
    session_id: &str,
    limit: usize,
) -> Result<(i64, Vec<ListedMessage>)> {
    let total: i64 = con.query_row(
        "SELECT COUNT(*) FROM session_message WHERE session_id = ?1",
        params![session_id],
        |r| r.get(0),
    )?;
    let mut stmt = con.prepare(
        "SELECT id, type, seq, time_created, COALESCE(length(CAST(data AS BLOB)),0), data \
         FROM session_message WHERE session_id = ?1 ORDER BY seq, id LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![session_id, limit as i64], |r| {
        Ok(ListedMessage {
            id: r.get(0)?,
            msg_type: r.get(1)?,
            seq: r.get(2)?,
            created: r.get(3)?,
            bytes: r.get(4)?,
            data: r.get(5)?,
            parts: Vec::new(),
        })
    })?;
    let mut rows = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    attach_parts(con, &mut rows)?;
    Ok((total, rows))
}

/// Total `session_message` count plus every row (conversation order).
pub fn list_all_messages(con: &Connection, session_id: &str) -> Result<(i64, Vec<ListedMessage>)> {
    let total: i64 = con.query_row(
        "SELECT COUNT(*) FROM session_message WHERE session_id = ?1",
        params![session_id],
        |r| r.get(0),
    )?;
    let mut stmt = con.prepare(
        "SELECT id, type, seq, time_created, COALESCE(length(CAST(data AS BLOB)),0), data \
         FROM session_message WHERE session_id = ?1 ORDER BY seq, id",
    )?;
    let rows = stmt.query_map(params![session_id], |r| {
        Ok(ListedMessage {
            id: r.get(0)?,
            msg_type: r.get(1)?,
            seq: r.get(2)?,
            created: r.get(3)?,
            bytes: r.get(4)?,
            data: r.get(5)?,
            parts: Vec::new(),
        })
    })?;
    let mut rows = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    attach_parts(con, &mut rows)?;
    Ok((total, rows))
}

/// Total `session_message` count plus the newest `limit` rows (oldest
/// first within the window) for a session.
pub fn list_messages_tail(
    con: &Connection,
    session_id: &str,
    limit: usize,
) -> Result<(i64, Vec<ListedMessage>)> {
    let total: i64 = con.query_row(
        "SELECT COUNT(*) FROM session_message WHERE session_id = ?1",
        params![session_id],
        |r| r.get(0),
    )?;
    let mut stmt = con.prepare(
        "SELECT * FROM (SELECT id, type, seq, time_created, \
         COALESCE(length(CAST(data AS BLOB)),0), data \
         FROM session_message WHERE session_id = ?1 ORDER BY seq DESC, id DESC LIMIT ?2) \
         ORDER BY seq, id",
    )?;
    let rows = stmt.query_map(params![session_id, limit as i64], |r| {
        Ok(ListedMessage {
            id: r.get(0)?,
            msg_type: r.get(1)?,
            seq: r.get(2)?,
            created: r.get(3)?,
            bytes: r.get(4)?,
            data: r.get(5)?,
            parts: Vec::new(),
        })
    })?;
    let mut rows = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    attach_parts(con, &mut rows)?;
    Ok((total, rows))
}

/// One session matching a content search.
pub struct SearchHit {
    pub session_id: String,
    pub matches: i64,
    pub updated: i64,
}

/// One matching message or part row.
pub struct SearchSnippet {
    pub message_id: String,
    pub msg_type: String,
    /// `message` for `session_message`, `part` for `session_part`.
    pub source: &'static str,
    pub created: i64,
    pub data: String,
}

/// Sessions whose message content (including `session_part` rows) matches
/// `pattern`, newest activity first.
pub fn search_hits(
    con: &Connection,
    pattern: &str,
    cutoff: Option<i64>,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let parts = table_exists(con, SESSION_PART.0)?;
    let union = if parts {
        format!(
            "SELECT sm.session_id AS session_id, s.time_updated AS updated \
             FROM session_message sm JOIN \"{SESSION_TABLE}\" s ON s.id = sm.session_id \
             WHERE sm.data LIKE ?1 ESCAPE '\\' AND (?2 IS NULL OR s.time_updated >= ?2) \
             UNION ALL \
             SELECT sp.session_id AS session_id, s.time_updated AS updated \
             FROM session_part sp JOIN \"{SESSION_TABLE}\" s ON s.id = sp.session_id \
             WHERE sp.data LIKE ?1 ESCAPE '\\' AND (?2 IS NULL OR s.time_updated >= ?2)"
        )
    } else {
        format!(
            "SELECT sm.session_id AS session_id, s.time_updated AS updated \
             FROM session_message sm JOIN \"{SESSION_TABLE}\" s ON s.id = sm.session_id \
             WHERE sm.data LIKE ?1 ESCAPE '\\' AND (?2 IS NULL OR s.time_updated >= ?2)"
        )
    };
    let sql = format!(
        "SELECT session_id, COUNT(*), MAX(updated) FROM ({union}) \
         GROUP BY session_id ORDER BY MAX(updated) DESC LIMIT ?3"
    );
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map(params![pattern, cutoff, limit as i64], |r| {
        Ok(SearchHit {
            session_id: r.get(0)?,
            matches: r.get(1)?,
            updated: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Matching rows for one session (message envelopes and parts), newest
/// first, at most `limit` rows.
pub fn search_snippets(
    con: &Connection,
    session_id: &str,
    pattern: &str,
    limit: usize,
) -> Result<Vec<SearchSnippet>> {
    let mut out = Vec::new();
    {
        let mut stmt = con.prepare(
            "SELECT id, type, time_created, data FROM session_message \
             WHERE session_id = ?1 AND data LIKE ?2 ESCAPE '\\' \
             ORDER BY seq, id LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![session_id, pattern, limit as i64], |r| {
            Ok(SearchSnippet {
                message_id: r.get(0)?,
                msg_type: r.get(1)?,
                source: "message",
                created: r.get(2)?,
                data: r.get(3)?,
            })
        })?;
        out.extend(rows.collect::<std::result::Result<Vec<_>, _>>()?);
    }
    if table_exists(con, SESSION_PART.0)? {
        let mut stmt = con.prepare(
            "SELECT message_id, type, time_created, data FROM session_part \
             WHERE session_id = ?1 AND data LIKE ?2 ESCAPE '\\' \
             ORDER BY time_created, message_id, ordinal LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![session_id, pattern, limit as i64], |r| {
            Ok(SearchSnippet {
                message_id: r.get(0)?,
                msg_type: r.get(1)?,
                source: "part",
                created: r.get(2)?,
                data: r.get(3)?,
            })
        })?;
        out.extend(rows.collect::<std::result::Result<Vec<_>, _>>()?);
    }
    out.sort_by(|a, b| {
        a.created
            .cmp(&b.created)
            .then_with(|| a.message_id.cmp(&b.message_id))
            .then_with(|| a.source.cmp(b.source))
    });
    out.truncate(limit);
    Ok(out)
}

/// Per-table row counts for a project delete preview, including the
/// project row itself and every cascade-eligible table. Tables missing
/// from older databases report 0 so the output shape stays stable.
pub fn project_impact(con: &Connection, project_id: &str) -> Result<Vec<(String, i64)>> {
    let session = SESSION_TABLE;
    let mut terms: Vec<String> = Vec::new();
    terms.push(
        "SELECT 'session_v2', (SELECT COUNT(*) FROM \"session_v2\" WHERE project_id = ?1)"
            .to_string(),
    );
    for table in [
        "session_message",
        "session_part",
        "session_inbox",
        "instruction_entry",
        "instruction_state",
        "blob_ref",
        "session_search_document",
    ] {
        if table_exists(con, table)? {
            terms.push(format!(
                "SELECT '{table}', (SELECT COUNT(*) FROM \"{table}\" t JOIN \"{session}\" s ON t.session_id = s.id WHERE s.project_id = ?1)"
            ));
        } else {
            terms.push(format!("SELECT '{table}', 0"));
        }
    }
    if table_exists(con, "schedule")? {
        terms.push(format!(
            "SELECT 'schedule', (SELECT COUNT(*) FROM \"schedule\" t JOIN \"{session}\" s ON t.session_id = s.id WHERE s.project_id = ?1)"
        ));
    }
    for (table, col) in [
        ("permission", "project_id"),
        ("project_directory", "project_id"),
        ("worktree", "project_id"),
        ("automation", "project_id"),
    ] {
        if table_exists(con, table)? {
            terms.push(format!(
                "SELECT '{table}', (SELECT COUNT(*) FROM \"{table}\" WHERE \"{col}\" = ?1)"
            ));
        } else {
            terms.push(format!("SELECT '{table}', 0"));
        }
    }
    for table in ["event", "event_sequence"] {
        if table_exists(con, table)? {
            terms.push(format!(
                "SELECT '{table}', (SELECT COUNT(*) FROM \"{table}\" t \
                 WHERE t.aggregate_id = ?1 \
                    OR t.aggregate_id IN (SELECT id FROM \"{session}\" WHERE project_id = ?1))"
            ));
        } else {
            terms.push(format!("SELECT '{table}', 0"));
        }
    }
    if table_exists(con, "job")? {
        terms.push(format!(
            "SELECT 'job', (SELECT COUNT(*) FROM job t WHERE t.session_id IN (SELECT id FROM \"{session}\" WHERE project_id = ?1) OR t.child_session_id IN (SELECT id FROM \"{session}\" WHERE project_id = ?1))"
        ));
    } else {
        terms.push("SELECT 'job', 0".to_string());
    }
    let sql = terms.join(" UNION ALL ");
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map(params![project_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;
    let counts = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    let mut out = vec![("project".to_string(), 1)];
    out.extend(counts);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb;

    #[test]
    fn load_sessions_include_message_and_inbox_bytes() {
        let con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_session_message(&con, "m1", "s1", "assistant", r#"{"x":1}"#);
        testdb::insert_part(&con, "m1", "s1", 0, "text", "part");
        testdb::insert_inbox(&con, "i1", "s1", "hello-payload");
        testdb::insert_media_blob(&con, "s1", "m1", "media", 6, true);
        con.execute(
            "INSERT INTO instruction_state (session_id, initial_values, current_values) \
             VALUES ('s1', '{\"k\":1}', '{}')",
            [],
        )
        .unwrap();
        let sessions = load_sessions(&con).unwrap();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.sm_msgs, 1);
        assert!(s.sm_bytes > 0);
        assert_eq!(s.inbox_msgs, 1);
        assert_eq!(s.part_msgs, 1);
        assert_eq!(s.part_bytes, 4);
        assert_eq!(s.instr_bytes, 9);
        assert_eq!(s.blob_bytes, 6);
        assert!(s.size_bytes() > 0);
        assert_eq!(
            s.size_bytes(),
            s.sm_bytes
                + s.inbox_bytes
                + s.event_bytes
                + s.part_bytes
                + s.instr_bytes
                + s.blob_bytes,
            "size sums V2 content"
        );
    }

    #[test]
    fn session_sizes_cover_all_content_tables() {
        let con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_session_message(&con, "m1", "s1", "assistant", "12345");
        testdb::insert_inbox(&con, "i1", "s1", "abc");
        testdb::insert_part(&con, "m1", "s1", 0, "text", "xy");
        testdb::insert_media_blob(&con, "s1", "m1", "media", 4, true);
        let sizes = session_sizes(&con, &["s1".to_string()]).unwrap();
        assert_eq!(sizes.get("s1").copied().unwrap_or(0), 14);
    }

    #[test]
    fn project_impact_lists_v2_tables() {
        let con = testdb::create();
        con.execute(
            "INSERT INTO project (id, worktree, name) VALUES ('p1','/a','p1')",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO session_v2 (id, directory, title, project_id, time_updated, cost) \
             VALUES ('s1','/a','t','p1',0,0)",
            [],
        )
        .unwrap();
        let impact = project_impact(&con, "p1").unwrap();
        let names: Vec<&str> = impact.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"session_v2"));
        assert!(names.contains(&"session_message"));
    }

    #[test]
    fn blob_orphans_detects_unreferenced_hashes() {
        let con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        con.execute(
            "INSERT INTO instruction_state (session_id, initial_values, current_values) \
             VALUES ('s1', '{\"k\":\"aaa\"}', '{}')",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO instruction_blob (hash, value) VALUES ('aaa', 'live')",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO instruction_blob (hash, value) VALUES ('zzz', 'orphan!')",
            [],
        )
        .unwrap();

        let orphans = blob_orphans(&con).unwrap();
        assert_eq!(
            orphans,
            vec![OrphanBlob {
                hash: "zzz".to_string(),
                bytes: 7,
                kind: BlobKind::Instruction,
            }]
        );

        con.execute(
            "INSERT INTO blob (hash, mime, bytes, data, time_created) \
             VALUES ('media-orphan', 'image/png', 4, x'00000000', 0)",
            [],
        )
        .unwrap();
        let orphans = blob_orphans(&con).unwrap();
        assert_eq!(orphans.len(), 2);
        assert_eq!(orphans[1].kind, BlobKind::Media);
        assert_eq!(orphans[1].bytes, 4);
    }

    #[test]
    fn parts_merge_before_inline_content() {
        let con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_session_message(
            &con,
            "m1",
            "s1",
            "assistant",
            r#"{"type":"assistant","content":[{"type":"text","text":"inline"}]}"#,
        );
        testdb::insert_part(
            &con,
            "m1",
            "s1",
            0,
            "reasoning",
            r#"{"type":"reasoning","text":"a"}"#,
        );
        testdb::insert_part(&con, "m1", "s1", 1, "text", r#"{"type":"text","text":"b"}"#);
        let (_, rows) = list_messages(&con, "s1", 10).unwrap();
        assert_eq!(rows[0].parts.len(), 2);
        let merged: serde_json::Value =
            serde_json::from_str(&rows[0].merged_data()).expect("valid JSON");
        let content = merged["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["text"], "a");
        assert_eq!(content[1]["text"], "b");
        assert_eq!(content[2]["text"], "inline");
    }

    #[test]
    fn job_orphans_and_gc_follow_session_deletes() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_session(&con, "c1", "/a", Some("s1"));
        testdb::insert_job(&con, "j1", "s1", Some("c1"));
        testdb::insert_job(&con, "j2", "s1", None);
        assert!(job_orphans(&con).unwrap().is_empty());

        con.execute("DELETE FROM session_v2 WHERE id IN ('s1','c1')", [])
            .unwrap();
        assert_eq!(job_orphans(&con).unwrap(), vec!["j1", "j2"]);
        assert_eq!(delete_orphan_jobs(&mut con).unwrap(), 2);
        assert!(job_orphans(&con).unwrap().is_empty());
    }

    #[test]
    fn list_messages_pages_oldest_first() {
        let con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        for (i, id) in ["m1", "m2", "m3"].iter().enumerate() {
            con.execute(
                "INSERT INTO session_message (id, session_id, type, seq, data) VALUES (?1, 's1', 'user', ?2, 'x')",
                rusqlite::params![id, i as i64],
            )
            .unwrap();
        }
        let (total, rows) = list_messages(&con, "s1", 2).unwrap();
        assert_eq!(total, 3);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "m1");
        assert_eq!(rows[1].id, "m2");
    }

    #[test]
    fn session_references_resolve_by_prefix() {
        let con = testdb::create();
        testdb::insert_session(&con, "ses_abc111", "/a", None);
        testdb::insert_session(&con, "ses_abc222", "/a", None);
        testdb::insert_session(&con, "ses_xyz", "/b", None);

        assert_eq!(
            resolve_session_id(&con, "ses_abc111").unwrap(),
            "ses_abc111"
        );
        assert_eq!(resolve_session_id(&con, "ses_abc1").unwrap(), "ses_abc111");
        assert_eq!(
            resolve_session_id(&con, "abc1").unwrap(),
            "ses_abc111",
            "ses_ prefix is optional"
        );
        let err = resolve_session_id(&con, "ses_abc").unwrap_err();
        assert_eq!(err.code, 2);
        assert!(err.message.contains("ambiguous"), "got: {err}");
        let err = resolve_session_id(&con, "ses_nope").unwrap_err();
        assert!(err.message.contains("not found"), "got: {err}");
    }

    #[test]
    fn project_references_resolve_by_prefix_and_worktree() {
        let con = testdb::create();
        testdb::insert_project(&con, "p1", "/a");
        testdb::insert_project(&con, "p2", "/a");
        testdb::insert_project(&con, "p3", "/b");

        assert_eq!(resolve_project(&con, "p3").unwrap().id, "p3");
        assert_eq!(resolve_project(&con, "/b").unwrap().id, "p3");
        assert_eq!(
            resolve_project(&con, "/b/").unwrap().id,
            "p3",
            "trailing slash ignored"
        );
        let err = resolve_project(&con, "/a")
            .err()
            .expect("ambiguous worktree");
        assert!(err.message.contains("multiple projects"), "got: {err}");
        let err = resolve_project(&con, "p").err().expect("ambiguous prefix");
        assert!(err.message.contains("ambiguous"), "got: {err}");
    }

    #[test]
    fn automation_and_search_orphans_are_detected_and_deleted() {
        let mut con = testdb::create();
        testdb::insert_project(&con, "p1", "/a");
        testdb::insert_project_session(&con, "s1", "/a", "p1", 0);
        // Deleting s1 leaves automation pointing at deleted session_id,
        // which opencode intentionally keeps as long as project_id exists.
        con.execute(
            "INSERT INTO automation (id, project_id, session_id, name) VALUES ('a_live', 'p1', 's_gone', 'keep')",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO automation (id, project_id, session_id, name) VALUES ('a_orph', 'p_gone', NULL, 'drop')",
            [],
        )
        .unwrap();
        assert_eq!(automation_orphans(&con).unwrap(), vec!["a_orph"]);
        assert_eq!(delete_orphan_automations(&mut con).unwrap(), 1);
        assert!(automation_orphans(&con).unwrap().is_empty());

        // Compact FTS5 layout (`content='', contentless_delete=1`).
        con.execute_batch(
            "CREATE TABLE session_search_document (id INTEGER PRIMARY KEY, source_id TEXT NOT NULL UNIQUE, session_id TEXT NOT NULL);
             CREATE VIRTUAL TABLE session_search USING fts5(title, summary, user_text, assistant_text, content='', contentless_delete=1);
             INSERT INTO session_search_document (id, source_id, session_id) VALUES (1, 's1', 's1'), (2, 's_gone', 's_gone');
             INSERT INTO session_search (rowid, title, summary, user_text, assistant_text) VALUES (1, 'live', NULL, NULL, NULL), (2, 'dead', NULL, NULL, NULL);",
        )
        .unwrap();
        assert_eq!(search_document_orphans(&con).unwrap(), vec!["s_gone"]);
        assert_eq!(delete_orphan_search_documents(&mut con).unwrap(), 1);
        assert!(search_document_orphans(&con).unwrap().is_empty());
        let fts_rows: i64 = con
            .query_row("SELECT COUNT(*) FROM session_search", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_rows, 1);
    }
}
