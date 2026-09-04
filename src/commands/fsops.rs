//! `fs` subcommands: filesystem storage cleanup.
//!
//! - `clean-orphans` deletes session_diff files whose session no longer
//!   exists in the database.
//! - `clean-snapshots` deletes the git snapshot storage (undo/redo
//!   history); it has no database relationship.
//! - `clean-tool-output` deletes all truncated tool-output files
//!   (`tool_*`).
//! - `clean-log` truncates the append-only `log/opencode.log`.

use crate::db::{env_status, EnvStatus};
use crate::error::{AppError, Result};
use crate::output::print_json;
use crate::util::{
    dir_size, expect_no_args, log_file, session_diff_dir, snapshot_dir, tool_output_dir,
};
use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;

/// One file entry in `fs clean-orphans` / `fs clean-tool-output`.
#[derive(Serialize)]
struct FileEntry {
    file: String,
    bytes: u64,
}

/// One entry in `fs clean-snapshots`.
#[derive(Serialize)]
struct SnapshotEntry {
    name: String,
    bytes: u64,
}

#[derive(Serialize)]
struct OrphansOut {
    #[serde(flatten)]
    env: EnvStatus,
    dry_run: bool,
    dir: String,
    orphans: Vec<FileEntry>,
    total_files: usize,
    total_bytes: u64,
    deleted: bool,
}

#[derive(Serialize)]
struct ToolOutputOut {
    #[serde(flatten)]
    env: EnvStatus,
    dry_run: bool,
    dir: String,
    files: Vec<FileEntry>,
    total_files: usize,
    total_bytes: u64,
    deleted: bool,
}

#[derive(Serialize)]
struct SnapshotsOut {
    #[serde(flatten)]
    env: EnvStatus,
    dry_run: bool,
    dir: String,
    entries: Vec<SnapshotEntry>,
    total_bytes: u64,
    deleted: bool,
}

#[derive(Serialize)]
struct LogOut {
    #[serde(flatten)]
    env: EnvStatus,
    dry_run: bool,
    file: String,
    bytes: u64,
    deleted: bool,
}

/// Delete `storage/session_diff` files with no matching session.
/// Safe while opencode runs: opencode only writes diff files for live
/// sessions, so the files removed here are never referenced again.
pub fn cmd_fs_clean_orphans(
    con: &Connection,
    args: &[String],
    dry_run: bool,
    db_path: &Path,
) -> Result<()> {
    expect_no_args(args, "fs clean-orphans")?;
    let dir = session_diff_dir(db_path);

    let mut ids: HashSet<String> = HashSet::new();
    if dir.exists() {
        let mut stmt = con.prepare("SELECT id FROM session")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for r in rows {
            ids.insert(r?);
        }
    }

    let mut orphans: Vec<FileEntry> = Vec::new();
    let mut total_bytes: u64 = 0;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if !e.file_type().is_ok_and(|ft| ft.is_file()) {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let Some(sid) = name.strip_suffix(".json") else {
                continue;
            };
            if ids.contains(sid) {
                continue;
            }
            let bytes = e.metadata().map(|m| m.len()).unwrap_or(0);
            total_bytes += bytes;
            orphans.push(FileEntry {
                file: sid.to_string(),
                bytes,
            });
        }
    }

    let mut out = OrphansOut {
        env: env_status(db_path),
        dry_run,
        dir: dir.to_string_lossy().to_string(),
        orphans,
        total_files: 0,
        total_bytes,
        deleted: true,
    };
    out.total_files = out.orphans.len();
    if dry_run {
        out.deleted = false;
        return print_json(&serde_json::to_value(&out)?);
    }

    for o in &out.orphans {
        std::fs::remove_file(dir.join(format!("{}.json", o.file)))
            .map_err(|e| AppError::db(format!("cannot remove {}.json: {e}", o.file)))?;
    }
    print_json(&serde_json::to_value(&out)?)
}

/// Delete all `tool-output` files (`tool_*`), like every other `clean-*`
/// command. Safe while opencode runs: these files are never read back,
/// only referenced by marker text, and opencode itself deletes them
/// regardless of references after 7 days.
pub fn cmd_fs_clean_tool_output(args: &[String], dry_run: bool, db_path: &Path) -> Result<()> {
    expect_no_args(args, "fs clean-tool-output")?;
    let dir = tool_output_dir(db_path);
    let mut files: Vec<FileEntry> = Vec::new();
    let mut total_bytes: u64 = 0;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            if !e.file_type().is_ok_and(|ft| ft.is_file()) {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("tool_") {
                continue;
            }
            let bytes = e.metadata().map(|m| m.len()).unwrap_or(0);
            total_bytes += bytes;
            files.push(FileEntry { file: name, bytes });
        }
    }

    let mut out = ToolOutputOut {
        env: env_status(db_path),
        dry_run,
        dir: dir.to_string_lossy().to_string(),
        files,
        total_files: 0,
        total_bytes,
        deleted: true,
    };
    out.total_files = out.files.len();
    if dry_run {
        out.deleted = false;
        return print_json(&serde_json::to_value(&out)?);
    }

    for f in &out.files {
        std::fs::remove_file(dir.join(&f.file))
            .map_err(|e| AppError::db(format!("cannot remove {}: {e}", f.file)))?;
    }
    print_json(&serde_json::to_value(&out)?)
}

