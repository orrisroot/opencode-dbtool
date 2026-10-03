//! Command dispatch, confirmation flow, and running-instance guards.

use crate::cli::{Cli, Command, DbCmd, FsCmd, KvCmd, ProjectCmd, SessionCmd, SortKey};
use crate::confirm;
use crate::db;
use crate::error::{AppError, Result};
use crate::models::{ProjectFilter, PurgeFilter};
use crate::service::{self, ServiceInfo};
use crate::{commands, config, output, sys};
use clap::CommandFactory;
use std::io::IsTerminal;
use std::path::Path;

/// Entry point after clap parsing: set up output, then either handle the
/// no-database commands or run dispatch behind the confirmation guard.
pub fn execute(cli: Cli) -> Result<()> {
    let Some(command) = cli.command.as_ref() else {
        print_help();
        return Ok(());
    };
    output::set_format(output::effective_format(cli.format));

    if let Command::Completions(a) = command {
        let mut cmd = Cli::command();
        clap_complete::generate(a.shell, &mut cmd, "opencode-dbtool", &mut std::io::stdout());
        return Ok(());
    }
    if matches!(command, Command::SelfUpdate) {
        return with_confirmation(&cli, "self-update", None, |dry| {
            commands::selfupdate::cmd_self_update(dry)
        });
    }

    let Some(dir) = config::data_dir() else {
        return Err(AppError::usage("cannot determine opencode data dir"));
    };
    let db_path = config::db_path()?;
    require_db(&db_path)?;
    let service = service::discover();

    match mutation_guard(command, service.as_ref(), &db_path) {
        Some((what, idle)) => with_confirmation(&cli, what, idle, |dry| {
            dispatch(command, &dir, &db_path, dry, cli.quiet, service.as_ref())
        }),
        None => dispatch(command, &dir, &db_path, false, cli.quiet, service.as_ref()),
    }
}

fn print_help() {
    let mut cmd = Cli::command();
    let _ = cmd.print_help();
    println!();
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

const IDLE_DELETE: &str = "deleting while opencode is running is not allowed";

/// Commands that modify data or storage, with the guard message shown
/// when opencode is running. `idle` is `None` for operations that are
/// safe while opencode runs (online backups, orphan/old-file cleanup, and
/// session deletes routed through the running server).
fn mutation_guard<'a>(
    cmd: &Command,
    service: Option<&ServiceInfo>,
    db_path: &Path,
) -> Option<(&'static str, Option<&'a str>)> {
    // Session deletes can go through the server API when the service
    // operates on the same database file.
    let api_online = service.is_some_and(|s| s.targets_db(db_path));
    match cmd {
        Command::Project(ProjectCmd::Delete(_)) => Some(("project delete", Some(IDLE_DELETE))),
        Command::Project(ProjectCmd::Purge(_)) => Some(("project purge", Some(IDLE_DELETE))),
        Command::Session(SessionCmd::Delete(_)) => Some((
            "session delete",
            if api_online { None } else { Some(IDLE_DELETE) },
        )),
        Command::Session(SessionCmd::Purge(_)) => Some((
            "session purge",
            if api_online { None } else { Some(IDLE_DELETE) },
        )),
        Command::Session(SessionCmd::StripReasoning(_)) => {
            Some(("session strip-reasoning", Some(IDLE_DELETE)))
        }
        Command::Kv(KvCmd::Delete(_)) => Some(("kv delete", Some(IDLE_DELETE))),
        // Online backup: consistent snapshots of a live WAL database.
        Command::Backup(_) => Some(("backup", None)),
        Command::Fs(FsCmd::Snapshots(a)) => Some((
            "fs clean-snapshots",
            if a.orphans_only {
                None
            } else {
                Some("snapshots are in use while opencode runs")
            },
        )),
        Command::Fs(FsCmd::Shell(a)) => Some((
            "fs clean-shell",
            if a.older_than.is_some() {
                None
            } else {
                Some("shell outputs are in use while opencode runs")
            },
        )),
        // Blob cleanup re-checks references inside its transaction.
        Command::Fs(FsCmd::BlobOrphans) => Some(("fs clean-blob-orphans", None)),
        Command::Fs(FsCmd::Log(_)) => Some((
            "fs clean-log",
            Some("truncating the log while opencode runs is not allowed"),
        )),
        Command::Vacuum(a) => Some((
            "VACUUM",
            if a.online {
                None
            } else {
                Some("VACUUM needs exclusive access")
            },
        )),
        Command::Cleanup(_) => Some((
            "cleanup",
            if api_online {
                None
            } else {
                Some("cleanup needs exclusive access")
            },
        )),
        _ => None,
    }
}

/// Confirmation rules:
/// - `--dry-run`: preview only, always allowed.
/// - `--yes`: execute for real; the running-instance guard applies when
///   the command is not safe online.
/// - neither, on a terminal: preview, ask `Proceed? [y/N]`, then execute.
/// - neither, not a terminal: usage error (exit 2) as before.
fn with_confirmation<F>(cli: &Cli, what: &str, idle: Option<&str>, mut run: F) -> Result<()>
where
    F: FnMut(bool) -> Result<()>,
{
    if cli.dry_run {
        return run(true);
    }
    if cli.yes {
        if let Some(message) = idle {
            sys::require_idle(message)?;
        }
        return run(false);
    }
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !interactive {
        return Err(AppError::usage(format!(
            "{what} modifies data; pass `--dry-run` to preview the impact or `--yes` to confirm"
        )));
    }
    if let Some(message) = idle {
        sys::require_idle(message)?;
    }
    run(true)?;
    if confirm::ask("Proceed?")? {
        run(false)
    } else {
        if !cli.quiet {
            eprintln!("aborted");
        }
        Ok(())
    }
}

