//! Database connection handling and generic DB helpers.

use crate::error::{AppError, Result};
use crate::sys;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::path::{Path, PathBuf};
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

/// Session table. V2-only: `session_v2`.
pub const SESSION_TABLE: &str = "session_v2";

/// Verify the database carries the opencode 2.x schema. V1 databases
/// (`session`/`message`/`part`/...) are not supported. Older or corrupt
/// databases fail with a descriptive error (exit 3).
pub fn ensure_schema(con: &Connection) -> Result<()> {
    // Core tables every command needs. Cache/account tables (`kv`,
    // `account`, ...) are intentionally not required, and newer content
    // tables (`session_part`, `blob`, `automation`, `job`, `machine`,
    // `session_search*`) are optional so a database opened by an older
    // 2.x build still works.
    for t in [
        "project",
        "session_v2",
        "session_message",
        "session_inbox",
        "instruction_entry",
        "instruction_state",
        "event",
        "event_sequence",
        "worktree",
        "workspace",
        "permission",
    ] {
        let n: i64 = con
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                params![t],
                |r| r.get(0),
            )
            .map_err(|e| AppError::db(e.to_string()))?;
        if n == 0 {
            return Err(AppError::db(format!(
                "database schema is not supported (missing table: {t}) - \
                 open the database once with opencode 2.x so its migrations run, then retry"
            )));
        }
    }
    Ok(())
}

fn sqlite_object_exists(con: &Connection, kind: &str, name: &str) -> Result<bool> {
    let n: i64 = con
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type=?1 AND name=?2",
            params![kind, name],
            |r| r.get(0),
        )
        .map_err(|e| AppError::db(e.to_string()))?;
    Ok(n > 0)
}

/// Whether a table exists in this database. Optional tables added by newer
/// opencode releases are detected this way so the tool keeps working when
/// the database has not run the newest migrations yet.
pub fn table_exists(con: &Connection, table: &str) -> Result<bool> {
    sqlite_object_exists(con, "table", table)
}

/// Whether a trigger exists in this database.
pub fn trigger_exists(con: &Connection, trigger: &str) -> Result<bool> {
    sqlite_object_exists(con, "trigger", trigger)
}

/// Directory holding `db_path` (falls back to `.` for bare filenames).
pub fn db_dir(db_path: &Path) -> &Path {
    db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Path of a SQLite sidecar file next to `path` (`<path>-wal`, ...).
pub fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

/// Path of the WAL file next to `db_path` (`<db_path>-wal`).
pub fn wal_path(db_path: &Path) -> PathBuf {
    sidecar_path(db_path, "-wal")
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

/// Run a multi-row `PRAGMA` check (such as `quick_check` or
/// `integrity_check`), returning `"ok"` when healthy or all diagnostic
/// lines joined by `"; "` (or error text on failure).
pub fn pragma_check(con: &Connection, pragma: &str) -> String {
    let sql = format!("PRAGMA {pragma}");
    let Ok(mut stmt) = con.prepare(&sql) else {
        return format!("{pragma} failed to prepare");
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else {
        return format!("{pragma} query failed");
    };
    let mut messages: Vec<String> = Vec::new();
    for row in rows {
        match row {
            Ok(msg) => messages.push(msg),
            Err(e) => return e.to_string(),
        }
    }
    if messages.len() == 1 && messages[0] == "ok" {
        "ok".to_string()
    } else if messages.is_empty() {
        format!("{pragma} returned no rows")
    } else {
        messages.join("; ")
    }
}

/// Run `PRAGMA quick_check`; returns `"ok"` or diagnostic error text.
pub fn quick_check(con: &Connection) -> String {
    pragma_check(con, "quick_check")
}

/// File size on disk; 0 when the file is missing or unreadable.
pub fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_table_is_v2() {
        assert_eq!(SESSION_TABLE, "session_v2");
        let con = crate::testdb::create();
        ensure_schema(&con).unwrap();
    }

    #[test]
    fn ensure_schema_rejects_v1_db() {
        // V1 layout (legacy `session` table) is no longer supported.
        let con = rusqlite::Connection::open_in_memory().unwrap();
        con.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY); CREATE TABLE project (id TEXT PRIMARY KEY);",
        )
        .unwrap();
        let err = ensure_schema(&con).unwrap_err();
        assert_eq!(err.code, 3);
        assert!(err.message.contains("session_v2"));
    }

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

    #[test]
    fn wal_path_and_db_dir_handle_bare_and_custom_extensions() {
        assert_eq!(db_dir(Path::new("opencode.db")), Path::new("."));
        assert_eq!(db_dir(Path::new("/tmp/a.sqlite")), Path::new("/tmp"));
        assert_eq!(
            wal_path(Path::new("opencode.db")),
            PathBuf::from("opencode.db-wal")
        );
        assert_eq!(
            wal_path(Path::new("/tmp/custom.sqlite")),
            PathBuf::from("/tmp/custom.sqlite-wal")
        );
        assert_eq!(
            wal_path(Path::new("/tmp/mydb")),
            PathBuf::from("/tmp/mydb-wal")
        );
    }
}
