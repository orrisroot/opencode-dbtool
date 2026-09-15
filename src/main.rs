//! opencode-dbtool: maintenance tool for the opencode SQLite database.
//!
//! Module layout:
//! - `config`    data-dir resolution
//! - `sys`       running opencode detection
//! - `db`        connection handling, generic DB helpers
//! - `repo`      all SQL against session/project tables
//! - `models`    read models + JSON conversion + filtering
//! - `commands`  one module per top-level subcommand
//! - `output`    JSON printing
//! - `util`      formatting helpers
//! - `error`     AppError (exit code + message)

mod commands;
mod config;
mod db;
mod error;
mod models;
mod output;
mod repo;
mod sys;
#[cfg(test)]
mod testdb;
mod util;

use crate::commands::vacuum::{BackupCleanupOut, BackupOut};
use crate::db::EnvStatus;
use error::{AppError, Result};
use serde::Serialize;
use std::env;
use std::path::Path;
use std::process::exit;

/// JSON shape of the `vacuum` command output.
#[derive(Serialize)]
struct VacuumOut {
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

fn usage() {
    println!("opencode-dbtool - opencode db maintenance");
    println!();
    println!("USAGE:");
    println!("  opencode-dbtool stats [--detail]           table sizes + totals (DB overview)");
    println!("  opencode-dbtool doctor                   integrity + consistency checks");
    println!("  opencode-dbtool project list             project overview (counts, sizes)");
    println!(
        "  opencode-dbtool project list --path <dir>  list only projects at directory (repeatable)"
    );
    println!("  opencode-dbtool project show <id>        project detail (sessions, breakdown)");
    println!("  opencode-dbtool project delete <id>...   delete project(s) + all related data");
    println!("  opencode-dbtool project purge [--older-than <age>] [--path <dir>...]  delete projects matching all filters");
    println!("  opencode-dbtool session list [--sort size] [--limit <n>]  per-session breakdown (full ids)");
    println!("  opencode-dbtool session show <id>        session detail");
    println!("  opencode-dbtool session delete <id>...   delete session(s) + cascade");
    println!("  opencode-dbtool session purge [--older-than <age>] [--subagents] [--path <dir>...] [--larger-than <size>] [--keep-latest <n>]  delete sessions matching all filters");
    println!("  opencode-dbtool session strip-reasoning [--older-than <age>] [--subagents] [--path <dir>...] [--larger-than <size>] [--keep-latest <n>]  delete only the reasoning parts of matching sessions");
    println!("  opencode-dbtool fs clean-orphans      delete session_diff files with no matching session");
    println!("  opencode-dbtool fs clean-snapshots    delete all snapshot (undo/redo) storage");
    println!("  opencode-dbtool fs clean-tool-output  delete all truncated tool output");
    println!("  opencode-dbtool fs clean-log          truncate log/opencode.log to zero bytes");
    println!("  opencode-dbtool vacuum [--no-backup] [--keep-backups <n>]  run VACUUM (backup + verify by default)");
    println!("  opencode-dbtool self-update [--dry-run|--yes]  check/apply the latest GitHub release binary");
    println!("  opencode-dbtool [--help]                 show this message");
    println!();
    println!("OUTPUT:");
    println!("  all commands print JSON to stdout; errors go to stderr.");
    println!();
    println!("FLAGS:");
    println!("  --dry-run, -n   print actions without changing anything");
    println!("  --yes, -y       confirm a destructive command (required unless --dry-run)");

    println!();
    println!("ENV:");
    println!("  OPENCODE_DATA_DIR  override data dir (default: XDG_DATA_HOME/opencode, ~/.local/share/opencode)");
    println!("  OPENCODE_DB        override database path (:memory: or absolute; relative resolves against the data dir)");
}

fn require_db(db_path: &Path) -> Result<()> {
    if !db_path.exists() {
        return Err(AppError::usage(format!(
            "DB not found: {}",
            db_path.display()
        )));
    }
    Ok(())
}

/// Destructive commands refuse to run without explicit confirmation
/// (`--yes`); `--dry-run` previews instead and is always allowed.
fn require_confirmation(dry_run: bool, yes: bool, what: &str) -> Result<()> {
    if dry_run || yes {
        return Ok(());
    }
    Err(AppError::usage(format!(
        "{what} modifies data; pass `--dry-run` to preview the impact or `--yes` to confirm"
    )))
}

/// `--yes` plus the idle guard: confirmation first (most actionable),
/// then the running-instance check.
fn require_mutation_guard(dry_run: bool, yes: bool, what: &str, idle: &str) -> Result<()> {
    require_confirmation(dry_run, yes, what)?;
    if !dry_run {
        sys::require_idle(idle)?;
    }
    Ok(())
}

fn run() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let dry_run = args.iter().any(|a| a == "--dry-run" || a == "-n");
    let yes = args.iter().any(|a| a == "--yes" || a == "-y");
    let filtered: Vec<String> = args
        .into_iter()
        .filter(|a| !matches!(a.as_str(), "--dry-run" | "-n" | "--yes" | "-y"))
        .collect();

