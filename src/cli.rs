//! Command-line definition (clap): the full command tree with per-command
//! help, typo suggestions, and value validation.
//!
//! Value parsers validate `--older-than` / `--larger-than` / count flags at
//! parse time; the raw strings are kept so the JSON output mirrors the
//! arguments as given.

use crate::error::{AppError, Result};
use crate::models::{ProjectFilter, PurgeFilter};
use crate::util::{now_ms, parse_age_ms, parse_count, parse_size_bytes};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// Output format; `auto` (the default when the flag is absent) picks a
/// table on a terminal and JSON when stdout is piped.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Table,
    Json,
}

#[derive(Parser)]
#[command(
    name = "opencode-dbtool",
    version,
    about = "Maintenance tool for the opencode 2.x SQLite database",
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Preview actions without changing anything (safe while opencode runs)
    #[arg(long, short = 'n', global = true)]
    pub dry_run: bool,

    /// Confirm a destructive command. On a terminal, omitting both flags
    /// shows a preview and asks for confirmation instead.
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,

    /// Output format (default: table on a terminal, json when piped)
    #[arg(long, value_enum, global = true, value_name = "FORMAT")]
    pub format: Option<Format>,

    /// Suppress progress and confirmation messages on stderr
    #[arg(long, global = true)]
    pub quiet: bool,

    /// Stop the opencode service before maintenance and restart it after
    /// (applies to commands that otherwise require opencode to be closed)
    #[arg(long, global = true)]
    pub restart_service: bool,

    /// Never prompt; destructive commands then require --yes
    #[arg(long, global = true)]
    pub no_input: bool,

    /// Load defaults from this config file instead of the standard location
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Print absolute timestamps in tables instead of relative ages
    #[arg(long, global = true)]
    pub absolute: bool,

    /// Disable colors in table output (also honors NO_COLOR)
    #[arg(long, global = true)]
    pub no_color: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Database overview: table sizes, totals, storage usage
    Stats(StatsArgs),
    /// Integrity and consistency checks (`--fix` repairs what it can)
    Doctor(DoctorArgs),
    /// Project commands (list, show, delete, purge)
    #[command(subcommand)]
    Project(ProjectCmd),
    /// Session commands (list, show, delete, purge, strip-reasoning)
    #[command(subcommand)]
    Session(SessionCmd),
    /// Global kv cache entries (list, show, delete)
    #[command(subcommand)]
    Kv(KvCmd),
    /// Database housekeeping (WAL checkpoint)
    #[command(subcommand)]
    Db(DbCmd),
    /// Create a verified timestamped backup of the database
    Backup(BackupArgs),
    /// Filesystem storage cleanup (snapshots, shell, blobs, log)
    #[command(subcommand)]
    Fs(FsCmd),
    /// Inspect the running opencode service
    #[command(subcommand)]
    Service(ServiceCmd),
    /// Compact the database, reclaiming space freed by deletes
    Vacuum(VacuumArgs),
    /// Read-only suggestions for reclaiming space (never deletes)
    Report,
    /// Update the binary from the latest GitHub release
    SelfUpdate,
    /// Remove old sessions and stale files/storage in one run
    Cleanup(CleanupArgs),
    /// Generate a shell completion script
    Completions(CompletionsArgs),
}

// ---------------------------------------------------------------------------
// stats / doctor
// ---------------------------------------------------------------------------

#[derive(Args)]
pub struct StatsArgs {
    /// Add message-type breakdown, 30-day activity, and subagent shares
    #[arg(long)]
    pub detail: bool,
}

#[derive(Args)]
pub struct DoctorArgs {
    /// Repair what can be repaired: orphan blobs/events and dangling
    /// parent/fork/workspace references
    #[arg(long)]
    pub fix: bool,
}

// ---------------------------------------------------------------------------
// project
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum ProjectCmd {
    /// Project overview (counts, sizes)
    List(ProjectListArgs),
    /// Project detail (sessions, breakdown)
    Show(ProjectShowArgs),
    /// Delete project(s) and all related data
    Delete(DeleteIdsArgs),
    /// Delete projects matching all filters
    Purge(ProjectPurgeArgs),
}

