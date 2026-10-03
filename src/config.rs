//! Data-directory and database-path resolution.

use crate::error::{AppError, Result};
use std::env;
use std::path::{Path, PathBuf};

/// Resolve the opencode data dir: `$OPENCODE_DATA_DIR`,
/// `$XDG_DATA_HOME/opencode`, or `~/.local/share/opencode`.
pub fn data_dir() -> Option<PathBuf> {
    if let Ok(dir) = env::var("OPENCODE_DATA_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    if let Ok(dir) = env::var("XDG_DATA_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("opencode"));
        }
    }
    if let Ok(home) = env::var("HOME") {
        if !home.is_empty() {
            return Some(PathBuf::from(home).join(".local/share/opencode"));
        }
    }
    None
}

/// Resolve opencode's state dir (service registration, password):
/// `$XDG_STATE_HOME/opencode` or `~/.local/state/opencode`.
pub fn state_dir() -> Option<PathBuf> {
    if let Ok(dir) = env::var("XDG_STATE_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("opencode"));
        }
    }
    if let Ok(home) = env::var("HOME") {
        if !home.is_empty() {
            return Some(PathBuf::from(home).join(".local/state/opencode"));
        }
    }
    None
}

/// Resolve the opencode database path, mirroring opencode's own
/// resolution:
///
/// 1. `$OPENCODE_DB`: `:memory:` and absolute paths are used as-is; a
///    relative path is joined onto the data dir.
/// 2. `<data dir>/opencode.db` when it exists (the database of the
///    `latest`/`beta`/`prod` channels; also what opencode uses when
///    `OPENCODE_DISABLE_CHANNEL_DB` is set).
/// 3. exactly one channel database `<data dir>/opencode-<channel>.db`.
///    Several candidates together with no default are a usage error
///    naming them — set `OPENCODE_DB` to pick one.
pub fn db_path() -> Result<PathBuf> {
    let env = |name: &str| env::var(name).ok().filter(|v| !v.is_empty());
    resolve_db(&env, data_dir().as_deref())
}

