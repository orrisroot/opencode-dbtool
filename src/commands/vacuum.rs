//! `vacuum` command: safe VACUUM with backup and journal mode handling.

use crate::db::{file_size, quick_check};
use crate::error::{AppError, Result};
use crate::util::{now_ms, timestamp_utc};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Options for the vacuum command.
pub struct VacuumOpts {
    /// Create and verify a timestamped backup before VACUUM.
    pub backup: bool,
}

impl Default for VacuumOpts {
    fn default() -> Self {
        VacuumOpts { backup: true }
    }
}

/// Parse `vacuum` flags: `--no-backup`.
pub fn parse_vacuum_args(args: &[String]) -> Result<VacuumOpts> {
    let mut opts = VacuumOpts::default();
    for a in args {
        match a.as_str() {
            "--no-backup" => opts.backup = false,
            other => return Err(AppError::usage(format!("unknown option: {other}"))),
        }
    }
    Ok(opts)
}

/// The backup path a run would use right now (for dry-run output).
pub fn planned_backup_path(db_path: &Path) -> PathBuf {
    backup_path(db_path, &timestamp_utc(now_ms().unwrap_or(0)))
}

/// Run the safe VACUUM sequence: checkpoint, integrity check, backup
/// (with verification), VACUUM, journal mode restore, checkpoint, and
/// a final integrity check. Returns the post-state plus the backup
/// report.
pub fn cmd_vacuum(
    con: &Connection,
    db_path: &Path,
    opts: &VacuumOpts,
) -> Result<serde_json::Value> {
    checkpoint(con);
    if quick_check(con) != "ok" {
        return Err(AppError::db("integrity check not ok - abort"));
    }

    let backup = if opts.backup {
        Some(create_backup(db_path)?)
    } else {
        None
    };

    con.execute_batch("VACUUM;")
        .map_err(|e| AppError::db(format!("VACUUM failed: {e}")))?;
    // VACUUM rebuilds the database header; restore the WAL journal mode.
    con.execute_batch("PRAGMA journal_mode = WAL;")
        .map_err(|e| AppError::db(format!("journal_mode failed: {e}")))?;
    checkpoint(con);

    let freelist: i64 = con
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .unwrap_or(-1);
    let integrity = quick_check(con);
    if integrity != "ok" {
        return Err(AppError::db(format!(
            "integrity check failed after vacuum: {integrity}"
        )));
    }
    let mut out = serde_json::json!({
        "db_bytes": file_size(db_path),
        "wal_bytes": file_size(&db_path.with_extension("db-wal")),
        "free_pages": freelist,
        "integrity": integrity,
    });
    out["backup"] = match &backup {
        Some(b) => serde_json::json!({
            "path": b.path.to_string_lossy().to_string(),
            "bytes": b.bytes,
            "integrity": b.integrity,
        }),
        None => serde_json::Value::Null,
    };
    Ok(out)
}

fn checkpoint(con: &Connection) {
    let _ = con.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
}

fn backup_path(db_path: &Path, stamp: &str) -> PathBuf {
    let mut name = db_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    name.push_str(".backup-");
    name.push_str(stamp);
    db_path.with_file_name(name)
}

struct BackupInfo {
    path: PathBuf,
    bytes: u64,
    integrity: String,
}

/// Copy the (already checkpointed) database file to a timestamped
/// backup in the same directory and verify it with quick_check. Any
/// failure aborts the vacuum.
fn create_backup(db_path: &Path) -> Result<BackupInfo> {
    let path = backup_path(db_path, &timestamp_utc(now_ms()?));
    if path.exists() {
        return Err(AppError::db(format!(
            "backup already exists: {}",
            path.display()
        )));
    }
    // The caller checkpointed the WAL first, so a plain file copy of
    // the main database is a consistent snapshot.
    std::fs::copy(db_path, &path)
        .map_err(|e| AppError::db(format!("backup failed ({}): {e}", path.display())))?;
    let check = {
        let backup_con = crate::db::open_conn(&path, true)?;
        quick_check(&backup_con)
    };
    if check != "ok" {
        return Err(AppError::db(format!(
            "backup integrity check failed ({check}) - abort"
        )));
    }
    let bytes = file_size(&path);
    Ok(BackupInfo {
        path,
        bytes,
        integrity: check,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb;
    use std::path::PathBuf;

    fn temp_db(stem: &str) -> (Connection, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "opencode-dbtool-vacuum-{}-{}",
            std::process::id(),
            stem
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{stem}.db"));
        let con = testdb::create_at(&path);
        // VACUUM needs a write connection, so re-open read-write.
        (con, path)
    }

    fn cleanup(path: &Path) {
        let dir = path.parent().unwrap();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
        let _ = std::fs::remove_dir(dir);
    }

    #[test]
    fn parse_vacuum_args_ok() {
        assert!(parse_vacuum_args(&[]).unwrap().backup);
        assert!(!parse_vacuum_args(&["--no-backup".into()]).unwrap().backup);
    }

    #[test]
    fn parse_vacuum_args_rejects_unknown() {
        assert!(parse_vacuum_args(&["--nope".into()]).is_err());
        assert!(parse_vacuum_args(&["--no-auto-vacuum".into()]).is_err());
        assert!(parse_vacuum_args(&["--backup".into()]).is_err());
    }

    #[test]
    fn vacuum_creates_verified_backup_and_preserves_data() {
        let (con, path) = temp_db("backup");
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_part(&con, "s1", r#"{"type":"text","text":"hello"}"#);

        let out = cmd_vacuum(&con, &path, &VacuumOpts::default()).unwrap();

        assert_eq!(out["integrity"], "ok");
        assert!(out["backup"]["path"].is_string());
        let backup_path = PathBuf::from(out["backup"]["path"].as_str().unwrap());
        assert!(backup_path.exists());
        assert_eq!(out["backup"]["integrity"], "ok");

        // The backup is a complete database containing the session.
        let backup_con = crate::db::open_conn(&backup_path, true).unwrap();
        let n: i64 = backup_con
            .query_row("SELECT COUNT(*) FROM session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        drop(backup_con);

        // The main database still has the data.
        let n: i64 = con
            .query_row("SELECT COUNT(*) FROM session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let jm: String = con
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(jm, "wal");

        std::fs::remove_file(&backup_path).unwrap();
        std::fs::remove_file(&path).unwrap();
        cleanup(&path);
    }

    #[test]
    fn vacuum_no_backup_creates_no_backup_file() {
        let (con, path) = temp_db("nobackup");
        testdb::insert_session(&con, "s1", "/a", None);

        let out = cmd_vacuum(&con, &path, &VacuumOpts { backup: false }).unwrap();

        assert!(out["backup"].is_null());
        let dir = path.parent().unwrap();
        let backups: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".backup-"))
            .collect();
        assert!(backups.is_empty(), "no backup file expected");

        cleanup(&path);
    }

    #[test]
    fn planned_backup_path_has_z_timestamp() {
        let p = planned_backup_path(Path::new("/tmp/opencode.db"));
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("opencode.db.backup-"), "got: {name}");
        assert!(name.ends_with('Z'), "got: {name}");
        assert!(!name.contains(':'), "got: {name}");
    }
}
