//! `fs` subcommands: filesystem storage cleanup (V2-only).
//!
//! - `clean-snapshots` deletes the git snapshot storage (undo/redo
//!   history); it has no database relationship.
//! - `clean-log` truncates the append-only `log/opencode.log`.

use crate::db::{env_status, EnvStatus};
use crate::error::Result;
use crate::output::print_json;
use crate::util::{
    dir_size, file_mtime_ms, log_file, now_ms, parse_age_ms, shell_dir, snapshot_dir,
};
use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;

/// One entry in `fs clean-snapshots`.
#[derive(Serialize)]
struct SnapshotEntry {
    name: String,
    bytes: u64,
}

#[derive(Serialize)]
struct SnapshotsOut {
    #[serde(flatten)]
    env: EnvStatus,
    dry_run: bool,
    dir: String,
    /// `--project` filter as given (empty = all projects).
    projects: Vec<String>,
    orphans_only: bool,
    entries: Vec<SnapshotEntry>,
    total_bytes: u64,
    deleted: bool,
}

/// One shell output file in `fs clean-shell`.
#[derive(Serialize)]
struct ShellEntry {
    file: String,
    bytes: u64,
}

#[derive(Serialize)]
struct ShellOut {
    #[serde(flatten)]
    env: EnvStatus,
    dry_run: bool,
    dir: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    older_than: Option<String>,
    files: Vec<ShellEntry>,
    total_files: usize,
    total_bytes: u64,
    deleted: bool,
}

/// One orphan blob in `fs clean-blob-orphans` / `doctor`.
#[derive(Serialize)]
struct BlobEntry {
    hash: String,
    bytes: i64,
}

#[derive(Serialize)]
struct BlobOrphansOut {
    #[serde(flatten)]
    env: EnvStatus,
    dry_run: bool,
    /// Remaining orphans after a real run (empty on success).
    orphans: Vec<BlobEntry>,
    total_blobs: usize,
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
    /// Bytes remaining after the operation (0 for full truncation).
    #[serde(skip_serializing_if = "Option::is_none")]
    remaining_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    older_than: Option<String>,
    deleted: bool,
}

/// Truncate `log/opencode.log` to zero bytes, or with `--older-than
/// <age>` drop only lines older than the cutoff (lines without a
/// parseable `timestamp=` prefix are kept). Rotation (renaming) would
/// leave the running opencode writing to the old file, so truncation is
/// the only full option; both modes are guarded while opencode runs for
/// the same reason as `clean-snapshots`.
pub fn cmd_fs_clean_log(
    args: &[String],
    dry_run: bool,
    data_dir: &Path,
    db_path: &Path,
) -> Result<()> {
    let mut cutoff: Option<i64> = None;
    let mut older_than_raw: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--older-than" => {
                let age = args.get(i + 1).ok_or_else(|| {
                    crate::error::AppError::usage("--older-than requires an age (e.g. 30d)")
                })?;
                cutoff = Some(
                    crate::util::now_ms()? - crate::util::parse_age_ms(age)?,
                );
                older_than_raw = Some(age.clone());
                i += 2;
            }
            other => {
                return Err(crate::error::AppError::usage(format!(
                    "unknown option: {other} (usage: opencode-dbtool fs clean-log [--older-than <age>])"
                )));
            }
        }
    }
    let file = log_file(data_dir);

    let mut out = LogOut {
        env: env_status(db_path),
        dry_run,
        file: file.to_string_lossy().to_string(),
        bytes: 0,
        remaining_bytes: None,
        older_than: older_than_raw,
        deleted: false,
    };
    if !file.exists() {
        return print_json(&serde_json::to_value(&out)?);
    }
    out.bytes = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
    if dry_run {
        out.remaining_bytes = Some(filtered_log_bytes(&file, cutoff));
        return print_json(&serde_json::to_value(&out)?);
    }

    match cutoff {
        None => {
            std::fs::write(&file, []).map_err(|e| {
                crate::error::AppError::db(format!("cannot truncate {}: {e}", file.display()))
            })?;
            out.remaining_bytes = Some(0);
        }
        Some(cutoff) => {
            let kept = filtered_log(&file, cutoff);
            // Write atomically-ish: same truncation semantics as the full
            // mode (a running opencode would already have been refused by
            // the idle guard, so no writer races us here).
            std::fs::write(&file, kept.as_bytes()).map_err(|e| {
                crate::error::AppError::db(format!("cannot prune {}: {e}", file.display()))
            })?;
            out.remaining_bytes = Some(kept.len() as u64);
        }
    }
    out.deleted = true;
    print_json(&serde_json::to_value(&out)?)
}

