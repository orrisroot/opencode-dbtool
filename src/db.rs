//! Database connection handling and generic DB helpers.

use crate::error::{AppError, Result};
use crate::sys;
use rusqlite::Connection;
use std::path::Path;
use std::time::Duration;

/// Open the database, read-only for passive commands, read-write otherwise.
pub fn open_conn(db_path: &Path, read_only: bool) -> Result<Connection> {
    let flags = if read_only {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let con = Connection::open_with_flags(db_path, flags)
        .map_err(|e| AppError::db(format!("cannot open db: {e}")))?;
    con.busy_timeout(Duration::from_secs(5))
        .map_err(|e| AppError::db(format!("busy_timeout: {e}")))?;
    Ok(con)
}

/// Run `PRAGMA quick_check`; returns "ok" or the first error text.
pub fn quick_check(con: &Connection) -> String {
    con.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
        .unwrap_or_else(|e| e.to_string())
}

/// File size on disk; 0 when the file is missing or unreadable.
pub fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Environment block attached to single-result commands.
pub fn db_status(db_path: &Path) -> serde_json::Value {
    match sys::running_pids() {
        Ok(pids) => serde_json::json!({
            "opencode_running": !pids.is_empty(),
            "pids": pids,
            "db": db_path.to_string_lossy().to_string(),
        }),
        Err(e) => serde_json::json!({
            "opencode_running": serde_json::Value::Null,
            "pids": serde_json::Value::Null,
            "pid_error": e.to_string(),
            "db": db_path.to_string_lossy().to_string(),
        }),
    }
}