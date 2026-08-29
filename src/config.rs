//! Data-directory resolution.

use std::env;
use std::path::PathBuf;

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