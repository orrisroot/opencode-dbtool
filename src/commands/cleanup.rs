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
use crate::service::ServiceInfo;
use crate::{commands, db};
use serde::Serialize;
use std::io::IsTerminal;
use std::path::Path;

#[derive(Serialize)]
struct CleanupOut {
    #[serde(flatten)]
    env: EnvStatus,
    dry_run: bool,
    action: &'static str,
    filters: PurgeFilterJson,
    fs_older_than: String,
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
    let fs_age = args.fs_older_than.as_str();
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
            &mut con, &filters, dry_run, db_path, service,
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
    //    Skipped while opencode runs: VACUUM needs the write lock, so it
    //    is left to the explicit `vacuum --online` command.
    let vacuum_skipped = service_running && !args.no_vacuum;
    let vacuum = if args.no_vacuum || service_running {
        None
    } else {
        progress(quiet, dry_run, "vacuuming the database");
        let opts = commands::vacuum::VacuumOpts {
            backup: false,
            keep_backups: None,
        };
        Some(without_env(commands::vacuum::vacuum_value(
            &con, db_path, &opts, dry_run, false,
        )?))
    };

    let note = if dry_run {
        if vacuum_skipped {
            Some(
                "preview only; VACUUM will be skipped while opencode is running (use `vacuum --online`)"
                    .to_string(),
            )
        } else {
            Some("preview only; re-run with --yes to apply".to_string())
        }
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
    let out = CleanupOut {
        env: env_status(db_path),
        dry_run,
        action: "cleanup",
        filters: filters.json(),
        fs_older_than: fs_age.to_string(),
        backup,
        purge,
        blob_orphans,
        snapshots,
        shell,
        log,
        vacuum,
        cleaned: !dry_run,
        note,
    };
    output::emit(&serde_json::to_value(&out)?)
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