#[derive(Args)]
pub struct ProjectListArgs {
    /// Only projects registered at this directory (repeatable)
    #[arg(long = "path", value_name = "DIR")]
    pub paths: Vec<String>,
    /// Sort key (default: worktree)
    #[arg(long, value_enum, value_name = "KEY")]
    pub sort: Option<ProjectSort>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum ProjectSort {
    Size,
    Updated,
    Sessions,
}

#[derive(Args)]
pub struct ProjectShowArgs {
    /// Project id, unique id prefix, or worktree directory
    #[arg(value_name = "ID")]
    pub id: String,
}

#[derive(Args)]
pub struct ProjectPurgeArgs {
    /// Projects whose latest session activity is older than the age (e.g. 30d)
    #[arg(long, value_name = "AGE", value_parser = age_value)]
    pub older_than: Option<String>,
    /// Projects at this directory (repeatable, exact worktree match)
    #[arg(long = "path", value_name = "DIR")]
    pub paths: Vec<String>,
    /// Only projects with no sessions
    #[arg(long)]
    pub empty: bool,
}

impl TryFrom<&ProjectPurgeArgs> for ProjectFilter {
    type Error = AppError;

    fn try_from(a: &ProjectPurgeArgs) -> Result<Self> {
        let mut f = ProjectFilter {
            older_than_raw: a.older_than.clone(),
            cutoff_ms: None,
            paths: a.paths.clone(),
            empty: a.empty,
        };
        if let Some(age) = &a.older_than {
            f.cutoff_ms = Some(now_ms()? - parse_age_ms(age)?);
        }
        Ok(f)
    }
}

// ---------------------------------------------------------------------------
// session
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum SessionCmd {
    /// Per-session breakdown (full ids)
    List(SessionListArgs),
    /// Session detail, optionally with message previews
    Show(SessionShowArgs),
    /// Delete session(s), cascading to children
    Delete(DeleteIdsArgs),
    /// Delete sessions matching all filters
    Purge(SessionPurgeArgs),
    /// Delete only the reasoning content of matching sessions
    StripReasoning(SessionPurgeArgs),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum SortKey {
    Size,
    Cost,
    Updated,
    Messages,
}

#[derive(Args)]
pub struct SessionListArgs {
    /// Sort key (default: time_updated descending)
    #[arg(long, value_enum, value_name = "KEY")]
    pub sort: Option<SortKey>,
    /// Maximum number of sessions to print
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
    /// Only sessions whose title or directory contains this text
    #[arg(long, value_name = "TEXT")]
    pub search: Option<String>,
    /// Only sessions at least this large (e.g. 50M)
    #[arg(long = "min-size", value_name = "SIZE", value_parser = size_value)]
    pub min_size: Option<String>,
}

#[derive(Args)]
pub struct SessionShowArgs {
    /// Session id or unique id prefix
    #[arg(value_name = "ID")]
    pub id: String,
    /// Include message previews (oldest first)
    #[arg(long)]
    pub messages: bool,
    /// Maximum number of message previews (requires --messages)
    #[arg(long, value_name = "N", requires = "messages")]
    pub limit: Option<usize>,
    /// Do not truncate message previews (requires --messages)
    #[arg(long, requires = "messages")]
    pub full: bool,
}

#[derive(Args)]
pub struct SessionPurgeArgs {
    /// Sessions with `time_updated` older than the age (e.g. 30d, 12h, 2w)
    #[arg(long, value_name = "AGE", value_parser = age_value)]
    pub older_than: Option<String>,
    /// Only subagent sessions
    #[arg(long)]
    pub subagents: bool,
    /// Only archived sessions
    #[arg(long)]
    pub archived: bool,
    /// Only sessions with no content rows
    #[arg(long)]
    pub empty: bool,
    /// Sessions in this directory (repeatable, exact match)
    #[arg(long = "path", value_name = "DIR")]
    pub paths: Vec<String>,
    /// Sessions in this directory or below it (repeatable)
    #[arg(long = "path-prefix", value_name = "DIR")]
    pub path_prefixes: Vec<String>,
    /// Sessions whose own size is larger than this (e.g. 50M)
    #[arg(long = "larger-than", value_name = "SIZE", value_parser = size_value)]
    pub larger_than: Option<String>,
    /// Keep the newest N matching sessions, purge the rest
    #[arg(
        long = "keep-latest",
        value_name = "N",
        value_parser = count_value,
        conflicts_with = "keep_latest_per_project"
    )]
    pub keep_latest: Option<i64>,
    /// Keep the newest N matching sessions per project
    #[arg(
        long = "keep-latest-per-project",
        value_name = "N",
        value_parser = count_value
    )]
    pub keep_latest_per_project: Option<i64>,
}

