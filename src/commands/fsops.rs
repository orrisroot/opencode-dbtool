//! `fs` subcommands: filesystem storage cleanup.
//!
//! - `clean-orphans` deletes session_diff files whose session no longer
//!   exists in the database.
//! - `clean-snapshots` deletes the git snapshot storage (undo/redo
//!   history); it has no database relationship.

use crate::db::db_status;
use crate::error::{AppError, Result};
use crate::output::print_json;
use crate::util::{dir_size, expect_no_args, session_diff_dir, snapshot_dir};
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::Path;

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

    let mut orphans: Vec<serde_json::Value> = Vec::new();
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
            orphans.push(serde_json::json!({ "file": sid, "bytes": bytes }));
        }
    }

    let mut out = db_status(db_path);
    out["dry_run"] = serde_json::json!(dry_run);
    out["dir"] = serde_json::json!(dir.to_string_lossy().to_string());
    out["orphans"] = serde_json::json!(orphans);
    out["total_files"] = serde_json::json!(orphans.len());
    out["total_bytes"] = serde_json::json!(total_bytes);
    out["deleted"] = serde_json::json!(false);
    if dry_run {
        return print_json(&out);
    }

    for o in &orphans {
        let file = o["file"].as_str().unwrap();
        std::fs::remove_file(dir.join(format!("{file}.json")))
            .map_err(|e| AppError::db(format!("cannot remove {file}.json: {e}")))?;
    }
    out["deleted"] = serde_json::json!(true);
    print_json(&out)
}

/// Delete all snapshot storage (undo/redo history). Guarded while
/// opencode runs because snapshots are in use during revert operations.
pub fn cmd_fs_clean_snapshots(args: &[String], dry_run: bool, db_path: &Path) -> Result<()> {
    expect_no_args(args, "fs clean-snapshots")?;
    let dir = snapshot_dir(db_path);

    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut total_bytes: u64 = 0;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if !e.file_type().is_ok_and(|ft| ft.is_dir()) {
                continue;
            }
            let bytes = dir_size(&p);
            total_bytes += bytes;
            entries.push(serde_json::json!({
                "name": e.file_name().to_string_lossy().to_string(),
                "bytes": bytes,
            }));
        }
    }

    let mut out = db_status(db_path);
    out["dry_run"] = serde_json::json!(dry_run);
    out["dir"] = serde_json::json!(dir.to_string_lossy().to_string());
    out["entries"] = serde_json::json!(entries);
    out["total_bytes"] = serde_json::json!(total_bytes);
    out["deleted"] = serde_json::json!(false);
    if dry_run {
        return print_json(&out);
    }

    for e in &entries {
        let name = e["name"].as_str().unwrap();
        let p = dir.join(name);
        std::fs::remove_dir_all(&p)
            .map_err(|err| AppError::db(format!("cannot remove snapshot {name}: {err}")))?;
    }
    out["deleted"] = serde_json::json!(true);
    print_json(&out)
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
    fn rejects_positional_args() {
        assert!(expect_no_args(&["x".into()], "fs clean-orphans").is_err());
        assert!(expect_no_args(&[], "fs clean-orphans").is_ok());
    }
}
