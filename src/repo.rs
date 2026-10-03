//! Data-access layer: every SQL statement that reads or writes
//! session/project tables lives here, so commands stay SQL-free
//! and testable against a fake schema.
//!
//! V2-only (opencode 2.x): `session_v2`, `session_message`,
//! `session_inbox`/`session_pending`, `instruction_*`, `worktree`.

use crate::db::SESSION_TABLE;
use crate::error::{AppError, Result};
use crate::models::{ProjectRow, SessionMeta, SessionRow};
use rusqlite::{params, Connection};

/// Per-session content tables: (table, id column, content columns).
/// `instruction_state` is one row per session at most; its two value
/// columns are summed per row.
const PER_SESSION_TABLES: &[(&str, &str, &[&str])] = &[
    ("event", "aggregate_id", &["data"]),
    ("session_message", "session_id", &["data"]),
    ("session_inbox", "session_id", &["payload"]),
    ("session_pending", "session_id", &["data"]),
    ("instruction_entry", "session_id", &["value"]),
    (
        "instruction_state",
        "session_id",
        &["initial_values", "current_values"],
    ),
];

/// Column list shared by `load_sessions` and `load_session`: session base
/// columns plus per-table count/byte aggregates.
///
/// Order: id, title, directory, parent_id, time_updated, cost,
/// event count/bytes, session_message count/bytes, inbox count/bytes,
/// pending count/bytes, instruction_entry bytes, instruction_state bytes.
fn session_cols() -> String {
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
        let count =
            format!("(SELECT COUNT(*) FROM \"{table}\" t WHERE t.\"{id_col}\" = s.id)");
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
    cols.join(", ")
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
    pending_msgs: i64,
    pending_bytes: i64,
    instr_entry_bytes: i64,
    instr_state_bytes: i64,
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
        pending_msgs: r.get(13)?,
        pending_bytes: r.get(14)?,
        instr_entry_bytes: r.get(15)?,
        instr_state_bytes: r.get(16)?,
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
        pending_msgs: a.pending_msgs,
        pending_bytes: a.pending_bytes,
        instr_bytes: a.instr_entry_bytes + a.instr_state_bytes,
        cost: a.cost,
    }
}