impl TryFrom<&SessionPurgeArgs> for PurgeFilter {
    type Error = AppError;

    fn try_from(a: &SessionPurgeArgs) -> Result<Self> {
        let mut f = PurgeFilter {
            older_than_raw: a.older_than.clone(),
            cutoff_ms: None,
            subagents: a.subagents,
            paths: a.paths.clone(),
            larger_than_raw: a.larger_than.clone(),
            larger_than_bytes: None,
            keep_latest_raw: a.keep_latest.map(|n| n.to_string()),
            keep_latest: a.keep_latest,
            keep_latest_per_project_raw: a.keep_latest_per_project.map(|n| n.to_string()),
            keep_latest_per_project: a.keep_latest_per_project,
            archived: a.archived,
            path_prefixes: a.path_prefixes.clone(),
            empty: a.empty,
        };
        if let Some(age) = &a.older_than {
            f.cutoff_ms = Some(now_ms()? - parse_age_ms(age)?);
        }
        if let Some(size) = &a.larger_than {
            f.larger_than_bytes = Some(parse_size_bytes(size)?);
        }
        Ok(f)
    }
}

// ---------------------------------------------------------------------------
// kw
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum KvCmd {
    /// List kv entries with sizes, largest first
    List(KvListArgs),
    /// Show a kv value (truncated)
    Show(KvShowArgs),
    /// Delete kv entries (caches regenerate on demand)
    Delete(KvDeleteArgs),
}

#[derive(Args)]
pub struct KvListArgs {
    /// Only entries not updated since the cutoff
    #[arg(long, value_name = "AGE", value_parser = age_value)]
    pub older_than: Option<String>,
}

#[derive(Args)]
pub struct KvShowArgs {
    /// Exact kv key
    #[arg(value_name = "KEY")]
    pub key: String,
    /// Print the value verbatim (no JSON wrapper, no truncation)
    #[arg(long)]
    pub raw: bool,
}

#[derive(Args)]
pub struct KvDeleteArgs {
    /// One or more exact kv keys
    #[arg(required = true, value_name = "KEY")]
    pub keys: Vec<String>,
}

// ---------------------------------------------------------------------------
// db
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum DbCmd {
    /// Checkpoint the WAL (PASSIVE by default; --truncate shrinks the file)
    Checkpoint(DbCheckpointArgs),
}

#[derive(Args)]
pub struct DbCheckpointArgs {
    /// Truncate the WAL file after checkpointing (needs no active readers)
    #[arg(long)]
    pub truncate: bool,
}

// ---------------------------------------------------------------------------
// backup / vacuum
// ---------------------------------------------------------------------------

#[derive(Args)]
pub struct BackupArgs {
    /// List backups or restore one (default: create a backup)
    #[command(subcommand)]
    pub command: Option<BackupCmd>,
    /// Keep only the newest N backup files after a successful run
    #[arg(
        long = "keep-backups",
        value_name = "N",
        value_parser = keep_backups_value
    )]
    pub keep_backups: Option<i64>,
}

#[derive(Subcommand)]
pub enum BackupCmd {
    /// List timestamped backups, newest first
    List(BackupListArgs),
    /// Restore a backup over the current database
    Restore(BackupRestoreArgs),
}

#[derive(Args)]
pub struct BackupListArgs {
    /// Verify every backup with an integrity check (slower)
    #[arg(long)]
    pub verify: bool,
}

#[derive(Args)]
pub struct BackupRestoreArgs {
    /// Backup file to restore (as listed by `backup list`)
    #[arg(value_name = "FILE")]
    pub file: PathBuf,
}

// ---------------------------------------------------------------------------
// service
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum ServiceCmd {
    /// Show registration, API reachability, and database match
    Status,
}

#[derive(Args)]
pub struct VacuumArgs {
    /// Skip the timestamped backup (dangerous)
    #[arg(long, conflicts_with = "keep_backups")]
    pub no_backup: bool,
    /// Keep only the newest N backups after a successful run
    #[arg(
        long = "keep-backups",
        value_name = "N",
        value_parser = keep_backups_value
    )]
    pub keep_backups: Option<i64>,
    /// Attempt VACUUM while opencode runs (may block the server briefly)
    #[arg(long)]
    pub online: bool,
}

