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
pub fn load_sessions(con: &Connection) -> Result<Vec<SessionRow>> {
    let mut stmt = con.prepare(
        "SELECT s.id, s.title, s.directory, s.parent_id, s.time_updated, s.cost, \
         (SELECT COUNT(*) FROM message m WHERE m.session_id = s.id), \
         (SELECT COALESCE(SUM(length(CAST(m.data AS BLOB))),0) FROM message m WHERE m.session_id = s.id), \
         (SELECT COUNT(*) FROM part p WHERE p.session_id = s.id), \
         (SELECT COALESCE(SUM(length(CAST(p.data AS BLOB))),0) FROM part p WHERE p.session_id = s.id), \
         (SELECT COUNT(*) FROM event e WHERE e.aggregate_id = s.id), \
         (SELECT COALESCE(SUM(length(CAST(e.data AS BLOB))),0) FROM event e WHERE e.aggregate_id = s.id) \
         FROM session s ORDER BY s.time_updated DESC",
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

/// All projects registered at a worktree (duplicates included).
pub fn lookup_projects_by_worktree(con: &Connection, dir: &str) -> Result<Vec<ProjectRow>> {
    let mut stmt = con.prepare("SELECT id, worktree, COALESCE(name,'') FROM project WHERE worktree = ?1")?;
    let rows = stmt.query_map(params![dir], project_row_from_row)?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
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