fn dispatch(
    command: &Command,
    dir: &Path,
    db_path: &Path,
    dry_run: bool,
    quiet: bool,
    service: Option<&ServiceInfo>,
) -> Result<()> {
    match command {
        Command::Stats(a) => {
            let con = db::open_conn(db_path, true)?;
            commands::stats::cmd_stats(&con, dir, db_path, a.detail)
        }
        Command::Doctor => {
            let con = db::open_conn(db_path, true)?;
            commands::doctor::cmd_doctor(&con, db_path)
        }
        Command::Project(cmd) => match cmd {
            ProjectCmd::List(a) => {
                let con = db::open_conn(db_path, true)?;
                commands::project::cmd_project_list(&con, &a.paths)
            }
            ProjectCmd::Show(a) => {
                let con = db::open_conn(db_path, true)?;
                commands::project::cmd_project_show(&con, &a.id)
            }
            ProjectCmd::Delete(a) => {
                let mut con = db::open_conn(db_path, dry_run)?;
                commands::project::cmd_project_delete(&mut con, &a.ids, dry_run, db_path)
            }
            ProjectCmd::Purge(a) => {
                let filters = ProjectFilter::try_from(a)?;
                let mut con = db::open_conn(db_path, dry_run)?;
                commands::project::cmd_project_purge(&mut con, &filters, dry_run, db_path)
            }
        },
        Command::Session(cmd) => match cmd {
            SessionCmd::List(a) => {
                let con = db::open_conn(db_path, true)?;
                commands::session::cmd_session_list(
                    &con,
                    a.sort == Some(SortKey::Size),
                    a.limit,
                    a.search.as_deref(),
                )
            }
            SessionCmd::Show(a) => {
                let con = db::open_conn(db_path, true)?;
                let messages = a.messages.then(|| a.limit.unwrap_or(50));
                commands::session::cmd_session_show(&con, &a.id, messages)
            }
            SessionCmd::Delete(a) => {
                let mut con = db::open_conn(db_path, dry_run)?;
                commands::session::cmd_session_delete(&mut con, &a.ids, dry_run, db_path, service)
            }
            SessionCmd::Purge(a) => {
                let filters = PurgeFilter::try_from(a)?;
                let mut con = db::open_conn(db_path, dry_run)?;
                commands::session::cmd_session_purge(&mut con, &filters, dry_run, db_path, service)
            }
            SessionCmd::StripReasoning(a) => {
                let filters = PurgeFilter::try_from(a)?;
                let mut con = db::open_conn(db_path, dry_run)?;
                commands::session::cmd_session_strip_reasoning(&mut con, &filters, dry_run, db_path)
            }
        },
        Command::Kv(cmd) => match cmd {
            KvCmd::List(a) => {
                let con = db::open_conn(db_path, true)?;
                commands::kv::cmd_kv_list(&con, a.older_than.as_deref())
            }
            KvCmd::Show(a) => {
                let con = db::open_conn(db_path, true)?;
                commands::kv::cmd_kv_show(&con, &a.key)
            }
            KvCmd::Delete(a) => {
                let mut con = db::open_conn(db_path, dry_run)?;
                commands::kv::cmd_kv_delete(&mut con, &a.keys, dry_run, db_path)
            }
        },
        Command::Db(cmd) => match cmd {
            DbCmd::Checkpoint(a) => commands::checkpoint::cmd_checkpoint(db_path, a.truncate),
        },
        Command::Backup(a) => commands::vacuum::cmd_backup(db_path, dry_run, a.keep_backups),
        Command::Fs(cmd) => match cmd {
            FsCmd::Snapshots(a) => {
                let con = db::open_conn(db_path, true)?;
                commands::fsops::cmd_fs_clean_snapshots(
                    &con,
                    &a.projects,
                    a.orphans_only,
                    dry_run,
                    dir,
                    db_path,
                )
            }
            FsCmd::Shell(a) => {
                commands::fsops::cmd_fs_clean_shell(a.older_than.as_deref(), dry_run, dir, db_path)
            }
            FsCmd::BlobOrphans => {
                let mut con = db::open_conn(db_path, dry_run)?;
                commands::fsops::cmd_fs_clean_blob_orphans(&mut con, dry_run, db_path)
            }
            FsCmd::Log(a) => {
                commands::fsops::cmd_fs_clean_log(a.older_than.as_deref(), dry_run, dir, db_path)
            }
        },
        Command::Vacuum(a) => commands::vacuum::cmd_vacuum_cli(db_path, a, dry_run),
        Command::Cleanup(a) => {
            commands::cleanup::cmd_cleanup(dir, db_path, a, dry_run, quiet, service)
        }
        Command::SelfUpdate | Command::Completions(_) => {
            unreachable!("handled before database resolution")
        }
    }
}
