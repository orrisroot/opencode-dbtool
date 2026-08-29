//! Running opencode instance detection.

use crate::error::{AppError, Result};
use std::path::Path;
use sysinfo::{ProcessesToUpdate, System};

const TARGET_COMMS: [&str; 2] = ["opencode", "opencode-server"];

/// Match a process name against the known opencode executables,
/// tolerating platform suffixes (".exe", ".app", ...).
fn proc_name_matches(name: &str) -> bool {
    let base = Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| Path::new(n).file_stem())
        .and_then(|n| n.to_str())
        .unwrap_or("");
    TARGET_COMMS.contains(&base)
}

/// Return the pids of running opencode instances, sorted.
pub fn running_pids() -> Result<Vec<i32>> {
    if !cfg!(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows"
    )) {
        return Err(AppError::db(
            "process detection is not supported on this platform",
        ));
    }
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    let mut pids = Vec::new();
    for (pid, process) in sys.processes() {
        let name = process.name().to_string_lossy();
        if proc_name_matches(&name) {
            pids.push(pid.as_u32() as i32);
        }
    }
    pids.sort_unstable();
    Ok(pids)
}

/// Fail with exit code 1 when an opencode instance is running.
pub fn require_idle(reason: &str) -> Result<()> {
    let pids =
        running_pids().map_err(|e| AppError::db(format!("cannot detect running opencode: {e}")))?;
    if pids.is_empty() {
        return Ok(());
    }
    Err(AppError::busy(format!(
        "opencode is running (pid={}) - {reason}. close opencode and retry",
        pids.iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",")
    )))
}
