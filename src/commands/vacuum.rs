//! `vacuum` command.

use crate::db::{file_size, quick_check};
use crate::error::{AppError, Result};
use rusqlite::Connection;
use std::path::Path;

/// Run VACUUM and report the post-state. Errors if integrity breaks.
pub fn cmd_vacuum(con: &Connection, db_path: &Path) -> Result<serde_json::Value> {
    let _ = con.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    con.execute_batch("VACUUM;")
        .map_err(|e| AppError::db(format!("VACUUM failed: {e}")))?;
    let _ = con.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    let freelist: i64 = con
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .unwrap_or(-1);
    let integrity = quick_check(con);
    if integrity != "ok" {
        return Err(AppError::db(format!(
            "integrity check failed after vacuum: {integrity}"
        )));
    }
    Ok(serde_json::json!({
        "db_bytes": file_size(db_path),
        "wal_bytes": file_size(&db_path.with_extension("db-wal")),
        "free_pages": freelist,
        "integrity": integrity,
    }))
}