// ---------------------------------------------------------------------------
// fs
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum FsCmd {
    /// Delete snapshot storage (undo/redo history)
    #[command(name = "clean-snapshots")]
    Snapshots(FsSnapshotsArgs),
    /// Delete old shell output files
    #[command(name = "clean-shell")]
    Shell(FsShellArgs),
    /// Delete instruction blobs referenced by no state
    #[command(name = "clean-blob-orphans")]
    BlobOrphans,
    /// Truncate or prune log/opencode.log
    #[command(name = "clean-log")]
    Log(FsLogArgs),
}

#[derive(Args)]
pub struct FsSnapshotsArgs {
    /// Limit deletion to these project ids (repeatable)
    #[arg(long = "project", value_name = "ID")]
    pub projects: Vec<String>,
    /// Delete only directories whose project no longer exists
    #[arg(long)]
    pub orphans_only: bool,
}

#[derive(Args)]
pub struct FsShellArgs {
    /// Only remove outputs not modified since the cutoff
    #[arg(long, value_name = "AGE", value_parser = age_value)]
    pub older_than: Option<String>,
}

#[derive(Args)]
pub struct FsLogArgs {
    /// Drop only lines older than the cutoff (default: truncate all)
    #[arg(long, value_name = "AGE", value_parser = age_value)]
    pub older_than: Option<String>,
}

// ---------------------------------------------------------------------------
// backup-style shared argument
// ---------------------------------------------------------------------------

#[derive(Args)]
pub struct DeleteIdsArgs {
    /// One or more ids
    #[arg(required = true, value_name = "ID")]
    pub ids: Vec<String>,
}

// ---------------------------------------------------------------------------
// cleanup
// ---------------------------------------------------------------------------

#[derive(Args)]
pub struct CleanupArgs {
    #[command(flatten)]
    pub purge: SessionPurgeArgs,
    /// Age cutoff for shell/log cleanup
    #[arg(long = "fs-older-than", value_name = "AGE", value_parser = age_value)]
    pub fs_older_than: Option<String>,
    /// Skip the pre-cleanup database backup
    #[arg(long)]
    pub no_backup: bool,
    /// Skip the final VACUUM
    #[arg(long)]
    pub no_vacuum: bool,
    /// Keep only the newest N backup files after a successful run
    #[arg(
        long = "keep-backups",
        value_name = "N",
        value_parser = keep_backups_value,
        conflicts_with = "no_backup"
    )]
    pub keep_backups: Option<i64>,
}

// ---------------------------------------------------------------------------
// completions
// ---------------------------------------------------------------------------

