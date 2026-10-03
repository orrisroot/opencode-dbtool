//! `cleanup`: one-shot maintenance run that removes old sessions and
//! stale storage, then compacts the database.
//!
//! Steps (in order): optional pre-run backup, session purge (only when
//! session filters are given), orphan blob cleanup, orphan snapshot
//! cleanup, old shell output cleanup, log pruning, and a final VACUUM.
//! Without session filters nothing is purged, so a bare `cleanup` only
//! removes orphans and old files.

use crate::cli::CleanupArgs;
use crate::db::{env_status, EnvStatus};
use crate::error::Result;
use crate::models::{PurgeFilter, PurgeFilterJson};
use crate::output;
use crate::output::human_bytes;
use crate::service::ServiceInfo;
use crate::{commands, db};
use serde::Serialize;
use std::io::IsTerminal;
use std::path::Path;

/// Compact aggregate of all steps, for scripts and the table summary.
#[derive(Serialize)]
struct CleanupSummary {
    sessions: usize,
    rows: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup: Option<String>,
    blob_orphans: usize,
    blob_orphan_bytes: u64,
    snapshot_entries: usize,
    snapshot_bytes: u64,
    shell_files: usize,
    shell_bytes: u64,
    log_bytes_before: u64,
    log_bytes_after: u64,
    /// `ran`, `planned` (dry-run), `skipped` (service running), or
    /// `disabled` (`--no-vacuum`).
    vacuum: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    db_bytes_before: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    db_bytes_after: Option<u64>,
    /// Post-run doctor result (only with `--verify` on a real run).
    #[serde(skip_serializing_if = "Option::is_none")]
    verify_ok: Option<bool>,
}