/// Bytes the log would have (or now has) after dropping lines older than
/// `cutoff`; with `None`, 0 (full truncation).
fn filtered_log_bytes(file: &Path, cutoff: Option<i64>) -> u64 {
    match cutoff {
        None => 0,
        Some(cutoff) => filtered_log(file, cutoff).len() as u64,
    }
}

/// Log content with lines older than `cutoff` removed. Lines without a
/// parseable timestamp are kept.
///
/// Note: the whole file is buffered in memory (opencode.log is a single
/// append-only file, so pruning inherently rewrites it). Logs are
/// typically a few MB; if yours is orders of magnitude larger, truncate
/// (`fs clean-log` without flags) instead of pruning.
fn filtered_log(file: &Path, cutoff: i64) -> String {
    let content = std::fs::read(file)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let mut kept = String::new();
    for line in content.split_inclusive('\n') {
        match crate::util::parse_log_ts(line) {
            Some(ts) if ts < cutoff => {}
            _ => kept.push_str(line),
        }
    }
    kept
}

/// Delete all snapshot storage (undo/redo history), optionally limited
/// to projects (`--project <id>`, repeatable) and/or to directories whose
/// project no longer exists (`--orphans-only`, e.g. left behind by project
/// deletes). Guarded while opencode runs because snapshots are in use
/// during revert operations.
pub fn cmd_fs_clean_snapshots(
    con: &Connection,
    args: &[String],
    dry_run: bool,
    data_dir: &Path,
    db_path: &Path,
) -> Result<()> {
    let mut projects: Vec<String> = Vec::new();
    let mut orphans_only = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--project" => {
                let p = args.get(i + 1).ok_or_else(|| {
                    crate::error::AppError::usage("--project requires a project id")
                })?;
                projects.push(p.clone());
                i += 2;
            }
            "--orphans-only" => {
                orphans_only = true;
                i += 1;
            }
            other => {
                return Err(crate::error::AppError::usage(format!(
                    "unknown option: {other} (usage: opencode-dbtool fs clean-snapshots [--project <id>...] [--orphans-only])"
                )));
            }
        }
    }
    let dir = snapshot_dir(data_dir);
    let known: HashSet<String> = if orphans_only {
        let mut stmt = con.prepare("SELECT id FROM project")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<HashSet<_>, _>>()?
    } else {
        HashSet::new()
    };

    let mut entries: Vec<SnapshotEntry> = Vec::new();
    let mut total_bytes: u64 = 0;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if !e.file_type().is_ok_and(|ft| ft.is_dir()) {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if !projects.is_empty() && !projects.contains(&name) {
                continue;
            }
            if orphans_only && known.contains(&name) {
                continue;
            }
            let bytes = dir_size(&p);
            total_bytes += bytes;
            entries.push(SnapshotEntry { name, bytes });
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    let mut out = SnapshotsOut {
        env: env_status(db_path),
        dry_run,
        dir: dir.to_string_lossy().to_string(),
        projects,
        orphans_only,
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
            .map_err(|err| crate::error::AppError::db(format!("cannot remove snapshot {}: {err}", e.name)))?;
    }
    print_json(&serde_json::to_value(&out)?)
}

