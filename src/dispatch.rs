//! Command dispatch, confirmation flow, and running-instance guards.

use crate::cli::{Cli, Command, FsCmd, KvCmd, ProjectCmd, SessionCmd, SortKey};
use crate::confirm;
use crate::db;
use crate::error::{AppError, Result};
use crate::models::{ProjectFilter, PurgeFilter};
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

    match mutation_guard(command) {
        Some((what, idle)) => with_confirmation(&cli, what, Some(idle), |dry| {
            dispatch(command, &dir, &db_path, dry, cli.quiet)
        }),
        None => dispatch(command, &dir, &db_path, false, cli.quiet),
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

/// Commands that modify data or storage, with the guard message shown
/// when opencode is running.
fn mutation_guard(cmd: &Command) -> Option<(&'static str, &'static str)> {
    match cmd {
        Command::Project(ProjectCmd::Delete(_)) => Some((
            "project delete",
            "deleting while opencode is running is not allowed",
        )),
        Command::Project(ProjectCmd::Purge(_)) => Some((
            "project purge",
            "deleting while opencode is running is not allowed",
        )),
        Command::Session(SessionCmd::Delete(_)) => Some((
            "session delete",
            "deleting while opencode is running is not allowed",
        )),
        Command::Session(SessionCmd::Purge(_)) => Some((
            "session purge",
            "deleting while opencode is running is not allowed",
        )),
        Command::Session(SessionCmd::StripReasoning(_)) => Some((
            "session strip-reasoning",
            "deleting while opencode is running is not allowed",
        )),
        Command::Kv(KvCmd::Delete(_)) => Some((
            "kv delete",
            "deleting while opencode is running is not allowed",
        )),
        Command::Backup(_) => Some(("backup", "backup needs exclusive access")),
        Command::Fs(FsCmd::Snapshots(_)) => Some((
            "fs clean-snapshots",
            "snapshots are in use while opencode runs",
        )),
        Command::Fs(FsCmd::Shell(_)) => Some((
            "fs clean-shell",
            "shell outputs are in use while opencode runs",
        )),
        Command::Fs(FsCmd::BlobOrphans) => Some((
            "fs clean-blob-orphans",
            "blob cleanup needs exclusive access",
        )),
        Command::Fs(FsCmd::Log(_)) => Some((
            "fs clean-log",
            "truncating the log while opencode runs is not allowed",
        )),
        Command::Vacuum(_) => Some(("VACUUM", "VACUUM needs exclusive access")),
        Command::Cleanup(_) => Some(("cleanup", "cleanup needs exclusive access")),
        _ => None,
    }
}

/// Confirmation rules:
/// - `--dry-run`: preview only, always allowed.
/// - `--yes`: execute for real; the running-instance guard applies.
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
                commands::session::cmd_session_delete(&mut con, &a.ids, dry_run, db_path)
            }
            SessionCmd::Purge(a) => {
                let filters = PurgeFilter::try_from(a)?;
                let mut con = db::open_conn(db_path, dry_run)?;
                commands::session::cmd_session_purge(&mut con, &filters, dry_run, db_path)
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
        Command::Cleanup(a) => commands::cleanup::cmd_cleanup(dir, db_path, a, dry_run, quiet),
        Command::SelfUpdate | Command::Completions(_) => {
            unreachable!("handled before database resolution")
        }
    }
}
