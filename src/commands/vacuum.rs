//! `vacuum` command: safe VACUUM with backup and journal mode handling.

use crate::cli::VacuumArgs;
use crate::db::{file_size, quick_check, EnvStatus};
use crate::error::{AppError, Result};
use crate::output;
use crate::util::{now_ms, timestamp_utc};
use rusqlite::Connection;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// JSON shape of the `backup` field.
#[derive(Serialize)]
pub struct BackupOut {
    pub path: String,
    pub bytes: u64,
    /// Present after a real run (the backup is verified); absent in
    /// dry-run output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,
}

/// JSON shape of the `backup_cleanup` field.
#[derive(Serialize)]
pub struct BackupCleanupOut {
    pub kept: i64,
    pub removed_files: usize,
    pub removed_bytes: u64,
}

/// Result of a real vacuum run (typed; serialization contract covered
/// by `VacuumOut` in `main.rs` and tested end-to-end).
#[derive(Serialize)]
pub struct VacuumResult {
    pub db_bytes: u64,
    pub wal_bytes: u64,
    pub free_pages: i64,
    pub integrity: String,
    pub backup: Option<BackupOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_cleanup: Option<BackupCleanupOut>,
}

/// Options for the vacuum command.
pub struct VacuumOpts {
    /// Create and verify a timestamped backup before VACUUM.
    pub backup: bool,
    /// Keep the newest `<n>` backups after a successful run and delete
    /// older ones (`None` = keep all).
    pub keep_backups: Option<i64>,
}

impl Default for VacuumOpts {
    fn default() -> Self {
        VacuumOpts {
            backup: true,
            keep_backups: None,
        }
    }
}

/// The backup path a run would use right now (for dry-run output).
pub fn planned_backup_path(db_path: &Path) -> PathBuf {
    backup_path(db_path, &timestamp_utc(now_ms().unwrap_or(0)))
}

/// JSON shape of the `vacuum` command output.
#[derive(Serialize)]
struct VacuumCmdOut {
    #[serde(flatten)]
    env: EnvStatus,
    dry_run: bool,
    db_bytes_before: u64,
    free_pages_before: i64,
    backup: Option<BackupOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    db_bytes_after: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wal_bytes_after: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    free_pages_after: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    integrity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup_cleanup: Option<BackupCleanupOut>,
}

/// Run `vacuum` from the CLI args (dry-run reports the plan and stops).
pub fn cmd_vacuum_cli(db_path: &Path, args: &VacuumArgs, dry_run: bool) -> Result<()> {
    let opts = VacuumOpts {
        backup: !args.no_backup,
        keep_backups: args.keep_backups,
    };
    let con = crate::db::open_conn(db_path, dry_run)?;
    output::emit(&vacuum_value(&con, db_path, &opts, dry_run)?)
}

/// Build (and, when not a dry run, execute) the vacuum output value.
/// Exposed so `cleanup` can run the final VACUUM on the same connection.
pub fn vacuum_value(
    con: &Connection,
    db_path: &Path,
    opts: &VacuumOpts,
    dry_run: bool,
) -> Result<serde_json::Value> {
    let integrity = quick_check(con);
    if integrity != "ok" {
        return Err(AppError::db(format!(
            "integrity check not ok ({integrity}) - abort"
        )));
    }
    let freelist: i64 = con.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    let mut out = VacuumCmdOut {
        env: crate::db::env_status(db_path),
        dry_run,
        db_bytes_before: file_size(db_path),
        free_pages_before: freelist,
        backup: if opts.backup {
            Some(BackupOut {
                path: planned_backup_path(db_path).to_string_lossy().to_string(),
                bytes: file_size(db_path),
                integrity: None,
            })
        } else {
            None
        },
        db_bytes_after: None,
        wal_bytes_after: None,
        free_pages_after: None,
        integrity: None,
        backup_cleanup: None,
    };
    if dry_run {
        return Ok(serde_json::to_value(&out)?);
    }
    let after = cmd_vacuum(con, db_path, opts)?;
    out.db_bytes_after = Some(after.db_bytes);
    out.wal_bytes_after = Some(after.wal_bytes);
    out.free_pages_after = Some(after.free_pages);
    out.integrity = Some(after.integrity);
    out.backup = after.backup;
    out.backup_cleanup = after.backup_cleanup;
    Ok(serde_json::to_value(&out)?)
}