/// Truncate `log/opencode.log` to zero bytes. Rotation (renaming) would
/// leave the running opencode writing to the old file, so truncation is
/// the only option; it is guarded while opencode runs for the same
/// reason as `clean-snapshots`.
pub fn cmd_fs_clean_log(args: &[String], dry_run: bool, db_path: &Path) -> Result<()> {
    expect_no_args(args, "fs clean-log")?;
    let file = log_file(db_path);

    let mut out = LogOut {
        env: env_status(db_path),
        dry_run,
        file: file.to_string_lossy().to_string(),
        bytes: 0,
        deleted: false,
    };
    if !file.exists() {
        return print_json(&serde_json::to_value(&out)?);
    }
    out.bytes = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
    if dry_run {
        return print_json(&serde_json::to_value(&out)?);
    }

    std::fs::write(&file, [])
        .map_err(|e| AppError::db(format!("cannot truncate {}: {e}", file.display())))?;
    out.deleted = true;
    print_json(&serde_json::to_value(&out)?)
}

/// Delete all snapshot storage (undo/redo history). Guarded while
/// opencode runs because snapshots are in use during revert operations.
pub fn cmd_fs_clean_snapshots(args: &[String], dry_run: bool, db_path: &Path) -> Result<()> {
    expect_no_args(args, "fs clean-snapshots")?;
    let dir = snapshot_dir(db_path);

    let mut entries: Vec<SnapshotEntry> = Vec::new();
    let mut total_bytes: u64 = 0;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if !e.file_type().is_ok_and(|ft| ft.is_dir()) {
                continue;
            }
            let bytes = dir_size(&p);
            total_bytes += bytes;
            entries.push(SnapshotEntry {
                name: e.file_name().to_string_lossy().to_string(),
                bytes,
            });
        }
    }

    let mut out = SnapshotsOut {
        env: env_status(db_path),
        dry_run,
        dir: dir.to_string_lossy().to_string(),
        entries,
        total_bytes,
        deleted: true,
    };
    if dry_run {
        out.deleted = false;
        return print_json(&serde_json::to_value(&out)?);
    }

    for e in &out.entries {
        let p = dir.join(&e.name);
        std::fs::remove_dir_all(&p)
            .map_err(|err| AppError::db(format!("cannot remove snapshot {}: {err}", e.name)))?;
    }
    print_json(&serde_json::to_value(&out)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb;
    use std::fs;
    use std::path::PathBuf;

    fn temp_data_dir(stem: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "opencode-dbtool-fs-{}-{}",
            std::process::id(),
            stem
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn clean_orphans_removes_only_orphans() {
        let dir = temp_data_dir("orphans");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);
        testdb::insert_session(&con, "alive", "/a", None);

        let diff = dir.join("storage/session_diff");
        fs::create_dir_all(&diff).unwrap();
        fs::write(diff.join("alive.json"), vec![0u8; 5]).unwrap();
        fs::write(diff.join("orphan.json"), vec![0u8; 7]).unwrap();
        fs::write(diff.join("other.json"), vec![0u8; 11]).unwrap();

        cmd_fs_clean_orphans(&con, &[], false, &db_path).unwrap();

        assert!(diff.join("alive.json").exists(), "live session file kept");
        assert!(!diff.join("orphan.json").exists(), "orphan removed");
        assert!(!diff.join("other.json").exists(), "orphan removed");
        drop(con);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_orphans_dry_run_changes_nothing() {
        let dir = temp_data_dir("orphans-dry");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);
        testdb::insert_session(&con, "alive", "/a", None);

        let diff = dir.join("storage/session_diff");
        fs::create_dir_all(&diff).unwrap();
        fs::write(diff.join("orphan.json"), vec![0u8; 7]).unwrap();

        cmd_fs_clean_orphans(&con, &[], true, &db_path).unwrap();

        assert!(diff.join("orphan.json").exists());
        drop(con);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_orphans_missing_dir_is_noop() {
        let dir = temp_data_dir("orphans-missing");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);
        testdb::insert_session(&con, "alive", "/a", None);

        cmd_fs_clean_orphans(&con, &[], false, &db_path).unwrap();
        assert!(!dir.join("storage").exists());
        drop(con);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_snapshots_removes_all_entries() {
        let dir = temp_data_dir("snap");
        let db_path = dir.join("opencode.db");
        fs::create_dir_all(dir.join("snapshot/p1/h")).unwrap();
        fs::write(dir.join("snapshot/p1/h/obj"), vec![0u8; 9]).unwrap();
        fs::create_dir_all(dir.join("snapshot/p2/h")).unwrap();
        fs::write(dir.join("snapshot/p2/h/obj"), vec![0u8; 3]).unwrap();

        cmd_fs_clean_snapshots(&[], false, &db_path).unwrap();

        assert!(!dir.join("snapshot/p1").exists());
        assert!(!dir.join("snapshot/p2").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_snapshots_dry_run_changes_nothing() {
        let dir = temp_data_dir("snap-dry");
        let db_path = dir.join("opencode.db");
        fs::create_dir_all(dir.join("snapshot/p1")).unwrap();

        cmd_fs_clean_snapshots(&[], true, &db_path).unwrap();

        assert!(dir.join("snapshot/p1").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_snapshots_missing_dir_is_noop() {
        let dir = temp_data_dir("snap-missing");
        let db_path = dir.join("opencode.db");

        cmd_fs_clean_snapshots(&[], false, &db_path).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_orphans_output_contract() {
        let out = OrphansOut {
            env: crate::db::EnvStatus {
                opencode_running: Some(false),
                pids: Some(vec![]),
                pid_error: None,
                db: "/tmp/x.db".into(),
            },
            dry_run: true,
            dir: "/d".into(),
            orphans: vec![FileEntry {
                file: "ses_orphan".into(),
                bytes: 4,
            }],
            total_files: 1,
            total_bytes: 4,
            deleted: false,
        };
        let v = serde_json::to_value(&out).unwrap();
        let expected = serde_json::json!({
            "opencode_running": false, "pids": [], "db": "/tmp/x.db",
            "dry_run": true, "dir": "/d",
            "orphans": [ { "file": "ses_orphan", "bytes": 4 } ],
            "total_files": 1, "total_bytes": 4, "deleted": false
        });
        assert_eq!(v, expected, "fs clean-orphans JSON contract changed");
    }

    #[test]
    fn rejects_positional_args() {
        assert!(expect_no_args(&["x".into()], "fs clean-orphans").is_err());
        assert!(expect_no_args(&[], "fs clean-orphans").is_ok());
        assert!(expect_no_args(
            &["--older-than".into(), "7d".into()],
            "fs clean-tool-output"
        )
        .is_err());
        assert!(expect_no_args(&[], "fs clean-tool-output").is_ok());
        assert!(expect_no_args(&["x".into()], "fs clean-log").is_err());
    }

    #[test]
    fn clean_tool_output_removes_all_tool_files() {
        let dir = temp_data_dir("toolout");
        let db_path = dir.join("opencode.db");
        let out_dir = dir.join("tool-output");
        fs::create_dir_all(&out_dir).unwrap();
        let old = out_dir.join("tool_old");
        let recent = out_dir.join("tool_recent");
        let other = out_dir.join("keep.txt");
        fs::write(&old, vec![0u8; 5]).unwrap();
        fs::write(&recent, vec![0u8; 3]).unwrap();
        fs::write(&other, vec![0u8; 2]).unwrap();

        cmd_fs_clean_tool_output(&[], false, &db_path).unwrap();

        assert!(!old.exists(), "old tool file removed");
        assert!(!recent.exists(), "recent tool file removed");
        assert!(other.exists(), "non-tool file kept");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_tool_output_dry_run_changes_nothing() {
        let dir = temp_data_dir("toolout-dry");
        let db_path = dir.join("opencode.db");
        let out_dir = dir.join("tool-output");
        fs::create_dir_all(&out_dir).unwrap();
        let old = out_dir.join("tool_x");
        fs::write(&old, vec![0u8; 5]).unwrap();

        cmd_fs_clean_tool_output(&[], true, &db_path).unwrap();

        assert!(old.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_tool_output_missing_dir_is_noop() {
        let dir = temp_data_dir("toolout-missing");
        let db_path = dir.join("opencode.db");

        cmd_fs_clean_tool_output(&[], false, &db_path).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_log_truncates_to_zero() {
        let dir = temp_data_dir("log");
        let db_path = dir.join("opencode.db");
        let log = dir.join("log/opencode.log");
        fs::create_dir_all(dir.join("log")).unwrap();
        fs::write(&log, vec![0u8; 100]).unwrap();

        cmd_fs_clean_log(&[], false, &db_path).unwrap();

        assert!(log.exists(), "file kept, only truncated");
        assert_eq!(fs::metadata(&log).unwrap().len(), 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_log_dry_run_changes_nothing() {
        let dir = temp_data_dir("log-dry");
        let db_path = dir.join("opencode.db");
        let log = dir.join("log/opencode.log");
        fs::create_dir_all(dir.join("log")).unwrap();
        fs::write(&log, vec![0u8; 100]).unwrap();

        cmd_fs_clean_log(&[], true, &db_path).unwrap();

        assert_eq!(fs::metadata(&log).unwrap().len(), 100);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_log_missing_file_is_noop() {
        let dir = temp_data_dir("log-missing");
        let db_path = dir.join("opencode.db");

        cmd_fs_clean_log(&[], false, &db_path).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }
}
