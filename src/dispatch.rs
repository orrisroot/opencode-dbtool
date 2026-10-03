//! Command dispatch, confirmation flow, and running-instance guards.

use crate::cli::{
    BackupCmd, Cli, Command, DbCmd, FsCmd, KvCmd, ProjectCmd, ServiceCmd, SessionCmd,
};
use crate::confirm;
use crate::db;
use crate::error::{AppError, Result};
use crate::models::{ProjectFilter, PurgeFilter};
use crate::service::{self, ServiceInfo};
use crate::util::shell_quote;
use crate::{commands, config, output, sys};
use clap::CommandFactory;
use std::io::IsTerminal;
use std::path::Path;

/// Entry point after clap parsing: set up output, then either handle the
/// no-database commands or run dispatch behind the confirmation guard.
pub fn execute(mut cli: Cli) -> Result<()> {
    let settings = crate::settings::Settings::load(cli.config.as_deref())?;
    settings.apply(&mut cli)?;

    let Some(command) = cli.command.as_ref() else {
        print_help();
        return Ok(());
    };
    output::set_format(output::effective_format(cli.format));
    output::set_fields(cli.fields.clone());
    output::set_pager(!cli.no_pager && std::io::stdout().is_terminal());
    output::set_relative(!cli.absolute);
    output::set_quiet(cli.quiet);
    let color = std::io::stdout().is_terminal()
        && output::table_mode()
        && !cli.no_color
        && std::env::var_os("NO_COLOR").is_none();
    output::set_color(color);

    if let Command::Completions(a) = command {
        let mut cmd = Cli::command();
        clap_complete::generate(a.shell, &mut cmd, "opencode-dbtool", &mut std::io::stdout());
        return Ok(());
    }
    if matches!(command, Command::SelfUpdate) {
        return run_guarded(&cli, "self-update", None, None, false, None, |dry| {
            commands::selfupdate::cmd_self_update(dry)
        });
    }

    let Some(dir) = config::data_dir() else {
        return Err(AppError::usage("cannot determine opencode data dir"));
    };
    let db_path = config::db_path()?;
    // `service status` diagnoses even when the database is missing.
    if let Command::Service(ServiceCmd::Status) = command {
        return commands::service::cmd_service_status(&db_path);
    }
    require_db(&db_path)?;
    let service = service::discover();
    let restart = cli.restart_service;

    match mutation_guard(command, service.as_ref(), &db_path, restart) {
        Some((what, idle)) => {
            // Stop the service only when one is actually registered; with
            // none, fall through to the normal running-instance guard.
            let stop_service = restart && service.is_some() && idle.is_some();
            // When the service is stopped for maintenance, run the direct
            // (offline) path instead of the server API.
            let service_for_run = if stop_service { None } else { service.as_ref() };
            run_guarded(
                &cli,
                what,
                idle,
                service.as_ref(),
                stop_service,
                Some(&db_path),
                |dry| dispatch(command, &dir, &db_path, dry, cli.quiet, service_for_run),
            )
        }
        None => dispatch(
            command,
            &dir,
            &db_path,
            cli.dry_run,
            cli.quiet,
            service.as_ref(),
        ),
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
/// session deletes routed through the running server). `restart` forces
/// the offline path: the service is stopped first, so the API route and
/// the online exceptions do not apply.
fn mutation_guard<'a>(
    cmd: &Command,
    service: Option<&ServiceInfo>,
    db_path: &Path,
    restart: bool,
) -> Option<(&'static str, Option<&'a str>)> {
    // Session deletes can go through the server API when the service
    // operates on the same database file.
    let api_online = !restart && service.is_some_and(|s| s.targets_db(db_path));
    match cmd {
        Command::Doctor(a) if a.fix => Some((
            "doctor --fix",
            Some("repairing while opencode is running is not allowed"),
        )),
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
        // Listing backups is read-only; creating one uses the online
        // backup API, and restoring needs exclusive access.
        Command::Backup(a) => match &a.command {
            Some(BackupCmd::List(_)) => None,
            Some(BackupCmd::Restore(_)) => Some((
                "backup restore",
                Some("restoring while opencode is running is not allowed"),
            )),
            None => Some(("backup", None)),
        },
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

/// Confirmation and guard rules:
/// - `--dry-run`: preview only, always allowed (never stops the service).
/// - `--yes`: execute for real.
/// - neither, on a terminal: preview, ask `Proceed? [y/N]`, then execute.
/// - neither, not a terminal: usage error (exit 2) as before.
///
/// For a real run, the running-instance guard applies unless the command
/// is safe online. With `--restart-service` the registered service is
/// stopped first and restarted afterwards (even on failure).
#[allow(clippy::too_many_arguments)]
fn run_guarded<F>(
    cli: &Cli,
    what: &str,
    idle: Option<&str>,
    service: Option<&ServiceInfo>,
    restart: bool,
    db_path: Option<&Path>,
    mut run: F,
) -> Result<()>
where
    F: FnMut(bool) -> Result<()>,
{
    if cli.dry_run {
        let result = run(true);
        if result.is_ok() && !cli.quiet {
            if let Some(command) = suggested_apply_command(&std::env::args().collect::<Vec<_>>()) {
                eprintln!("to apply: {command}");
            }
        }
        return result;
    }
    if cli.yes {
        return execute_confirmed(cli, idle, service, restart, db_path, &mut run);
    }
    let interactive =
        !cli.no_input && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !interactive {
        return Err(AppError::usage(format!(
            "{what} modifies data; pass `--dry-run` to preview the impact or `--yes` to confirm"
        )));
    }
    run(true)?;
    if confirm::ask("Proceed?")? {
        execute_confirmed(cli, idle, service, restart, db_path, &mut run)
    } else {
        if !cli.quiet {
            eprintln!("aborted");
        }
        Ok(())
    }
}

fn execute_confirmed<F>(
    cli: &Cli,
    idle: Option<&str>,
    service: Option<&ServiceInfo>,
    restart: bool,
    db_path: Option<&Path>,
    run: &mut F,
) -> Result<()>
where
    F: FnMut(bool) -> Result<()>,
{
    // One real maintenance run at a time, across processes (cron + manual).
    let _lock = crate::lock::ToolLock::acquire()?;
    if restart {
        let svc = service.ok_or_else(|| {
            AppError::usage(
                "--restart-service: no running opencode service was found; start it with \
                 `opencode service start` or stop opencode manually",
            )
        })?;
        let mut guard = service::ServiceRestart::stop(svc)?;
        // Another process (e.g. a v1 instance) could still hold the
        // database open; refuse rather than run maintenance behind it.
        if let Some(db_path) = db_path {
            let others = service::processes_with_db_open(db_path, &[svc.pid]);
            if !others.is_empty() {
                let _ = guard.restart();
                return Err(AppError::busy(format!(
                    "other opencode processes still have the database open (pid={}) - stop them and retry",
                    others
                        .iter()
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                )));
            }
        }
        if !cli.quiet {
            eprintln!("opencode service stopped (pid {})", guard.pid());
        }
        let result = run(false);
        let restart_result = guard.restart();
        return match (result, restart_result) {
            (Ok(()), Ok(())) => {
                if !cli.quiet {
                    eprintln!("opencode service restarted");
                }
                Ok(())
            }
            (Err(e), Ok(())) => Err(e),
            (Ok(()), Err(e)) => Err(e),
            (Err(e), Err(restart_error)) => Err(AppError::db(format!(
                "{e}; additionally, restarting opencode failed: {restart_error}"
            ))),
        };
    }
    if let Some(message) = idle {
        sys::require_idle(message)?;
    }
    run(false)
}

/// The current command line with `--dry-run` removed and `--yes` added,
/// printed after a preview so the confirmed command can be copied.
fn suggested_apply_command(args: &[String]) -> Option<String> {
    let (program, rest) = args.split_first()?;
    let mut out = vec![shell_quote(program)];
    let mut has_yes = false;
    for arg in rest {
        match arg.as_str() {
            "--dry-run" | "-n" => continue,
            "--yes" | "-y" => {
                has_yes = true;
                out.push(arg.clone());
            }
            _ => {
                if let Some((rewritten, contains_yes)) = rewrite_short_cluster(arg) {
                    has_yes |= contains_yes;
                    if !rewritten.is_empty() {
                        out.push(rewritten);
                    }
                } else {
                    out.push(crate::util::shell_quote(arg));
                }
            }
        }
    }
    if !has_yes {
        out.push("--yes".to_string());
    }
    Some(out.join(" "))
}

/// Strip `n` from a combined short-flag cluster (`-ny` -> `-y`). Returns
/// `None` when the argument is not a cluster; an empty rewritten string
/// means the whole argument should be dropped.
fn rewrite_short_cluster(arg: &str) -> Option<(String, bool)> {
    if !arg.starts_with('-') || arg.starts_with("--") || arg.chars().count() <= 2 {
        return None;
    }
    let mut kept = String::new();
    let mut has_yes = false;
    for c in arg[1..].chars() {
        match c {
            'n' => {}
            'y' => {
                has_yes = true;
                kept.push(c);
            }
            other => kept.push(other),
        }
    }
    let rewritten = if kept.is_empty() {
        String::new()
    } else {
        format!("-{kept}")
    };
    Some((rewritten, has_yes))
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
        Command::Doctor(a) => {
            let read_only = !a.fix || dry_run;
            let mut con = db::open_conn(db_path, read_only)?;
            commands::doctor::cmd_doctor(&mut con, db_path, a.fix, dry_run)
        }
        Command::Project(cmd) => match cmd {
            ProjectCmd::List(a) => {
                let con = db::open_conn(db_path, true)?;
                commands::project::cmd_project_list(&con, &a.paths, a.sort)
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
                let min_size = a
                    .min_size
                    .as_deref()
                    .map(crate::util::parse_size_bytes)
                    .transpose()?;
                let con = db::open_conn(db_path, true)?;
                commands::session::cmd_session_list(
                    &con,
                    a.sort,
                    a.limit,
                    a.search.as_deref(),
                    min_size,
                )
            }
            SessionCmd::Show(a) => {
                let con = db::open_conn(db_path, true)?;
                let messages = a.messages.then(|| a.limit.unwrap_or(50));
                commands::session::cmd_session_show(&con, &a.id, messages, a.full)
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
                commands::kv::cmd_kv_show(&con, &a.key, a.raw)
            }
            KvCmd::Delete(a) => {
                let mut con = db::open_conn(db_path, dry_run)?;
                commands::kv::cmd_kv_delete(&mut con, &a.keys, dry_run, db_path)
            }
        },
        Command::Db(cmd) => match cmd {
            DbCmd::Checkpoint(a) => {
                commands::checkpoint::cmd_checkpoint(db_path, a.truncate, dry_run)
            }
        },
        Command::Backup(a) => {
            if a.keep_backups.is_some() && a.command.is_some() {
                return Err(AppError::usage(
                    "--keep-backups only applies when creating a backup",
                ));
            }
            match &a.command {
                Some(BackupCmd::List(l)) => commands::vacuum::cmd_backup_list(db_path, l.verify),
                Some(BackupCmd::Restore(r)) => {
                    commands::vacuum::cmd_restore(db_path, &r.file, dry_run)
                }
                None => commands::vacuum::cmd_backup(db_path, dry_run, a.keep_backups),
            }
        }
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
        Command::Report => {
            let con = db::open_conn(db_path, true)?;
            commands::report::cmd_report(&con, db_path)
        }
        Command::Cleanup(a) => {
            commands::cleanup::cmd_cleanup(dir, db_path, a, dry_run, quiet, service)
        }
        Command::SelfUpdate | Command::Completions(_) | Command::Service(_) => {
            unreachable!("handled before database resolution")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn suggested_command_drops_dry_run_and_adds_yes() {
        let out = suggested_apply_command(&args(&[
            "opencode-dbtool",
            "session",
            "purge",
            "--older-than",
            "30d",
            "--dry-run",
        ]))
        .unwrap();
        assert_eq!(out, "opencode-dbtool session purge --older-than 30d --yes");
    }

    #[test]
    fn suggested_command_keeps_existing_yes_and_quotes_spaces() {
        let out = suggested_apply_command(&args(&[
            "opencode-dbtool",
            "session",
            "purge",
            "--path",
            "/work/my dir",
            "-y",
        ]))
        .unwrap();
        assert_eq!(
            out,
            "opencode-dbtool session purge --path '/work/my dir' -y"
        );
    }

    #[test]
    fn suggested_command_needs_a_program() {
        assert!(suggested_apply_command(&[]).is_none());
    }

    #[test]
    fn suggested_command_handles_combined_short_flags() {
        let out = suggested_apply_command(&args(&["opencode-dbtool", "session", "purge", "-ny"]))
            .unwrap();
        assert_eq!(out, "opencode-dbtool session purge -y");
        let out = suggested_apply_command(&args(&["opencode-dbtool", "vacuum", "-yn"])).unwrap();
        assert_eq!(out, "opencode-dbtool vacuum -y");
        let out = suggested_apply_command(&args(&["opencode-dbtool", "vacuum", "-n"])).unwrap();
        assert_eq!(out, "opencode-dbtool vacuum --yes");
    }
}