/// Delete shell output files (`shell/<project>/sh_*.out`), oldest first
/// in spirit: `--older-than <age>` keeps recent outputs. Guarded while
/// opencode runs because live runs append to these files.
pub fn cmd_fs_clean_shell(
    args: &[String],
    dry_run: bool,
    data_dir: &Path,
    db_path: &Path,
) -> Result<()> {
    let mut cutoff: Option<i64> = None;
    let mut older_than_raw: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--older-than" => {
                let age = args.get(i + 1).ok_or_else(|| {
                    crate::error::AppError::usage("--older-than requires an age (e.g. 30d)")
                })?;
                cutoff = Some(now_ms()? - parse_age_ms(age)?);
                older_than_raw = Some(age.clone());
                i += 2;
            }
            other => {
                return Err(crate::error::AppError::usage(format!(
                    "unknown option: {other} (usage: opencode-dbtool fs clean-shell [--older-than <age>])"
                )));
            }
        }
    }
    let dir = shell_dir(data_dir);
    let mut files: Vec<ShellEntry> = Vec::new();
    let mut total_bytes: u64 = 0;
    collect_shell_files(&dir, &dir, cutoff, &mut files, &mut total_bytes);
    files.sort_by(|a, b| a.file.cmp(&b.file));

    let mut out = ShellOut {
        env: env_status(db_path),
        dry_run,
        dir: dir.to_string_lossy().to_string(),
        older_than: older_than_raw,
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
    // Fail fast like `clean-snapshots`, but report how far the run got:
    // re-running converges, since already-removed files simply drop out of
    // the next scan.
    let mut removed = 0;
    for f in &out.files {
        if let Err(e) = std::fs::remove_file(dir.join(&f.file)) {
            return Err(crate::error::AppError::db(format!(
                "cannot remove {}: {e} ({removed} of {} file(s) already removed; re-run to converge)",
                f.file,
                out.files.len(),
            )));
        }
        removed += 1;
    }
    remove_empty_dirs(&dir);
    print_json(&serde_json::to_value(&out)?)
}

fn collect_shell_files(
    base: &Path,
    dir: &Path,
    cutoff: Option<i64>,
    files: &mut Vec<ShellEntry>,
    total_bytes: &mut u64,
) {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for e in rd.flatten() {
        let p = e.path();
        if let Ok(ft) = e.file_type() {
            if ft.is_dir() {
                collect_shell_files(base, &p, cutoff, files, total_bytes);
            } else if ft.is_file() {
                if let Some(cutoff) = cutoff {
                    match file_mtime_ms(&p) {
                        Some(mtime) if mtime < cutoff => {}
                        _ => continue, // keep recent or undatable files
                    }
                }
                let bytes = e.metadata().map(|m| m.len()).unwrap_or(0);
                *total_bytes += bytes;
                let rel = p
                    .strip_prefix(base)
                    .map(|r| r.to_string_lossy().to_string())
                    .unwrap_or_else(|_| p.to_string_lossy().to_string());
                files.push(ShellEntry { file: rel, bytes });
            }
        }
    }
}

fn remove_empty_dirs(dir: &Path) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if e.file_type().is_ok_and(|ft| ft.is_dir()) {
                remove_empty_dirs(&p);
                let _ = std::fs::remove_dir(&p); // fails unless empty
            }
        }
    }
}