/// Run the safe VACUUM sequence: checkpoint, integrity check, backup
/// (with verification), VACUUM, journal mode restore, checkpoint, and
/// a final integrity check. Returns the post-state plus the backup
/// report.
pub fn cmd_vacuum(con: &Connection, db_path: &Path, opts: &VacuumOpts) -> Result<VacuumResult> {
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

    let freelist: i64 = con.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    let integrity = quick_check(con);
    if integrity != "ok" {
        return Err(AppError::db(format!(
            "integrity check failed after vacuum: {integrity}"
        )));
    }
    let backup_out = backup.map(|b| BackupOut {
        path: b.path.to_string_lossy().to_string(),
        bytes: b.bytes,
        integrity: Some(b.integrity),
    });
    let mut result = VacuumResult {
        db_bytes: file_size(db_path),
        wal_bytes: file_size(&db_path.with_extension("db-wal")),
        free_pages: freelist,
        integrity,
        backup: backup_out,
        backup_cleanup: None,
    };
    if let Some(keep) = opts.keep_backups {
        let (removed_files, removed_bytes) = prune_backups(db_path, keep)?;
        result.backup_cleanup = Some(BackupCleanupOut {
            kept: keep,
            removed_files,
            removed_bytes,
        });
    }
    Ok(result)
}

/// JSON shape of the `backup` command output.
#[derive(Serialize)]
pub struct BackupCmdOut {
    #[serde(flatten)]
    pub env: crate::db::EnvStatus,
    pub dry_run: bool,
    pub backup: BackupOut,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_cleanup: Option<BackupCleanupOut>,
}

/// Create a verified timestamped backup without touching table data:
/// checkpoint, integrity check, copy + verify, optional backup pruning.
/// Returns the output value (also used by `cleanup`).
pub fn backup_value(db_path: &Path, dry_run: bool, keep: Option<i64>) -> Result<BackupCmdOut> {
    let con = crate::db::open_conn(db_path, dry_run)?;
    checkpoint(&con);
    if quick_check(&con) != "ok" {
        return Err(AppError::db("integrity check not ok - abort"));
    }
    let mut out = BackupCmdOut {
        env: crate::db::env_status(db_path),
        dry_run,
        backup: BackupOut {
            path: planned_backup_path(db_path).to_string_lossy().to_string(),
            bytes: file_size(db_path),
            integrity: None,
        },
        backup_cleanup: None,
    };
    if dry_run {
        return Ok(out);
    }
    let info = create_backup(db_path)?;
    out.backup = BackupOut {
        path: info.path.to_string_lossy().to_string(),
        bytes: info.bytes,
        integrity: Some(info.integrity),
    };
    if let Some(n) = keep {
        let (removed_files, removed_bytes) = prune_backups(db_path, n)?;
        out.backup_cleanup = Some(BackupCleanupOut {
            kept: n,
            removed_files,
            removed_bytes,
        });
    }
    Ok(out)
}

/// Create a verified timestamped backup without touching table data:
/// checkpoint, integrity check, copy + verify, optional backup pruning.
/// Use it before `purge`/`delete` runs.
pub fn cmd_backup(db_path: &Path, dry_run: bool, keep: Option<i64>) -> Result<()> {
    output::emit(&serde_json::to_value(backup_value(
        db_path, dry_run, keep,
    )?)?)
}