/// Light session fields for filter selection, ordered by
/// `time_updated` desc (id as tiebreaker), the same order `load_sessions`
/// uses.
pub fn load_session_meta(con: &Connection) -> Result<Vec<SessionMeta>> {
    let sql = format!(
        "SELECT id, directory, parent_id, time_updated, project_id, \
         time_archived IS NOT NULL FROM \"{SESSION_TABLE}\" \
         ORDER BY time_updated DESC, id"
    );
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok(SessionMeta {
            id: r.get(0)?,
            directory: r.get(1)?,
            parent_id: r.get(2)?,
            updated: r.get(3)?,
            project_id: r.get(4)?,
            archived: r.get(5)?,
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
    for (table, id_col, contents) in PER_SESSION_TABLES {
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
    Ok(map)
}

/// Direct children of a session (recursive subagent sessions).
pub fn child_session_ids(con: &Connection, id: &str) -> Result<Vec<String>> {
    let sql = format!("SELECT id FROM \"{SESSION_TABLE}\" WHERE parent_id = ?1");
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Look up session ids by exact id; errors when any id is unknown.
pub fn resolve_session_ids(con: &Connection, id_args: &[&str]) -> Result<Vec<String>> {
    let mut resolved: Vec<String> = Vec::new();
    for id in id_args {
        let sql = format!("SELECT id FROM \"{SESSION_TABLE}\" WHERE id = ?1");
        let mut stmt = con.prepare(&sql)?;
        let rows: Vec<String> = stmt
            .query_map(params![id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if rows.is_empty() {
            return Err(AppError::usage(format!("session not found: {id}")));
        }
        resolved.push(rows.into_iter().next().unwrap());
    }
    Ok(resolved)
}

/// All sessions with per-session counts and sizes.
pub fn load_sessions(con: &Connection) -> Result<Vec<SessionRow>> {
    let cols = session_cols();
    let sql = format!(
        "SELECT {cols} FROM \"{SESSION_TABLE}\" s ORDER BY s.time_updated DESC, s.id"
    );
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map([], read_session_row)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(make_session_row(r?));
    }
    Ok(out)
}

/// A single session by exact id, with the same aggregate columns as
/// `load_sessions`; `None` when the id is unknown.
pub fn load_session(con: &Connection, id: &str) -> Result<Option<SessionRow>> {
    let cols = session_cols();
    let sql = format!("SELECT {cols} FROM \"{SESSION_TABLE}\" s WHERE s.id = ?1");
    match con.query_row(&sql, params![id], read_session_row) {
        Ok(a) => Ok(Some(make_session_row(a))),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(AppError::db(e.to_string())),
    }
}

/// All projects with aggregated session stats.
pub fn load_projects(con: &Connection) -> Result<Vec<ProjectRow>> {
    let sql = format!(
        "SELECT p.id, p.worktree, COALESCE(p.name,''), \
         (SELECT COUNT(*) FROM \"{SESSION_TABLE}\" s WHERE s.project_id = p.id), \
         (SELECT COUNT(*) FROM session_message t JOIN \"{SESSION_TABLE}\" s ON t.session_id = s.id WHERE s.project_id = p.id), \
         (SELECT COALESCE(SUM(COALESCE(length(CAST(t.data AS BLOB)),0)),0) FROM session_message t JOIN \"{SESSION_TABLE}\" s ON t.session_id = s.id WHERE s.project_id = p.id), \
         (SELECT COUNT(*) FROM event t JOIN \"{SESSION_TABLE}\" s ON t.aggregate_id = s.id WHERE s.project_id = p.id), \
         (SELECT COALESCE(SUM(COALESCE(length(CAST(t.data AS BLOB)),0)),0) FROM event t JOIN \"{SESSION_TABLE}\" s ON t.aggregate_id = s.id WHERE s.project_id = p.id), \
         (SELECT COALESCE(SUM(COALESCE(length(CAST(t.payload AS BLOB)),0)),0) FROM session_inbox t JOIN \"{SESSION_TABLE}\" s ON t.session_id = s.id WHERE s.project_id = p.id), \
         (SELECT COALESCE(SUM(COALESCE(length(CAST(t.data AS BLOB)),0)),0) FROM session_pending t JOIN \"{SESSION_TABLE}\" s ON t.session_id = s.id WHERE s.project_id = p.id), \
         (SELECT COALESCE(SUM(COALESCE(length(CAST(t.value AS BLOB)),0)),0) FROM instruction_entry t JOIN \"{SESSION_TABLE}\" s ON t.session_id = s.id WHERE s.project_id = p.id) \
          + (SELECT COALESCE(SUM(COALESCE(length(CAST(t.initial_values AS BLOB)),0)+COALESCE(length(CAST(t.current_values AS BLOB)),0)),0) FROM instruction_state t JOIN \"{SESSION_TABLE}\" s ON t.session_id = s.id WHERE s.project_id = p.id), \
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
            pending_bytes: r.get(9)?,
            instr_bytes: r.get(10)?,
            cost: r.get(11)?,
            updated: r.get(12)?,
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
        pending_bytes: 0,
        instr_bytes: 0,
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
pub fn reasoning_event_counts(
    con: &Connection,
    ids: &[String],
) -> Result<Vec<(String, i64, i64)>> {
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

/// `instruction_blob` rows whose hash is referenced by no
/// `instruction_state` row (the blobs leak when sessions are deleted:
/// states cascade but the content-addressed blobs have no FK).
/// Ordered by hash for stable output.
pub fn blob_orphans(con: &Connection) -> Result<Vec<(String, i64)>> {
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

/// Delete all orphan `instruction_blob` rows; returns (rows, bytes).
pub fn delete_blob_orphans(con: &mut Connection) -> Result<(usize, u64)> {
    let orphans = blob_orphans(con)?;
    if orphans.is_empty() {
        return Ok((0, 0));
    }
    con.execute_batch("PRAGMA foreign_keys = ON;")?;
    let tx = con.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut rows = 0;
    let mut bytes = 0u64;
    for (hash, b) in &orphans {
        // Re-check inside the transaction: a concurrent opencode could
        // have stored a state referencing this blob after the scan.
        let refs: i64 = tx.query_row(
            "SELECT COUNT(*) FROM instruction_state \
             WHERE initial_values LIKE '%'||?1||'%' OR current_values LIKE '%'||?1||'%'",
            params![hash],
            |r| r.get(0),
        )?;
        if refs > 0 {
            continue;
        }
        rows += tx.execute("DELETE FROM instruction_blob WHERE hash = ?1", params![hash])?;
        bytes += *b as u64;
    }
    tx.commit()?;
    Ok((rows, bytes))
}

/// One `session_message` row for content previews.
pub struct ListedMessage {
    pub id: String,
    pub msg_type: String,
    pub seq: i64,
    pub created: i64,
    pub bytes: i64,
    pub data: String,
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
        })
    })?;
    Ok((
        total,
        rows.collect::<std::result::Result<Vec<_>, _>>()?,
    ))
}

/// Per-table row counts for a project delete preview, including the
/// project row itself and every cascade-eligible table.
pub fn project_impact(con: &Connection, project_id: &str) -> Result<Vec<(String, i64)>> {
    let sql = format!(
        "SELECT 'session_v2', COUNT(*) FROM \"{SESSION_TABLE}\" WHERE project_id = ?1 \
         UNION ALL SELECT 'session_message', (SELECT COUNT(*) FROM session_message t JOIN \"{SESSION_TABLE}\" s ON t.session_id = s.id WHERE s.project_id = ?1) \
         UNION ALL SELECT 'session_inbox', (SELECT COUNT(*) FROM session_inbox t JOIN \"{SESSION_TABLE}\" s ON t.session_id = s.id WHERE s.project_id = ?1) \
         UNION ALL SELECT 'session_pending', (SELECT COUNT(*) FROM session_pending t JOIN \"{SESSION_TABLE}\" s ON t.session_id = s.id WHERE s.project_id = ?1) \
         UNION ALL SELECT 'instruction_entry', (SELECT COUNT(*) FROM instruction_entry t JOIN \"{SESSION_TABLE}\" s ON t.session_id = s.id WHERE s.project_id = ?1) \
         UNION ALL SELECT 'instruction_state', (SELECT COUNT(*) FROM instruction_state t JOIN \"{SESSION_TABLE}\" s ON t.session_id = s.id WHERE s.project_id = ?1) \
         UNION ALL SELECT 'permission', COUNT(*) FROM permission WHERE project_id = ?1 \
         UNION ALL SELECT 'worktree', COUNT(*) FROM worktree WHERE project_id = ?1 \
         UNION ALL SELECT 'event', (SELECT COUNT(*) FROM event e WHERE e.aggregate_id IN (SELECT id FROM \"{SESSION_TABLE}\" WHERE project_id = ?1)) \
         UNION ALL SELECT 'event_sequence', (SELECT COUNT(*) FROM event_sequence es WHERE es.aggregate_id IN (SELECT id FROM \"{SESSION_TABLE}\" WHERE project_id = ?1))"
    );
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
        testdb::insert_inbox(&con, "i1", "s1", "hello-payload");
        let sessions = load_sessions(&con).unwrap();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.sm_msgs, 1);
        assert!(s.sm_bytes > 0);
        assert_eq!(s.inbox_msgs, 1);
        assert!(s.size_bytes() > 0);
        assert_eq!(
            s.size_bytes(),
            s.sm_bytes + s.inbox_bytes + s.event_bytes + s.pending_bytes + s.instr_bytes,
            "size sums V2 content"
        );
    }

    #[test]
    fn session_sizes_cover_all_content_tables() {
        let con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_session_message(&con, "m1", "s1", "assistant", "12345");
        testdb::insert_inbox(&con, "i1", "s1", "abc");
        let sizes = session_sizes(&con, &["s1".to_string()]).unwrap();
        assert_eq!(sizes.get("s1").copied().unwrap_or(0), 8);
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
        con.execute("INSERT INTO instruction_blob (hash, value) VALUES ('aaa', 'live')", [])
            .unwrap();
        con.execute("INSERT INTO instruction_blob (hash, value) VALUES ('zzz', 'orphan!')", [])
            .unwrap();

        let orphans = blob_orphans(&con).unwrap();
        assert_eq!(orphans, vec![("zzz".to_string(), 7)]);
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
}
