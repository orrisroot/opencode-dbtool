//! Cross-process maintenance lock so two runs cannot overlap (e.g. a cron
//! job and a manual `cleanup`).

use crate::error::{AppError, Result};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

/// Held for the duration of a real (non-dry-run) maintenance command.
#[derive(Debug)]
pub struct ToolLock {
    file: File,
}

impl ToolLock {
    /// Try to acquire the maintenance lock. Returns `Ok(None)` when no
    /// state directory is available (locking is best effort).
    pub fn acquire() -> Result<Option<ToolLock>> {
        let Some(dir) = crate::config::state_dir() else {
            return Ok(None);
        };
        Ok(Some(acquire_in(&dir.join("opencode-dbtool"))?))
    }
}

fn acquire_in(dir: &Path) -> Result<ToolLock> {
    std::fs::create_dir_all(dir)
        .map_err(|e| AppError::db(format!("cannot create {}: {e}", dir.display())))?;
    let path = dir.join("maintenance.lock");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| AppError::db(format!("cannot open lock {}: {e}", path.display())))?;
    match file.try_lock_exclusive() {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            return Err(AppError::busy(format!(
                "another opencode-dbtool run is in progress (lock: {})",
                path.display()
            )));
        }
        Err(e) => return Err(AppError::db(format!("cannot lock {}: {e}", path.display()))),
    }
    let _ = file.set_len(0);
    let _ = writeln!(file, "pid={}", std::process::id());
    Ok(ToolLock { file })
}

impl Drop for ToolLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(stem: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "opencode-dbtool-lock-{}-{}",
            std::process::id(),
            stem
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    #[cfg(unix)]
    fn second_lock_is_refused_and_released_on_drop() {
        let dir = temp_dir("exclusive");
        let first = acquire_in(&dir).unwrap();
        let err = acquire_in(&dir).unwrap_err();
        assert_eq!(err.code, 1, "busy exit code");
        assert!(err.message.contains("in progress"), "got: {err}");
        drop(first);
        acquire_in(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
