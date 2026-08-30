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

use error::{AppError, Result};
use std::env;
use std::path::Path;
use std::process::exit;

fn usage() {
    println!("opencode-dbtool - opencode db maintenance");
    println!();
    println!("USAGE:");
    println!("  opencode-dbtool stats                    table sizes + totals (DB overview)");
    println!("  opencode-dbtool doctor                   integrity + consistency checks");
    println!("  opencode-dbtool project list             project overview (counts, sizes)");
    println!(
        "  opencode-dbtool project list --path <dir>  list only projects at directory (repeatable)"
    );
    println!("  opencode-dbtool project show <id>        project detail (sessions, breakdown)");
    println!("  opencode-dbtool project delete <id>...   delete project(s) + all related data");
    println!("  opencode-dbtool project purge [--older-than <age>] [--path <dir>...]  delete projects matching all filters");
    println!("  opencode-dbtool session list             per-session breakdown (full ids)");
    println!("  opencode-dbtool session show <id>        session detail");
    println!("  opencode-dbtool session delete <id>...   delete session(s) + cascade");
    println!("  opencode-dbtool session purge [--older-than <age>] [--subagents] [--path <dir>...] [--larger-than <size>] [--keep-latest <n>]  delete sessions matching all filters");
    println!("  opencode-dbtool session strip-reasoning [--older-than <age>] [--subagents] [--path <dir>...] [--larger-than <size>] [--keep-latest <n>]  delete only the reasoning parts of matching sessions");
    println!("  opencode-dbtool fs clean-orphans      delete session_diff files with no matching session");
    println!("  opencode-dbtool fs clean-snapshots    delete all snapshot (undo/redo) storage");
    println!("  opencode-dbtool vacuum [--no-backup]    run VACUUM (backup + verify by default)");
    println!("  opencode-dbtool [--help]                 show this message");
    println!();
    println!("OUTPUT:");
    println!("  all commands print JSON to stdout; errors go to stderr.");
    println!();
    println!("FLAGS:");
    println!("  --dry-run, -n   print actions without changing anything");

    println!();
    println!("ENV:");
    println!("  OPENCODE_DATA_DIR  override data dir (default: XDG_DATA_HOME/opencode, ~/.local/share/opencode)");
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

fn run() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let dry_run = args.iter().any(|a| a == "--dry-run" || a == "-n");
    let filtered: Vec<String> = args
        .into_iter()
        .filter(|a| !matches!(a.as_str(), "--dry-run" | "-n"))
        .collect();

    if filtered.iter().any(|a| a == "-h" || a == "--help") {
        usage();
        return Ok(());
    }

    let Some(dir) = config::data_dir() else {
        return Err(AppError::usage("cannot determine opencode data dir"));
    };
    let db_path = dir.join("opencode.db");

    let command = filtered.first().cloned().unwrap_or_default();
    let rest: &[String] = if command.is_empty() {
        &[]
    } else {
        &filtered[1..]
    };

    match command.as_str() {
        "" => {
            usage();
            Ok(())
        }
        "vacuum" => {
            let opts = commands::vacuum::parse_vacuum_args(rest)?;
            require_db(&db_path)
                    .and_then(|_| {
                        if dry_run {
                            Ok(())
                        } else {
                            sys::require_idle("VACUUM needs exclusive access")
                        }
                    })
                    .and_then(|_| {
                        let con = db::open_conn(&db_path, dry_run)?;
                        let integrity = db::quick_check(&con);
                        if integrity != "ok" {
                            return Err(AppError::db(format!(
                                "integrity check not ok ({integrity}) - abort"
                            )));
                        }
                        let freelist: i64 = con
                            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
                            .unwrap_or(-1);
                        let mut out = db::db_status(&db_path);
                        out["dry_run"] = serde_json::json!(dry_run);
                        out["db_bytes_before"] = serde_json::json!(db::file_size(&db_path));
                        out["free_pages_before"] = serde_json::json!(freelist);
                        out["backup"] = if opts.backup {
                            serde_json::json!({
                                "path": commands::vacuum::planned_backup_path(&db_path).to_string_lossy().to_string(),
                                "bytes": db::file_size(&db_path),
                            })
                        } else {
                            serde_json::Value::Null
                        };
                        if dry_run {
                            return output::print_json(&out);
                        }
                        let after = commands::vacuum::cmd_vacuum(&con, &db_path, &opts)?;
                        out["backup"] = after["backup"].clone();
                        out["db_bytes_after"] = after["db_bytes"].clone();
                        out["wal_bytes_after"] = after["wal_bytes"].clone();
                        out["free_pages_after"] = after["free_pages"].clone();
                        out["integrity"] = after["integrity"].clone();
                        output::print_json(&out)
                    })
        }
        "stats" => require_db(&db_path).and_then(|_| {
            let con = db::open_conn(&db_path, true)?;
            commands::stats::cmd_stats(&con, &db_path)
        }),
        "doctor" => require_db(&db_path).and_then(|_| {
            let con = db::open_conn(&db_path, true)?;
            commands::doctor::cmd_doctor(&con, &db_path)
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
                    if dry_run {
                        Ok(())
                    } else {
                        sys::require_idle("deleting while opencode is running is not allowed")
                    }
                })
                .and_then(|_| {
                    let mut con = db::open_conn(&db_path, dry_run)?;
                    if rest.first().map(|s| s.as_str()) == Some("delete") {
                        commands::project::cmd_project_delete(
                            &mut con,
                            &rest[1..],
                            dry_run,
                            &db_path,
                        )
                    } else {
                        commands::project::cmd_project_purge(
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
        "session" => match rest.first().map(|s| s.as_str()).unwrap_or("") {
            "list" => {
                if rest.len() != 1 {
                    usage();
                    return Err(AppError::silent(error::EXIT_NOT_FOUND));
                }
                require_db(&db_path).and_then(|_| {
                    let con = db::open_conn(&db_path, true)?;
                    commands::session::cmd_session_list(&con, &db_path)
                })
            }
            "show" => require_db(&db_path).and_then(|_| {
                let con = db::open_conn(&db_path, true)?;
                commands::session::cmd_session_show(&con, &rest[1..], &db_path)
            }),
            "delete" => require_db(&db_path)
                .and_then(|_| {
                    if dry_run {
                        Ok(())
                    } else {
                        sys::require_idle("deleting while opencode is running is not allowed")
                    }
                })
                .and_then(|_| {
                    let mut con = db::open_conn(&db_path, dry_run)?;
                    commands::session::cmd_session_delete(&mut con, &rest[1..], dry_run, &db_path)
                }),
            "purge" | "strip-reasoning" => require_db(&db_path)
                .and_then(|_| {
                    if dry_run {
                        Ok(())
                    } else {
                        sys::require_idle("deleting while opencode is running is not allowed")
                    }
                })
                .and_then(|_| {
                    let mut con = db::open_conn(&db_path, dry_run)?;
                    if rest.first().map(|s| s.as_str()) == Some("purge") {
                        commands::session::cmd_session_purge(
                            &mut con,
                            &rest[1..],
                            dry_run,
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
            "clean-orphans" => require_db(&db_path).and_then(|_| {
                // Unguarded: orphan diff files are never referenced
                // by a live opencode session.
                let con = db::open_conn(&db_path, true)?;
                commands::fsops::cmd_fs_clean_orphans(&con, &rest[1..], dry_run, &db_path)
            }),
            "clean-snapshots" => require_db(&db_path)
                .and_then(|_| {
                    if dry_run {
                        Ok(())
                    } else {
                        sys::require_idle("snapshots are in use while opencode runs")
                    }
                })
                .and_then(|_| {
                    commands::fsops::cmd_fs_clean_snapshots(&rest[1..], dry_run, &db_path)
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