/// Pure resolution over an env accessor and an optional data dir, so
/// the precedence rules are testable without mutating process state.
fn resolve_db(env: &dyn Fn(&str) -> Option<String>, dir: Option<&Path>) -> Result<PathBuf> {
    let dir = dir.ok_or_else(|| AppError::usage("cannot determine opencode data dir"))?;
    if let Some(value) = env("OPENCODE_DB") {
        let path = Path::new(&value);
        return Ok(if value == ":memory:" || path.is_absolute() {
            path.to_path_buf()
        } else {
            dir.join(path)
        });
    }
    let default = dir.join("opencode.db");
    if matches!(
        env("OPENCODE_DISABLE_CHANNEL_DB").as_deref(),
        Some("1") | Some("true")
    ) {
        return Ok(default);
    }
    if default.exists() {
        return Ok(default);
    }
    // No default database: fall back to a channel-named database
    // (`opencode-<channel>.db`), the layout of opencode installs on
    // channels other than `latest`/`beta`/`prod`.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if !e.file_type().is_ok_and(|ft| ft.is_file()) {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("opencode-") && name.ends_with(".db") {
                candidates.push(e.path());
            }
        }
    }
    match candidates.len() {
        // Nothing found: report the default path; the caller's existence
        // check turns this into the standard "DB not found" error.
        0 => Ok(default),
        1 => Ok(candidates.remove(0)),
        _ => {
            let mut names: Vec<String> = candidates
                .iter()
                .map(|p| {
                    p.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                })
                .collect();
            names.sort();
            Err(AppError::usage(format!(
                "multiple opencode channel databases found in {}: {} - set OPENCODE_DB to choose one",
                dir.display(),
                names.join(", ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Env accessor backed by a fixed map (empty-string values model
    /// unset variables, matching `db_path`'s env handling).
    fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    fn temp_dir(stem: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "opencode-dbtool-config-{}-{}",
            std::process::id(),
            stem
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn open_code_db_env_absolute_wins() {
        let env = fake_env(&[("OPENCODE_DB", "/elsewhere/db.sqlite")]);
        let out = resolve_db(&env, Some(Path::new("/d"))).unwrap();
        assert_eq!(out, PathBuf::from("/elsewhere/db.sqlite"));
    }

    #[test]
    fn open_code_db_env_memory_wins() {
        let env = fake_env(&[("OPENCODE_DB", ":memory:")]);
        let out = resolve_db(&env, Some(Path::new("/d"))).unwrap();
        assert_eq!(out, PathBuf::from(":memory:"));
    }

    #[test]
    fn open_code_db_env_relative_joins_data_dir() {
        let env = fake_env(&[("OPENCODE_DB", "custom.db")]);
        let out = resolve_db(&env, Some(Path::new("/d"))).unwrap();
        assert_eq!(out, PathBuf::from("/d/custom.db"));
    }

    #[test]
    fn default_db_wins_over_channel_dbs() {
        let dir = temp_dir("default-wins");
        std::fs::write(dir.join("opencode.db"), []).unwrap();
        std::fs::write(dir.join("opencode-prod.db"), []).unwrap();

        let out = resolve_db(&fake_env(&[]), Some(&dir)).unwrap();
        assert_eq!(out, dir.join("opencode.db"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn single_channel_db_is_fallback() {
        let dir = temp_dir("single-channel");
        std::fs::write(dir.join("opencode-prod.db"), []).unwrap();

        let out = resolve_db(&fake_env(&[]), Some(&dir)).unwrap();
        assert_eq!(out, dir.join("opencode-prod.db"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn several_channel_dbs_are_ambiguous() {
        let dir = temp_dir("ambiguous-channel");
        std::fs::write(dir.join("opencode-prod.db"), []).unwrap();
        std::fs::write(dir.join("opencode-nightly.db"), []).unwrap();

        let err = resolve_db(&fake_env(&[]), Some(&dir)).unwrap_err();
        assert_eq!(err.code, 2, "ambiguous database is a usage error");
        assert!(err.message.contains("OPENCODE_DB"), "stderr: {err}");
        assert!(err.message.contains("opencode-prod.db"), "stderr: {err}");
        assert!(err.message.contains("opencode-nightly.db"), "stderr: {err}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_db_reports_default_path() {
        let dir = temp_dir("missing-db");
        let out = resolve_db(&fake_env(&[]), Some(&dir)).unwrap();
        assert_eq!(out, dir.join("opencode.db"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn channel_db_scan_skips_wal_and_directories() {
        let dir = temp_dir("channel-scan");
        // WAL/shm siblings and a directory are not channel databases.
        std::fs::write(dir.join("opencode-prod.db-wal"), []).unwrap();
        std::fs::write(dir.join("opencode-prod.db-shm"), []).unwrap();
        std::fs::create_dir(dir.join("opencode-dir.db")).unwrap();

        let out = resolve_db(&fake_env(&[]), Some(&dir)).unwrap();
        assert_eq!(out, dir.join("opencode.db"), "no candidates -> default");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn disable_channel_db_targets_default() {
        let dir = temp_dir("disable-channel");
        std::fs::write(dir.join("opencode-prod.db"), []).unwrap();

        let env = fake_env(&[("OPENCODE_DISABLE_CHANNEL_DB", "1")]);
        let out = resolve_db(&env, Some(&dir)).unwrap();
        assert_eq!(out, dir.join("opencode.db"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_env_values_are_ignored() {
        let dir = temp_dir("empty-env");
        std::fs::write(dir.join("opencode.db"), []).unwrap();

        // An empty OPENCODE_DB behaves like an unset one.
        let env = fake_env(&[("OPENCODE_DB", "")]);
        let out = resolve_db(&env, Some(&dir)).unwrap();
        assert_eq!(out, dir.join("opencode.db"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_data_dir_is_usage_error() {
        let err = resolve_db(&fake_env(&[]), None).unwrap_err();
        assert_eq!(err.code, 2);
        assert!(err.message.contains("data dir"), "stderr: {err}");
    }
}
