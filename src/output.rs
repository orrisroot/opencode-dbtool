//! JSON output helpers.

use crate::error::Result;
use std::io::Write;

/// Pretty-print a JSON value, surviving closed pipes (`head`, `less`, ...).
pub fn print_json(v: &serde_json::Value) -> Result<()> {
    let out = serde_json::to_string_pretty(v)?;
    let mut stdout = std::io::stdout().lock();
    if let Err(e) = writeln!(stdout, "{out}") {
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            return Err(e.into());
        }
    }
    Ok(())
}