/// Delete all `*.backup-*` files except the newest `keep` (the filename
/// timestamp is zero-padded UTC, so lexicographic order is chronological).
/// Best-effort: files that cannot be removed are skipped.
fn prune_backups(db_path: &Path, keep: i64) -> Result<(usize, u64)> {
    let dir = db_path.parent().unwrap_or(Path::new("."));
    let prefix = format!(
        "{}.backup-",
        db_path.file_name().unwrap_or_default().to_string_lossy()
    );
    let mut backups: Vec<PathBuf> = Vec::new();
    let rd = std::fs::read_dir(dir)
        .map_err(|e| AppError::db(format!("cannot list backups in {}: {e}", dir.display())))?;
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with(&prefix) && e.file_type().is_ok_and(|ft| ft.is_file()) {
            backups.push(p);
        }
    }
    // Newest first: the timestamp suffix sorts lexicographically.
    backups.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    let mut removed_files = 0;
    let mut removed_bytes = 0;
    for p in backups.into_iter().skip(keep.max(0) as usize) {
        if let Ok(meta) = std::fs::metadata(&p) {
            if std::fs::remove_file(&p).is_ok() {
                removed_files += 1;
                removed_bytes += meta.len();
            }
        }
    }
    Ok((removed_files, removed_bytes))
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
    fn prune_backups_keeps_newest() {
        let dir =
            std::env::temp_dir().join(format!("opencode-dbtool-prune-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("opencode.db");
        let mk = |suffix: &str, len: u64| {
            let p = dir.join(format!("opencode.db.backup-{suffix}"));
            std::fs::write(&p, vec![0u8; len as usize]).unwrap();
            p
        };
        let newest = mk("20260102T010203Z", 20);
        let middle = mk("20260101T000000Z", 10);
        let oldest = mk("20251231T235959Z", 30);

        let (removed, bytes) = prune_backups(&db_path, 1).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(bytes, 40);
        assert!(newest.exists(), "newest kept");
        assert!(!middle.exists());
        assert!(!oldest.exists());

        // Other files are never touched.
        std::fs::write(dir.join("other.txt"), vec![0u8; 5]).unwrap();
        let (removed, _) = prune_backups(&db_path, 0).unwrap();
        assert_eq!(removed, 1);
        assert!(dir.join("other.txt").exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn vacuum_creates_verified_backup_and_preserves_data() {
        let (con, path) = temp_db("backup");
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_session_message(&con, "m1", "s1", "assistant", r#"{"type":"text"}"#);

        let out =
            serde_json::to_value(cmd_vacuum(&con, &path, &VacuumOpts::default()).unwrap()).unwrap();

        assert_eq!(out["integrity"], "ok");
        assert!(out["backup"]["path"].is_string());
        let backup_path = PathBuf::from(out["backup"]["path"].as_str().unwrap());
        assert!(backup_path.exists());
        assert_eq!(out["backup"]["integrity"], "ok");

        // The backup is a complete database containing the session.
        let backup_con = crate::db::open_conn(&backup_path, true).unwrap();
        let n: i64 = backup_con
            .query_row("SELECT COUNT(*) FROM session_v2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        drop(backup_con);

        // The main database still has the data.
        let n: i64 = con
            .query_row("SELECT COUNT(*) FROM session_v2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let jm: String = con
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(jm, "wal");

        drop(con);
        std::fs::remove_file(&backup_path).unwrap();
        std::fs::remove_file(&path).unwrap();
        cleanup(&path);
    }

    #[test]
    fn vacuum_no_backup_creates_no_backup_file() {
        let (con, path) = temp_db("nobackup");
        testdb::insert_session(&con, "s1", "/a", None);

        let out = serde_json::to_value(
            cmd_vacuum(
                &con,
                &path,
                &VacuumOpts {
                    backup: false,
                    keep_backups: None,
                },
            )
            .unwrap(),
        )
        .unwrap();

        assert!(out["backup"].is_null());
        let dir = path.parent().unwrap();
        let backups: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".backup-"))
            .collect();
        assert!(backups.is_empty(), "no backup file expected");

        drop(con);
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
