//! `doctor` command: integrity and consistency checks.

use crate::db::{db_status, file_size, quick_check};
use crate::error::{AppError, Result};
use crate::output::print_json;
use rusqlite::Connection;
use std::path::Path;

pub fn cmd_doctor(con: &Connection, db_path: &Path) -> Result<()> {
    let mut out = db_status(db_path);
    out["db_bytes"] = serde_json::json!(file_size(db_path));
    out["wal_bytes"] = serde_json::json!(file_size(&db_path.with_extension("db-wal")));

    let quick = quick_check(con);
    out["quick_check"] = serde_json::json!(quick);
    let integrity: String = con
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap_or_else(|e| e.to_string());
    out["integrity_check"] = serde_json::json!(integrity);

    let fk_violations: Vec<serde_json::Value> = {
        let mut stmt = con.prepare("PRAGMA foreign_key_check")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        let mut v = Vec::new();
        for r in rows {
            let (table, rowid, ref_table, fk_id) = r?;
            v.push(serde_json::json!({
                "table": table,
                "rowid": rowid,
                "ref_table": ref_table,
                "fk_id": fk_id,
            }));
        }
        v
    };
    out["foreign_key_violations"] = serde_json::json!(fk_violations);

    // References without FK constraints.
    let sessions_missing_parent: Vec<serde_json::Value> = {
        let mut stmt = con.prepare(
            "SELECT s.id, s.parent_id, s.title FROM session s \
             WHERE s.parent_id IS NOT NULL AND s.parent_id != '' \
               AND s.parent_id NOT IN (SELECT id FROM session) ORDER BY s.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut v = Vec::new();
        for r in rows {
            let (id, parent_id, title) = r?;
            v.push(serde_json::json!({
                "id": id,
                "parent_id": parent_id,
                "title": title,
            }));
        }
        v
    };
    let sessions_missing_workspace: Vec<serde_json::Value> = {
        let mut stmt = con.prepare(
            "SELECT s.id, s.workspace_id FROM session s \
             WHERE s.workspace_id IS NOT NULL AND s.workspace_id != '' \
               AND s.workspace_id NOT IN (SELECT id FROM workspace) ORDER BY s.id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut v = Vec::new();
        for r in rows {
            let (id, workspace_id) = r?;
            v.push(serde_json::json!({
                "id": id,
                "workspace_id": workspace_id,
            }));
        }
        v
    };
    let orphaned_event_sequences: Vec<String> = {
        let mut stmt = con.prepare(
            "SELECT DISTINCT aggregate_id FROM event_sequence \
             WHERE aggregate_id NOT IN (SELECT id FROM session) ORDER BY 1",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mismatched_parts: i64 = con
        .query_row(
            "SELECT COUNT(*) FROM part p \
             JOIN message m ON p.message_id = m.id \
             WHERE p.session_id != m.session_id",
            [],
            |r| r.get(0),
        )
        .unwrap_or(-1);
    let orphans = serde_json::json!({
        "sessions_missing_parent": sessions_missing_parent,
        "sessions_missing_workspace": sessions_missing_workspace,
        "orphaned_event_sequences": orphaned_event_sequences,
        "mismatched_parts": mismatched_parts,
    });
    out["orphans"] = orphans;

    let ok = quick == "ok"
        && integrity == "ok"
        && fk_violations.is_empty()
        && sessions_missing_parent.is_empty()
        && sessions_missing_workspace.is_empty()
        && orphaned_event_sequences.is_empty()
        && mismatched_parts == 0;
    out["ok"] = serde_json::json!(ok);
    print_json(&out)?;
    if !ok {
        return Err(AppError::db("integrity problems found (see JSON output)"));
    }
    Ok(())
}