    if filtered.iter().any(|a| a == "-h" || a == "--help") {
        usage();
        return Ok(());
    }

    let command = filtered.first().cloned().unwrap_or_default();
    let rest: &[String] = if command.is_empty() {
        &[]
    } else {
        &filtered[1..]
    };

    // `self-update` touches neither the database nor the data dir; dispatch it
    // before either is resolved.
    if command == "self-update" {
        return require_confirmation(dry_run, yes, "self-update")
            .and_then(|_| commands::selfupdate::cmd_self_update(dry_run, rest));
    }

    let Some(dir) = config::data_dir() else {
        return Err(AppError::usage("cannot determine opencode data dir"));
    };
    // Database path: `$OPENCODE_DB` if set, then `opencode.db` in the
    // data dir, then a channel-named `opencode-<channel>.db`.
    let db_path = config::db_path()?;

    match command.as_str() {
        "" => {
            usage();
            Ok(())
        }
        "vacuum" => {
            let opts = commands::vacuum::parse_vacuum_args(rest)?;
            require_db(&db_path)
                .and_then(|_| {
                    require_mutation_guard(dry_run, yes, "VACUUM", "VACUUM needs exclusive access")
                })
                .and_then(|_| {
                    let con = db::open_conn(&db_path, dry_run)?;
                    let integrity = db::quick_check(&con);
                    if integrity != "ok" {
                        return Err(AppError::db(format!(
                            "integrity check not ok ({integrity}) - abort"
                        )));
                    }
                    let freelist: i64 = con.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
                    let mut out = VacuumOut {
                        env: db::env_status(&db_path),
                        dry_run,
                        db_bytes_before: db::file_size(&db_path),
                        free_pages_before: freelist,
                        backup: if opts.backup {
                            Some(BackupOut {
                                path: commands::vacuum::planned_backup_path(&db_path)
                                    .to_string_lossy()
                                    .to_string(),
                                bytes: db::file_size(&db_path),
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
                        return output::print_json(&serde_json::to_value(&out)?);
                    }
                    let after = commands::vacuum::cmd_vacuum(&con, &db_path, &opts)?;
                    out.db_bytes_after = Some(after.db_bytes);
                    out.wal_bytes_after = Some(after.wal_bytes);
                    out.free_pages_after = Some(after.free_pages);
                    out.integrity = Some(after.integrity);
                    out.backup = after.backup;
                    out.backup_cleanup = after.backup_cleanup;
                    output::print_json(&serde_json::to_value(&out)?)
                })
        }
        "stats" => require_db(&db_path).and_then(|_| {
            let con = db::open_conn(&db_path, true)?;
            commands::stats::cmd_stats(&con, &dir, &db_path, rest)
        }),
        "doctor" => require_db(&db_path).and_then(|_| {
            let con = db::open_conn(&db_path, true)?;
            commands::doctor::cmd_doctor(&con, &db_path, rest)
        }),
        "project" => match rest.first().map(|s| s.as_str()).unwrap_or("") {
            "list" => require_db(&db_path).and_then(|_| {
                let con = db::open_conn(&db_path, true)?;
                commands::project::cmd_project_list(&con, &rest[1..])
            }),
            "show" => require_db(&db_path).and_then(|_| {
                let con = db::open_conn(&db_path, true)?;
                commands::project::cmd_project_show(&con, &rest[1..])
            }),
            "delete" | "purge" => require_db(&db_path)
                .and_then(|_| {
                    require_mutation_guard(
                        dry_run,
                        yes,
                        "project delete",
                        "deleting while opencode is running is not allowed",
                    )
                })
                .and_then(|_| {
                    let mut con = db::open_conn(&db_path, dry_run)?;
                    if rest.first().map(|s| s.as_str()) == Some("delete") {
                        commands::project::cmd_project_delete(
                            &mut con,
                            &rest[1..],
                            dry_run,
                            &dir,
                            &db_path,
                        )
                    } else {
                        commands::project::cmd_project_purge(
                            &mut con,
                            &rest[1..],
                            dry_run,
                            &dir,
                            &db_path,
                        )
                    }
                }),
            _ => {
                usage();
                Err(AppError::silent(error::EXIT_NOT_FOUND))
            }
        },
        "session" => match rest.first().map(|s| s.as_str()).unwrap_or("") {
            "list" => require_db(&db_path).and_then(|_| {
                let con = db::open_conn(&db_path, true)?;
                commands::session::cmd_session_list(&con, &dir, &rest[1..])
            }),
            "show" => require_db(&db_path).and_then(|_| {
                let con = db::open_conn(&db_path, true)?;
                commands::session::cmd_session_show(&con, &dir, &rest[1..])
            }),
            "delete" => require_db(&db_path)
                .and_then(|_| {
                    require_mutation_guard(
                        dry_run,
                        yes,
                        "session delete",
                        "deleting while opencode is running is not allowed",
                    )
                })
                .and_then(|_| {
                    let mut con = db::open_conn(&db_path, dry_run)?;
                    commands::session::cmd_session_delete(
                        &mut con,
                        &rest[1..],
                        dry_run,
                        &dir,
                        &db_path,
                    )
                }),
            "purge" | "strip-reasoning" => require_db(&db_path)
                .and_then(|_| {
                    require_mutation_guard(
                        dry_run,
                        yes,
                        "session operation",
                        "deleting while opencode is running is not allowed",
                    )
                })
                .and_then(|_| {
                    let mut con = db::open_conn(&db_path, dry_run)?;
                    if rest.first().map(|s| s.as_str()) == Some("purge") {
                        commands::session::cmd_session_purge(
                            &mut con,
                            &rest[1..],
                            dry_run,
                            &dir,
                            &db_path,
                        )
                    } else {
                        commands::session::cmd_session_strip_reasoning(
                            &mut con,
                            &rest[1..],
                            dry_run,
                            &db_path,
                        )
                    }
                }),
            _ => {
                usage();
                Err(AppError::silent(error::EXIT_NOT_FOUND))
            }
        },
        "fs" => match rest.first().map(|s| s.as_str()).unwrap_or("") {
            "clean-orphans" => require_db(&db_path)
                .and_then(|_| require_confirmation(dry_run, yes, "fs clean-orphans"))
                .and_then(|_| {
                    // Unguarded: orphan diff files are never referenced
                    // by a live opencode session.
                    let con = db::open_conn(&db_path, true)?;
                    commands::fsops::cmd_fs_clean_orphans(&con, &rest[1..], dry_run, &dir, &db_path)
                }),
            "clean-snapshots" => require_db(&db_path)
                .and_then(|_| {
                    require_mutation_guard(
                        dry_run,
                        yes,
                        "fs clean-snapshots",
                        "snapshots are in use while opencode runs",
                    )
                })
                .and_then(|_| {
                    commands::fsops::cmd_fs_clean_snapshots(&rest[1..], dry_run, &dir, &db_path)
                }),
            "clean-tool-output" => require_confirmation(dry_run, yes, "fs clean-tool-output")
                .and_then(|_| {
                    // Unguarded: the same retention-based cleanup opencode
                    // performs itself while running.
                    commands::fsops::cmd_fs_clean_tool_output(&rest[1..], dry_run, &dir, &db_path)
                }),
            "clean-log" => require_db(&db_path)
                .and_then(|_| {
                    require_mutation_guard(
                        dry_run,
                        yes,
                        "fs clean-log",
                        "truncating the log while opencode runs is not allowed",
                    )
                })
                .and_then(|_| {
                    commands::fsops::cmd_fs_clean_log(&rest[1..], dry_run, &dir, &db_path)
                }),
            _ => {
                usage();
                Err(AppError::silent(error::EXIT_NOT_FOUND))
            }
        },
        _ => {
            usage();
            Err(AppError::silent(error::EXIT_NOT_FOUND))
        }
    }
}

fn main() {
    match run() {
        Ok(()) => exit(error::EXIT_OK),
        Err(e) => {
            if !e.message.is_empty() {
                eprintln!("error: {}", e.message);
            }
            exit(e.code);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_required_unless_yes_or_dry_run() {
        assert!(require_confirmation(false, false, "session purge").is_err());
        assert!(require_confirmation(false, true, "session purge").is_ok());
        assert!(require_confirmation(true, false, "session purge").is_ok());
    }
}