#[derive(Args)]
pub struct CompletionsArgs {
    /// Shell to generate a completion script for
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

// ---------------------------------------------------------------------------
// value parsers
// ---------------------------------------------------------------------------

fn age_value(s: &str) -> std::result::Result<String, String> {
    parse_age_ms(s)
        .map(|_| s.to_string())
        .map_err(|e| e.message)
}

fn size_value(s: &str) -> std::result::Result<String, String> {
    parse_size_bytes(s)
        .map(|_| s.to_string())
        .map_err(|e| e.message)
}

fn count_value(s: &str) -> std::result::Result<i64, String> {
    parse_count(s).map_err(|e| e.message)
}

fn keep_backups_value(s: &str) -> std::result::Result<i64, String> {
    let n = count_value(s)?;
    if n == 0 {
        return Err("must be at least 1 (0 would delete the backup just created)".to_string());
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::PurgeFilter;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    #[test]
    fn global_flags_are_accepted_after_subcommands() {
        let cli = parse(&["opencode-dbtool", "session", "delete", "ses_1", "-n"]);
        assert!(cli.dry_run);
        assert!(!cli.yes);
        let cli = parse(&["opencode-dbtool", "-y", "vacuum"]);
        assert!(cli.yes);
        let cli = parse(&["opencode-dbtool", "stats", "--format", "table"]);
        assert_eq!(cli.format, Some(Format::Table));
        let cli = parse(&["opencode-dbtool", "vacuum", "--restart-service"]);
        assert!(cli.restart_service);
    }

    #[test]
    fn unknown_options_are_rejected() {
        assert!(Cli::try_parse_from(["opencode-dbtool", "stats", "--detail", "--nope"]).is_err());
        assert!(
            Cli::try_parse_from(["opencode-dbtool", "session", "purge", "--subagents", "x"])
                .is_err()
        );
    }

    #[test]
    fn purge_parser_rejects_bad_ages_sizes_and_counts() {
        for args in [
            vec!["opencode-dbtool", "session", "purge", "--older-than"],
            vec!["opencode-dbtool", "session", "purge", "--older-than", "xyz"],
            vec![
                "opencode-dbtool",
                "session",
                "purge",
                "--larger-than",
                "1.5M",
            ],
            vec!["opencode-dbtool", "session", "purge", "--keep-latest", "-1"],
            vec![
                "opencode-dbtool",
                "session",
                "purge",
                "--keep-latest",
                "1",
                "--keep-latest-per-project",
                "1",
            ],
        ] {
            assert!(
                Cli::try_parse_from(&args).is_err(),
                "should reject: {args:?}"
            );
        }
    }

    #[test]
    fn purge_filter_conversion_keeps_raw_and_parsed_values() {
        let cli = parse(&[
            "opencode-dbtool",
            "session",
            "purge",
            "--older-than",
            "30d",
            "--larger-than",
            "50M",
            "--path",
            "/a",
            "--path-prefix",
            "/b",
            "--archived",
            "--keep-latest",
            "2",
        ]);
        let Some(Command::Session(SessionCmd::Purge(a))) = cli.command else {
            panic!("expected session purge");
        };
        let f = PurgeFilter::try_from(&a).unwrap();
        assert_eq!(f.older_than_raw.as_deref(), Some("30d"));
        assert!(f.cutoff_ms.is_some());
        assert_eq!(f.larger_than_raw.as_deref(), Some("50M"));
        assert_eq!(f.larger_than_bytes, Some(50 * 1024 * 1024));
        assert_eq!(f.paths, vec!["/a"]);
        assert_eq!(f.path_prefixes, vec!["/b"]);
        assert!(f.archived);
        assert_eq!(f.keep_latest, Some(2));
        assert!(!f.is_empty());
    }

    #[test]
    fn backup_flags_conflict() {
        assert!(Cli::try_parse_from([
            "opencode-dbtool",
            "vacuum",
            "--no-backup",
            "--keep-backups",
            "1",
        ])
        .is_err());
        assert!(Cli::try_parse_from(["opencode-dbtool", "vacuum", "--keep-backups", "0"]).is_err());
    }

    #[test]
    fn list_requires_ids() {
        assert!(Cli::try_parse_from(["opencode-dbtool", "session", "delete"]).is_err());
        assert!(Cli::try_parse_from(["opencode-dbtool", "kv", "delete"]).is_err());
    }

    #[test]
    fn show_limit_requires_messages() {
        assert!(Cli::try_parse_from([
            "opencode-dbtool",
            "session",
            "show",
            "ses_1",
            "--limit",
            "5"
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "opencode-dbtool",
            "session",
            "show",
            "ses_1",
            "--messages",
            "--limit",
            "5",
        ])
        .is_ok());
    }

    #[test]
    fn list_and_show_options_parse() {
        assert!(Cli::try_parse_from([
            "opencode-dbtool",
            "session",
            "list",
            "--sort",
            "cost",
            "--min-size",
            "1M"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "opencode-dbtool",
            "session",
            "show",
            "ses_1",
            "--messages",
            "--full"
        ])
        .is_ok());
        // --full requires --messages.
        assert!(
            Cli::try_parse_from(["opencode-dbtool", "session", "show", "ses_1", "--full"]).is_err()
        );
        assert!(
            Cli::try_parse_from(["opencode-dbtool", "project", "list", "--sort", "size"]).is_ok()
        );
        assert!(Cli::try_parse_from(["opencode-dbtool", "kv", "show", "k", "--raw"]).is_ok());
        assert!(
            Cli::try_parse_from(["opencode-dbtool", "stats", "--absolute", "--no-color"]).is_ok()
        );
    }

    #[test]
    fn cleanup_defaults() {
        let cli = parse(&["opencode-dbtool", "cleanup"]);
        let Some(Command::Cleanup(a)) = cli.command else {
            panic!("expected cleanup");
        };
        assert!(
            a.fs_older_than.is_none(),
            "resolved to the 7d default at runtime"
        );
        assert!(!a.no_backup);
        assert!(!a.no_vacuum);
        let f = PurgeFilter::try_from(&a.purge).unwrap();
        assert!(
            f.is_empty(),
            "cleanup without session filters purges nothing"
        );
    }

    #[test]
    fn invalid_subcommands_list_suggestions() {
        let err = match Cli::try_parse_from(["opencode-dbtool", "sessions"]) {
            Err(e) => e,
            Ok(_) => panic!("expected a parse error"),
        };
        let text = err.to_string();
        assert!(text.contains("sessions"), "got: {text}");
    }
}