#[derive(Serialize)]
struct CleanupOut {
    #[serde(flatten)]
    env: EnvStatus,
    dry_run: bool,
    action: &'static str,
    filters: PurgeFilterJson,
    fs_older_than: String,
    summary: CleanupSummary,
    /// Pre-run verified backup (omitted without session filters or with
    /// `--no-backup`).
    backup: Option<serde_json::Value>,
    /// Session purge result (only when session filters were given).
    purge: Option<serde_json::Value>,
    blob_orphans: serde_json::Value,
    snapshots: serde_json::Value,
    shell: serde_json::Value,
    log: serde_json::Value,
    /// Final VACUUM result (omitted with `--no-vacuum`).
    vacuum: Option<serde_json::Value>,
    /// Post-run health check (only with `--verify` on a real run).
    verify: Option<serde_json::Value>,
    cleaned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

/// Remove each nested step's environment block so the aggregate output
/// carries a single one.
fn without_env(mut v: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = v.as_object_mut() {
        for key in ["opencode_running", "pids", "pid_error", "db"] {
            obj.remove(key);
        }
    }
    v
}

fn progress(quiet: bool, dry_run: bool, message: &str) {
    if !quiet && !dry_run && std::io::stderr().is_terminal() {
        eprintln!("cleanup: {message}");
    }
}

pub fn cmd_cleanup(
    data_dir: &Path,
    db_path: &Path,
    args: &CleanupArgs,
    dry_run: bool,
    quiet: bool,
    service: Option<&ServiceInfo>,
) -> Result<()> {
    let filters = PurgeFilter::try_from(&args.purge)?;
    let fs_age = args.fs_older_than.as_deref().unwrap_or("7d");
    let service_running = service.is_some();
    let mut con = db::open_conn(db_path, dry_run)?;

    // 1. Backup before any session deletion (the VACUUM at the end skips
    //    its own backup: this one already covers the pre-change state).
    //    The online backup API works while opencode runs.
    let backup = if !filters.is_empty() && !args.no_backup {
        progress(quiet, dry_run, "creating verified backup");
        Some(without_env(serde_json::to_value(
            commands::vacuum::backup_value(db_path, dry_run, args.keep_backups)?,
        )?))
    } else {
        None
    };

    // 2. Session purge (skipped without session filters: a bare cleanup
    //    never deletes sessions). With a running server the deletes are
    //    routed through its API so caches and the event log stay intact.
    let purge = if filters.is_empty() {
        None
    } else {
        progress(quiet, dry_run, "purging matching sessions");
        Some(without_env(commands::session::session_purge_value(
            &mut con, &filters, dry_run, db_path, service, None,
        )?))
    };

    // 3. Orphan instruction blobs (leaked by session/project deletes).
    progress(quiet, dry_run, "removing orphan instruction blobs");
    let blob_orphans = without_env(commands::fsops::blob_orphans_value(
        &mut con, dry_run, db_path,
    )?);

    // 4. Snapshot directories whose project no longer exists.
    progress(quiet, dry_run, "removing orphan snapshot storage");
    let snapshots = without_env(commands::fsops::snapshots_cleanup_value(
        &con,
        &[],
        true,
        dry_run,
        data_dir,
        db_path,
    )?);

    // 5. Old shell outputs and log lines.
    progress(quiet, dry_run, "removing old shell outputs");
    let shell = without_env(commands::fsops::shell_cleanup_value(
        Some(fs_age),
        dry_run,
        data_dir,
        db_path,
    )?);
    progress(quiet, dry_run, "pruning the log");
    let log = without_env(commands::fsops::log_cleanup_value(
        Some(fs_age),
        dry_run,
        data_dir,
        db_path,
    )?);

    // 6. Final VACUUM (no second backup; the pre-run one is kept).
    //    While opencode runs the VACUUM is skipped unless
    //    `--vacuum-online` explicitly allows it.
    let vacuum_skipped = service_running && !args.vacuum_online && !args.no_vacuum;
    let vacuum = if args.no_vacuum || (service_running && !args.vacuum_online) {
        None
    } else {
        progress(quiet, dry_run, "vacuuming the database");
        if service_running {
            con.busy_timeout(std::time::Duration::from_secs(60))
                .map_err(|e| crate::error::AppError::db(format!("busy_timeout: {e}")))?;
        }
        let opts = commands::vacuum::VacuumOpts {
            backup: false,
            keep_backups: None,
        };
        Some(without_env(commands::vacuum::vacuum_value(
            &con,
            db_path,
            &opts,
            dry_run,
            service_running && args.vacuum_online,
        )?))
    };

    // 7. Optional post-run health check.
    let verify_result = if args.verify && !dry_run {
        progress(quiet, dry_run, "verifying database health");
        Some(without_env(commands::doctor::doctor_value(&con, db_path)?))
    } else {
        None
    };

    let note = if dry_run {
        let mut note = if vacuum_skipped {
            "preview only; VACUUM will be skipped while opencode is running (use `vacuum --online`)"
                .to_string()
        } else {
            "preview only; re-run with --yes to apply".to_string()
        };
        if args.verify {
            note.push_str("; `--verify` runs after the real cleanup");
        }
        Some(note)
    } else if vacuum_skipped {
        Some(
            "VACUUM skipped while opencode is running; run `opencode-dbtool vacuum --online` or stop the service"
                .to_string(),
        )
    } else if args.no_vacuum {
        Some(
            "deleted rows free space only after `opencode-dbtool vacuum` (skipped with --no-vacuum)"
                .to_string(),
        )
    } else {
        None
    };
    let vacuum_status = if args.no_vacuum {
        "disabled"
    } else if vacuum_skipped {
        "skipped"
    } else if dry_run {
        "planned"
    } else {
        "ran"
    };
    let summary = CleanupSummary {
        sessions: purge
            .as_ref()
            .and_then(|p| p.get("sessions"))
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0),
        rows: purge
            .as_ref()
            .and_then(|p| p.get("total_rows"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        backup: backup
            .as_ref()
            .and_then(|b| b.get("backup"))
            .and_then(|b| b.get("path"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        blob_orphans: json_u64(&blob_orphans, "total_blobs") as usize,
        blob_orphan_bytes: json_u64(&blob_orphans, "total_bytes"),
        snapshot_entries: snapshots
            .get("entries")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0),
        snapshot_bytes: json_u64(&snapshots, "total_bytes"),
        shell_files: json_u64(&shell, "total_files") as usize,
        shell_bytes: json_u64(&shell, "total_bytes"),
        log_bytes_before: json_u64(&log, "bytes"),
        log_bytes_after: json_u64(&log, "remaining_bytes"),
        vacuum: vacuum_status.to_string(),
        db_bytes_before: vacuum
            .as_ref()
            .and_then(|v| v.get("db_bytes_before"))
            .and_then(|v| v.as_u64()),
        db_bytes_after: vacuum
            .as_ref()
            .and_then(|v| v.get("db_bytes_after"))
            .and_then(|v| v.as_u64()),
        verify_ok: verify_result
            .as_ref()
            .and_then(|v| v.get("ok"))
            .and_then(|v| v.as_bool()),
    };
    let out = CleanupOut {
        env: env_status(db_path),
        dry_run,
        action: "cleanup",
        filters: filters.json(),
        fs_older_than: fs_age.to_string(),
        summary,
        backup,
        purge,
        blob_orphans,
        snapshots,
        shell,
        log,
        vacuum,
        verify: verify_result,
        cleaned: !dry_run,
        note,
    };
    if output::table_mode() {
        output::emit_text(&summary_table(&out))
    } else {
        output::emit(&serde_json::to_value(&out)?)
    }
}

fn json_u64(value: &serde_json::Value, key: &str) -> u64 {
    value.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// Concise table-mode rendering: one line per aggregate instead of the
/// full nested step output.
fn summary_table(out: &CleanupOut) -> String {
    let s = &out.summary;
    let mut lines: Vec<(&str, String)> = Vec::new();
    lines.push((
        "action",
        if out.dry_run {
            "cleanup (dry-run)".to_string()
        } else {
            "cleanup".to_string()
        },
    ));
    if s.sessions > 0 {
        lines.push(("sessions", format!("{} ({} rows)", s.sessions, s.rows)));
    } else {
        lines.push(("sessions", "none".to_string()));
    }
    if let Some(path) = &s.backup {
        lines.push(("backup", path.clone()));
    }
    lines.push((
        "blob orphans",
        format!(
            "{} ({})",
            s.blob_orphans,
            human_bytes(s.blob_orphan_bytes as i64)
        ),
    ));
    lines.push((
        "snapshots",
        format!(
            "{} ({})",
            s.snapshot_entries,
            human_bytes(s.snapshot_bytes as i64)
        ),
    ));
    lines.push((
        "shell files",
        format!("{} ({})", s.shell_files, human_bytes(s.shell_bytes as i64)),
    ));
    lines.push((
        "log",
        format!(
            "{} -> {}",
            human_bytes(s.log_bytes_before as i64),
            human_bytes(s.log_bytes_after as i64)
        ),
    ));
    let vacuum = match (s.vacuum.as_str(), s.db_bytes_before, s.db_bytes_after) {
        ("ran", Some(before), Some(after)) => format!(
            "ran ({} -> {})",
            human_bytes(before as i64),
            human_bytes(after as i64)
        ),
        ("planned", Some(before), _) => format!("planned ({})", human_bytes(before as i64)),
        (status, _, _) => status.to_string(),
    };
    lines.push(("vacuum", vacuum));
    if let Some(ok) = s.verify_ok {
        lines.push(("verify", if ok { "ok" } else { "FAILED" }.to_string()));
    }
    if let Some(note) = &out.note {
        lines.push(("note", note.clone()));
    }
    let width = lines.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    lines
        .iter()
        .map(|(k, v)| format!("{k:width$}  {v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb;
    use clap::Parser;

    fn cleanup_args(argv: &[&str]) -> CleanupArgs {
        let mut full = vec!["opencode-dbtool", "cleanup"];
        full.extend(argv);
        match crate::cli::Cli::try_parse_from(full)
            .expect("valid cleanup args")
            .command
            .unwrap()
        {
            crate::cli::Command::Cleanup(a) => a,
            _ => unreachable!("expected cleanup"),
        }
    }

    fn temp_dir(stem: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "opencode-dbtool-cleanup-{}-{}",
            std::process::id(),
            stem
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bare_cleanup_dry_run_purges_nothing() {
        let dir = temp_dir("bare-dry");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);
        testdb::insert_session(&con, "s1", "/a", None);
        con.execute(
            "INSERT INTO instruction_blob (hash, value) VALUES ('orphan', 'x')",
            [],
        )
        .unwrap();
        drop(con);
        std::fs::create_dir_all(dir.join("shell/p1")).unwrap();
        std::fs::write(dir.join("shell/p1/sh_x.out"), b"x").unwrap();

        cmd_cleanup(
            &dir,
            &db_path,
            &cleanup_args(&["--no-backup"]),
            true,
            true,
            None,
        )
        .unwrap();

        let con = crate::db::open_conn(&db_path, true).unwrap();
        assert_eq!(testdb::session_count(&con), 1, "dry-run keeps sessions");
        assert!(dir.join("shell/p1/sh_x.out").exists());
        let blobs: i64 = con
            .query_row("SELECT COUNT(*) FROM instruction_blob", [], |r| r.get(0))
            .unwrap();
        assert_eq!(blobs, 1, "dry-run keeps blobs");
    }

    #[test]
    fn cleanup_with_filters_purges_and_vacuums() {
        let dir = temp_dir("purge");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);
        testdb::insert_session(&con, "old", "/a", None);
        con.execute(
            "INSERT INTO instruction_blob (hash, value) VALUES ('orphan', 'x')",
            [],
        )
        .unwrap();
        drop(con);

        cmd_cleanup(
            &dir,
            &db_path,
            &cleanup_args(&["--older-than", "30d", "--no-backup"]),
            false,
            true,
            None,
        )
        .unwrap();

        let con = crate::db::open_conn(&db_path, true).unwrap();
        assert_eq!(testdb::session_count(&con), 0, "old session purged");
        let blobs: i64 = con
            .query_row("SELECT COUNT(*) FROM instruction_blob", [], |r| r.get(0))
            .unwrap();
        assert_eq!(blobs, 0, "orphan blob removed");
    }
}