/// Delete orphan `instruction_blob` rows (hashes referenced by no
/// `instruction_state`). Blobs leak when sessions/projects are deleted;
/// run this after purges. Guarded while opencode runs: a concurrent run
/// could store a state referencing a blob mid-cleanup (re-checked inside
/// the transaction, but the guard keeps it simple and safe).
pub fn cmd_fs_clean_blob_orphans(
    con: &mut Connection,
    args: &[String],
    dry_run: bool,
    db_path: &Path,
) -> Result<()> {
    crate::util::expect_no_args(args, "fs clean-blob-orphans")?;
    let orphans = crate::repo::blob_orphans(con)?;
    let total_bytes: u64 = orphans.iter().map(|(_, b)| *b as u64).sum();
    let mut out = BlobOrphansOut {
        env: env_status(db_path),
        dry_run,
        orphans: orphans
            .iter()
            .map(|(h, b)| BlobEntry {
                hash: h.clone(),
                bytes: *b,
            })
            .collect(),
        total_blobs: orphans.len(),
        total_bytes,
        deleted: false,
    };
    if dry_run {
        return print_json(&serde_json::to_value(&out)?);
    }
    let (rows, bytes) = crate::repo::delete_blob_orphans(con)?;
    out.total_blobs = rows;
    out.total_bytes = bytes;
    // Re-list leftovers (should be empty; a concurrent writer could have
    // added an unreferenced blob between scan and delete, which stays).
    out.orphans = crate::repo::blob_orphans(con)?
        .into_iter()
        .map(|(hash, bytes)| BlobEntry { hash, bytes })
        .collect();
    out.deleted = true;
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
    fn clean_snapshots_removes_all_entries() {
        let dir = temp_data_dir("snap");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);
        fs::create_dir_all(dir.join("snapshot/p1/h")).unwrap();
        fs::write(dir.join("snapshot/p1/h/obj"), vec![0u8; 9]).unwrap();
        fs::create_dir_all(dir.join("snapshot/p2/h")).unwrap();
        fs::write(dir.join("snapshot/p2/h/obj"), vec![0u8; 3]).unwrap();

        cmd_fs_clean_snapshots(&con, &[], false, &dir, &db_path).unwrap();

        assert!(!dir.join("snapshot/p1").exists());
        assert!(!dir.join("snapshot/p2").exists());
        drop(con);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_snapshots_dry_run_changes_nothing() {
        let dir = temp_data_dir("snap-dry");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);
        fs::create_dir_all(dir.join("snapshot/p1")).unwrap();

        cmd_fs_clean_snapshots(&con, &[], true, &dir, &db_path).unwrap();

        assert!(dir.join("snapshot/p1").exists());
        drop(con);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_snapshots_missing_dir_is_noop() {
        let dir = temp_data_dir("snap-missing");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);

        cmd_fs_clean_snapshots(&con, &[], false, &dir, &db_path).unwrap();
        drop(con);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_snapshots_project_filter_and_orphans() {
        let dir = temp_data_dir("snap-filter");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);
        testdb::insert_project(&con, "p1", "/a");
        for p in ["p1", "p2", "p3"] {
            fs::create_dir_all(dir.join(format!("snapshot/{p}"))).unwrap();
            fs::write(dir.join(format!("snapshot/{p}/o")), vec![0u8; 2]).unwrap();
        }

        // --project limits the scope.
        cmd_fs_clean_snapshots(
            &con,
            &["--project".to_string(), "p1".to_string()],
            false,
            &dir,
            &db_path,
        )
        .unwrap();
        assert!(!dir.join("snapshot/p1").exists());
        assert!(dir.join("snapshot/p2").exists());
        assert!(dir.join("snapshot/p3").exists());

        // --orphans-only removes only unknown projects (p2; p3 was recreated
        // as known below... here p2/p3 are both unknown, p1 dir is gone).
        testdb::insert_project(&con, "p2", "/b");
        cmd_fs_clean_snapshots(
            &con,
            &["--orphans-only".to_string()],
            false,
            &dir,
            &db_path,
        )
        .unwrap();
        assert!(dir.join("snapshot/p2").exists(), "known project kept");
        assert!(!dir.join("snapshot/p3").exists(), "orphan removed");
        drop(con);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_shell_removes_old_files_only() {
        use std::fs::File;
        use std::time::{Duration, SystemTime};

        let dir = temp_data_dir("shell");
        let db_path = dir.join("opencode.db");
        let shell = dir.join("shell/p1");
        fs::create_dir_all(&shell).unwrap();
        let old = shell.join("sh_old.out");
        let recent = shell.join("sh_recent.out");
        fs::write(&old, vec![0u8; 5]).unwrap();
        fs::write(&recent, vec![0u8; 3]).unwrap();
        // Backdate the old file by 2 days.
        let past = SystemTime::now() - Duration::from_secs(2 * 86_400);
        File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(past)
            .unwrap();

        cmd_fs_clean_shell(
            &["--older-than".to_string(), "1d".to_string()],
            false,
            &dir,
            &db_path,
        )
        .unwrap();
        assert!(!old.exists(), "old file removed");
        assert!(recent.exists(), "recent file kept");

        // No filter removes everything, then prunes the empty dir.
        cmd_fs_clean_shell(&[], false, &dir, &db_path).unwrap();
        assert!(!recent.exists());
        assert!(!shell.exists(), "empty project dir pruned");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_shell_dry_run_changes_nothing() {
        let dir = temp_data_dir("shell-dry");
        let db_path = dir.join("opencode.db");
        let shell = dir.join("shell/p1");
        fs::create_dir_all(&shell).unwrap();
        fs::write(shell.join("sh_x.out"), vec![0u8; 4]).unwrap();

        cmd_fs_clean_shell(&[], true, &dir, &db_path).unwrap();
        assert!(shell.join("sh_x.out").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_shell_failure_reports_progress() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_data_dir("shell-fail");
        let db_path = dir.join("opencode.db");
        let shell = dir.join("shell/p1");
        fs::create_dir_all(&shell).unwrap();
        // Sorted first, so it is attempted first and fails.
        fs::write(shell.join("sh_aaa.out"), vec![0u8; 4]).unwrap();
        fs::write(shell.join("sh_zzz.out"), vec![0u8; 4]).unwrap();
        // Read-only dir: file removal inside it fails for non-root.
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o555)).unwrap();

        let err = cmd_fs_clean_shell(&[], false, &dir, &db_path).unwrap_err();
        assert!(
            err.message.contains("0 of 2 file(s) already removed"),
            "unexpected message: {}",
            err.message
        );

        fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_log_older_than_prunes_lines() {
        let dir = temp_data_dir("log-filter");
        let db_path = dir.join("opencode.db");
        let log = dir.join("log/opencode.log");
        fs::create_dir_all(dir.join("log")).unwrap();
        let now = crate::util::now_ms().unwrap();
        // Old line (2020), recent line (now), dateless line (kept).
        let old = "timestamp=2020-01-01T00:00:00Z level=INFO old\n";
        let recent_day = crate::util::dt(now);
        let recent = format!("timestamp={recent_day} level=INFO new\n");
        fs::write(&log, format!("{old}{recent}dateless line\n")).unwrap();

        cmd_fs_clean_log(
            &["--older-than".to_string(), "30d".to_string()],
            false,
            &dir,
            &db_path,
        )
        .unwrap();
        let content = fs::read_to_string(&log).unwrap();
        assert!(!content.contains("old"), "old line pruned");
        assert!(content.contains("new"), "recent line kept");
        assert!(content.contains("dateless"), "dateless line kept");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_positional_args() {
        let dir = temp_data_dir("reject-args");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);
        assert!(cmd_fs_clean_snapshots(&con, &["x".into()], true, &dir, &db_path).is_err());
        assert!(cmd_fs_clean_snapshots(&con, &[], true, &dir, &db_path).is_ok());
        assert!(cmd_fs_clean_log(&["x".into()], true, &dir, &db_path).is_err());
        assert!(cmd_fs_clean_shell(&["x".into()], true, &dir, &db_path).is_err());
        drop(con);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_log_truncates_to_zero() {
        let dir = temp_data_dir("log");
        let db_path = dir.join("opencode.db");
        let log = dir.join("log/opencode.log");
        fs::create_dir_all(dir.join("log")).unwrap();
        fs::write(&log, vec![0u8; 100]).unwrap();

        cmd_fs_clean_log(&[], false, &dir, &db_path).unwrap();

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

        cmd_fs_clean_log(&[], true, &dir, &db_path).unwrap();

        assert_eq!(fs::metadata(&log).unwrap().len(), 100);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_log_missing_file_is_noop() {
        let dir = temp_data_dir("log-missing");
        let db_path = dir.join("opencode.db");

        cmd_fs_clean_log(&[], false, &dir, &db_path).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }
}
