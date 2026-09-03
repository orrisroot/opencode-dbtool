//! Database connection handling and generic DB helpers.

use crate::error::{AppError, Result};
use crate::sys;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::path::Path;
use std::time::Duration;

/// Environment block prepended to single-result command outputs.
#[derive(Serialize)]
pub struct EnvStatus {
    /// `false` when no opencode process is running, `null` when the
    /// running-process detection failed.
    pub opencode_running: Option<bool>,
    /// Pids of running opencode instances; `null` when detection failed.
    pub pids: Option<Vec<i32>>,
    /// Why detection failed (only present when it did).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid_error: Option<String>,
    /// Database path this run operated on.
    pub db: String,
}

/// Compute the environment block (typed; serialization contract tested
/// in this module).
pub fn env_status(db_path: &Path) -> EnvStatus {
    match sys::running_pids() {
        Ok(pids) => EnvStatus {
            opencode_running: Some(!pids.is_empty()),
            pids: Some(pids),
            pid_error: None,
            db: db_path.to_string_lossy().to_string(),
        },
        Err(e) => EnvStatus {
            opencode_running: None,
            pids: None,
            pid_error: Some(e.to_string()),
            db: db_path.to_string_lossy().to_string(),
        },
    }
}

/// Tables the tool depends on (opencode >= 1.18.0 schema).
fn required_tables() -> &'static [&'static str] {
    &[
        "session",
        "project",
        "part",
        "event",
        "event_sequence",
        "session_message",
        "session_input",
        "session_context_epoch",
        "workspace",
        "permission",
        "project_directory",
    ]
}

/// Verify the database carries the opencode schema the tool expects.
/// Older databases (pre 1.18.0) fail with a descriptive error (exit 3).
pub fn ensure_schema(con: &Connection) -> Result<()> {
    let mut missing: Vec<&str> = Vec::new();
    for t in required_tables() {
        let n: i64 = con
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                params![t],
                |r| r.get(0),
            )
            .map_err(|e| AppError::db(e.to_string()))?;
        if n == 0 {
            missing.push(t);
        }
    }
    if !missing.is_empty() {
        return Err(AppError::db(format!(
            "database schema is not supported (missing table(s): {}) - \
             open the database once with opencode >= 1.18.0 so its migrations run, then retry",
            missing.join(", ")
        )));
    }
    Ok(())
}

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
    ensure_schema(&con)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_status_serializes_running_shape() {
        let e = EnvStatus {
            opencode_running: Some(false),
            pids: Some(vec![]),
            pid_error: None,
            db: "/x/opencode.db".into(),
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "opencode_running": false,
                "pids": [],
                "db": "/x/opencode.db"
            })
        );
        assert!(v.get("pid_error").is_none(), "no pid_error on success");
    }

    #[test]
    fn env_status_serializes_error_shape() {
        let e = EnvStatus {
            opencode_running: None,
            pids: None,
            pid_error: Some("detection failed".into()),
            db: "/x/opencode.db".into(),
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "opencode_running": null,
                "pids": null,
                "pid_error": "detection failed",
                "db": "/x/opencode.db"
            })
        );
    }
}
