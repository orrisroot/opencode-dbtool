//! Optional user configuration (`config.toml`).
//!
//! Defaults live in `$OPENCODE_DBTOOL_CONFIG`, else
//! `$XDG_CONFIG_HOME/opencode-dbtool/config.toml`, else
//! `~/.config/opencode-dbtool/config.toml`. Command-line flags always win
//! over config values.

use crate::cli::{Cli, Command, Format, SessionCmd, SessionPurgeArgs};
use crate::error::{AppError, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Default output format (`table` or `json`).
    pub format: Option<String>,
    /// Default for `--quiet`.
    pub quiet: Option<bool>,
    /// Default for `--restart-service`.
    pub restart_service: Option<bool>,
    /// Default for `--no-input`.
    pub no_input: Option<bool>,
    /// Default age cutoff for `cleanup --fs-older-than` (default `7d`).
    pub fs_older_than: Option<String>,
    /// Defaults for `session purge` / `strip-reasoning` / `cleanup`.
    #[serde(default)]
    pub purge: PurgeDefaults,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PurgeDefaults {
    pub older_than: Option<String>,
    pub subagents: Option<bool>,
    pub archived: Option<bool>,
    pub empty: Option<bool>,
    pub larger_than: Option<String>,
    pub keep_latest: Option<i64>,
    pub keep_latest_per_project: Option<i64>,
    #[serde(default)]
    pub path: Vec<String>,
    #[serde(default)]
    pub path_prefix: Vec<String>,
}

impl Settings {
    /// Load from `explicit` (error when unreadable) or the standard
    /// location (silently absent).
    pub fn load(explicit: Option<&Path>) -> Result<Settings> {
        let path = match explicit {
            Some(path) => path.to_path_buf(),
            None => match default_path() {
                Some(path) => path,
                None => return Ok(Settings::default()),
            },
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Settings::parse(&text)
                .map_err(|e| AppError::usage(format!("invalid config {}: {e}", path.display()))),
            Err(e) if explicit.is_some() => Err(AppError::usage(format!(
                "cannot read config {}: {e}",
                path.display()
            ))),
            Err(_) => Ok(Settings::default()),
        }
    }

    fn parse(text: &str) -> Result<Settings> {
        toml::from_str(text).map_err(|e| AppError::usage(e.to_string()))
    }

    /// Fill unset CLI options from the config (flags always win).
    pub fn apply(&self, cli: &mut Cli) -> Result<()> {
        if cli.format.is_none() {
            if let Some(format) = &self.format {
                cli.format = Some(match format.as_str() {
                    "table" => Format::Table,
                    "json" => Format::Json,
                    other => {
                        return Err(AppError::usage(format!(
                            "invalid config format: {other} (expected table or json)"
                        )))
                    }
                });
            }
        }
        if !cli.quiet {
            cli.quiet = self.quiet.unwrap_or(false);
        }
        if !cli.restart_service {
            cli.restart_service = self.restart_service.unwrap_or(false);
        }
        if !cli.no_input {
            cli.no_input = self.no_input.unwrap_or(false);
        }

        if let Some(command) = cli.command.as_mut() {
            match command {
                Command::Session(SessionCmd::Purge(a) | SessionCmd::StripReasoning(a)) => {
                    self.apply_purge(a)?;
                }
                Command::Cleanup(a) => {
                    self.apply_purge(&mut a.purge)?;
                    if a.fs_older_than.is_none() {
                        a.fs_older_than = self.fs_older_than.clone();
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn apply_purge(&self, a: &mut SessionPurgeArgs) -> Result<()> {
        if a.older_than.is_none() {
            a.older_than = self.purge.older_than.clone();
        }
        if !a.subagents {
            a.subagents = self.purge.subagents.unwrap_or(false);
        }
        if !a.archived {
            a.archived = self.purge.archived.unwrap_or(false);
        }
        if !a.empty {
            a.empty = self.purge.empty.unwrap_or(false);
        }
        if a.larger_than.is_none() {
            a.larger_than = self.purge.larger_than.clone();
        }
        if a.keep_latest.is_none() {
            a.keep_latest = self.purge.keep_latest;
        }
        if a.keep_latest_per_project.is_none() {
            a.keep_latest_per_project = self.purge.keep_latest_per_project;
        }
        if a.paths.is_empty() {
            a.paths = self.purge.path.clone();
        }
        if a.path_prefixes.is_empty() {
            a.path_prefixes = self.purge.path_prefix.clone();
        }
        if a.keep_latest.is_some() && a.keep_latest_per_project.is_some() {
            return Err(AppError::usage(
                "--keep-latest and --keep-latest-per-project are mutually exclusive \
                 (check the config file)",
            ));
        }
        Ok(())
    }
}

/// Standard config location.
pub fn default_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("OPENCODE_DBTOOL_CONFIG") {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("opencode-dbtool/config.toml"));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".config/opencode-dbtool/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse_cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    fn purge_of(cli: &Cli) -> &SessionPurgeArgs {
        match cli.command.as_ref().unwrap() {
            Command::Session(SessionCmd::Purge(a)) => a,
            _ => panic!("expected session purge"),
        }
    }

    #[test]
    fn config_defaults_fill_unset_options() {
        let settings = Settings::parse(
            r#"
            format = "table"
            quiet = true
            restart_service = true
            fs_older_than = "14d"

            [purge]
            older_than = "30d"
            subagents = true
            keep_latest_per_project = 5
            path = ["/work/a"]
            "#,
        )
        .unwrap();

        let mut cli = parse_cli(&["opencode-dbtool", "session", "purge"]);
        settings.apply(&mut cli).unwrap();
        assert_eq!(cli.format, Some(Format::Table));
        assert!(cli.quiet);
        assert!(cli.restart_service);
        let a = purge_of(&cli);
        assert_eq!(a.older_than.as_deref(), Some("30d"));
        assert!(a.subagents);
        assert_eq!(a.keep_latest_per_project, Some(5));
        assert_eq!(a.paths, vec!["/work/a"]);

        let mut cli = parse_cli(&["opencode-dbtool", "cleanup"]);
        settings.apply(&mut cli).unwrap();
        match cli.command.as_ref().unwrap() {
            Command::Cleanup(a) => {
                assert_eq!(a.fs_older_than.as_deref(), Some("14d"));
                assert_eq!(a.purge.older_than.as_deref(), Some("30d"));
            }
            _ => panic!("expected cleanup"),
        }
    }

    #[test]
    fn command_line_flags_win_over_config() {
        let settings = Settings::parse(
            r#"
            format = "json"

            [purge]
            older_than = "30d"
            "#,
        )
        .unwrap();

        let mut cli = parse_cli(&[
            "opencode-dbtool",
            "session",
            "purge",
            "--older-than",
            "7d",
            "--format",
            "table",
        ]);
        settings.apply(&mut cli).unwrap();
        assert_eq!(cli.format, Some(Format::Table));
        assert_eq!(purge_of(&cli).older_than.as_deref(), Some("7d"));
    }

    #[test]
    fn conflicting_retention_options_are_rejected() {
        let settings = Settings::parse(
            r#"
            [purge]
            keep_latest = 1
            keep_latest_per_project = 1
            "#,
        )
        .unwrap();
        let mut cli = parse_cli(&["opencode-dbtool", "session", "purge"]);
        let err = settings.apply(&mut cli).unwrap_err();
        assert_eq!(err.code, 2);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(Settings::parse("nope = 1").is_err());
    }
}
