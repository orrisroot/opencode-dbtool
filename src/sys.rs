//! Running opencode instance detection.

use crate::error::{AppError, Result};
use std::path::Path;
use sysinfo::{ProcessesToUpdate, System};

/// Executable names of opencode processes: the CLI and server
/// (`opencode`, `opencode-server`; the service is spawned from the same
/// binary) plus the 2.x beta npm executable (`opencode2`). The beta name
/// is kept deliberately: if a stale beta binary ever runs against the same
/// data dir, destructive commands must still be refused.
const TARGET_COMMS: [&str; 3] = ["opencode", "opencode-server", "opencode2"];

/// File stem of a process name / path ("opencode.exe" -> "opencode").
fn target_stem(s: &str) -> &str {
    Path::new(s)
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| Path::new(n).file_stem())
        .and_then(|n| n.to_str())
        .unwrap_or("")
}

/// Match a process name or executable path against the known opencode
/// executables, tolerating platform suffixes (".exe", ".app", ...).
fn name_matches(s: &str) -> bool {
    TARGET_COMMS.contains(&target_stem(s))
}

/// Match a command-line token. Only invocations match (argv[0] style:
/// "opencode", "opencode-server", "/usr/bin/opencode"); data files such
/// as "opencode.db" are not matched, to avoid false positives from
/// commands that merely reference opencode's files.
fn cmd_token_matches(arg: &str) -> bool {
    let file = Path::new(arg)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    TARGET_COMMS.contains(&file)
}

fn process_matches(p: &sysinfo::Process) -> bool {
    if name_matches(&p.name().to_string_lossy()) {
        return true;
    }
    if let Some(exe) = p.exe() {
        if name_matches(&exe.to_string_lossy()) {
            return true;
        }
    }
    // Only argv[0] (the program invocation) may identify an opencode
    // process; later arguments such as `grep opencode …` or paths into
    // `~/.local/share/opencode` are never treated as invocations.
    match p.cmd().first() {
        Some(arg) => cmd_token_matches(&arg.to_string_lossy()),
        None => false,
    }
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
        if process_matches(process) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_matches_known_executables() {
        assert!(name_matches("opencode"));
        assert!(name_matches("opencode-server"));
        assert!(name_matches("opencode2"));
        assert!(name_matches("opencode2.exe"));
        assert!(name_matches("opencode.exe"));
        assert!(name_matches("/usr/bin/opencode"));
        // Suffix tolerance also covers names with dots; data files are
        // excluded by cmd_token_matches instead.
        assert!(!name_matches("myapp"));
        assert!(!name_matches("postgres"));
    }

    #[test]
    fn cmd_tokens_match_only_invocations() {
        assert!(cmd_token_matches("opencode"));
        assert!(cmd_token_matches("opencode-server"));
        assert!(cmd_token_matches("opencode2"));
        assert!(cmd_token_matches("/home/u/bin/opencode2"));
        assert!(cmd_token_matches("/home/u/bin/opencode"));
        assert!(cmd_token_matches("./opencode"));
        // Files and paths referencing opencode data are not invocations.
        assert!(!cmd_token_matches("opencode.db"));
        assert!(!cmd_token_matches("opencode2.db"));
        assert!(!cmd_token_matches("~/.local/share/opencode/opencode.db"));
        assert!(!cmd_token_matches("--opencode"));
        assert!(!cmd_token_matches("ls"));
    }
}
