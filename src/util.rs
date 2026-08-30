//! Small formatting and SQL helpers.

use crate::error::{AppError, Result};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Current time in epoch milliseconds.
pub fn now_ms() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| AppError::db(e.to_string()))?
        .as_millis() as i64)
}

/// Escape a SQLite identifier for safe interpolation in dynamic queries.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Parse an age like `30d`, `12h` or `2w` into milliseconds; a bare
/// number means days. Invalid or non-positive input is a usage error.
pub fn parse_age_ms(s: &str) -> Result<i64> {
    let s = s.trim();
    let (num, mult) = match s.as_bytes().last() {
        Some(b'h') => (&s[..s.len() - 1], 3_600_000i64),
        Some(b'd') => (&s[..s.len() - 1], 86_400_000i64),
        Some(b'w') => (&s[..s.len() - 1], 604_800_000i64),
        _ => (s, 86_400_000i64),
    };
    let n: i64 = num
        .parse()
        .map_err(|_| AppError::usage(format!("invalid age: {s}")))?;
    if n <= 0 {
        return Err(AppError::usage(format!("age must be positive: {s}")));
    }
    n.checked_mul(mult)
        .ok_or_else(|| AppError::usage(format!("age too large: {s}")))
}

/// Parse a byte size like `50M`, `500K` or `2G` (1024-based); a bare
/// number means bytes. Invalid or non-positive input is a usage error.
pub fn parse_size_bytes(s: &str) -> Result<i64> {
    let s = s.trim();
    let (num, mult) = match s.as_bytes().last() {
        Some(b'k' | b'K') => (&s[..s.len() - 1], 1024i64),
        Some(b'm' | b'M') => (&s[..s.len() - 1], 1024i64 * 1024),
        Some(b'g' | b'G') => (&s[..s.len() - 1], 1024i64 * 1024 * 1024),
        _ => (s, 1i64),
    };
    let n: i64 = num
        .parse()
        .map_err(|_| AppError::usage(format!("invalid size: {s}")))?;
    if n <= 0 {
        return Err(AppError::usage(format!("size must be positive: {s}")));
    }
    n.checked_mul(mult)
        .ok_or_else(|| AppError::usage(format!("size too large: {s}")))
}

/// Reject any argument for commands that take none.
pub fn expect_no_args(args: &[String], usage: &str) -> Result<()> {
    if let Some(a) = args.first() {
        return Err(AppError::usage(format!(
            "unexpected argument: {a} (usage: opencode-dbtool {usage})"
        )));
    }
    Ok(())
}

/// Parse a non-negative count like `10` for `--keep-latest`.
pub fn parse_count(s: &str) -> Result<i64> {
    let n: i64 = s
        .trim()
        .parse()
        .map_err(|_| AppError::usage(format!("invalid count: {s}")))?;
    if n < 0 {
        return Err(AppError::usage(format!("count must be non-negative: {s}")));
    }
    Ok(n)
}

/// Recursive size of a directory in bytes (regular files only, no
/// symlink following); 0 when the directory is missing or unreadable.
pub fn dir_size(path: &std::path::Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(path) {
        for e in rd.flatten() {
            if let Ok(ft) = e.file_type() {
                if ft.is_dir() {
                    total += dir_size(&e.path());
                } else if ft.is_file() {
                    total += e.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
    }
    total
}

/// `storage/session_diff` dir for a database path (one `<id>.json` per
/// session).
pub fn session_diff_dir(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("storage")
        .join("session_diff")
}

/// `snapshot` dir for a database path (git object packs per project).
pub fn snapshot_dir(db_path: &Path) -> PathBuf {
    db_path.parent().unwrap_or(Path::new(".")).join("snapshot")
}

/// `tool-output` dir for a database path.
pub fn tool_output_dir(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("tool-output")
}

/// `log` dir for a database path.
pub fn log_dir(db_path: &Path) -> PathBuf {
    db_path.parent().unwrap_or(Path::new(".")).join("log")
}

/// `log/opencode.log` file for a database path.
pub fn log_file(db_path: &Path) -> PathBuf {
    log_dir(db_path).join("opencode.log")
}

/// Decompose a unix-millis timestamp into UTC calendar fields.
fn calendar(ms: i64) -> (i64, u32, u32, u32, u32, u32) {
    let secs = ms / 1000;
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + if m <= 2 { 1 } else { 0 };
    (
        y,
        m as u32,
        d as u32,
        (tod / 3600) as u32,
        ((tod % 3600) / 60) as u32,
        (tod % 60) as u32,
    )
}

/// Format a unix-millis timestamp as UTC ISO-8601; "-" for 0.
pub fn dt(ms: i64) -> String {
    if ms == 0 {
        return "-".to_string();
    }
    let (y, m, d, h, mi, s) = calendar(ms);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Compact UTC timestamp for backup filenames (no colons), e.g.
/// `20260830T120000Z`.
pub fn timestamp_utc(ms: i64) -> String {
    let (y, m, d, h, mi, s) = calendar(ms);
    format!("{y:04}{m:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// Round to 4 decimal places for cost output.
pub fn round4(x: f64) -> f64 {
    (x * 10000.0).round() / 10000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dt_formats_known_dates() {
        assert_eq!(super::dt(0), "-");
        assert_eq!(super::dt(1136214245000), "2006-01-02T15:04:05Z");
        assert_eq!(super::dt(1451606400000), "2016-01-01T00:00:00Z");
        assert_eq!(super::dt(1582934400000), "2020-02-29T00:00:00Z");
        assert_eq!(super::dt(1592611200000), "2020-06-20T00:00:00Z");
    }

    #[test]
    fn timestamp_utc_is_compact_with_z() {
        assert_eq!(super::timestamp_utc(1136214245000), "20060102T150405Z");
        assert_eq!(super::timestamp_utc(1451606400000), "20160101T000000Z");
    }

    #[test]
    fn parse_age_units() {
        assert_eq!(parse_age_ms("1h").unwrap(), 3_600_000);
        assert_eq!(parse_age_ms("30d").unwrap(), 30 * 86_400_000);
        assert_eq!(parse_age_ms("2w").unwrap(), 14 * 86_400_000);
        assert_eq!(parse_age_ms("7").unwrap(), 7 * 86_400_000);
    }

    #[test]
    fn parse_age_rejects_invalid() {
        for bad in ["", "0", "0d", "-1d", "abc", "1m", "1.5d", "30D", "d"] {
            assert!(parse_age_ms(bad).is_err(), "should reject: {bad:?}");
        }
    }

    #[test]
    fn parse_age_overflow() {
        assert!(parse_age_ms("999999999999999999999d").is_err());
    }

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size_bytes("500").unwrap(), 500);
        assert_eq!(parse_size_bytes("1K").unwrap(), 1024);
        assert_eq!(parse_size_bytes("1k").unwrap(), 1024);
        assert_eq!(parse_size_bytes("50M").unwrap(), 50 * 1024 * 1024);
        assert_eq!(parse_size_bytes("2g").unwrap(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_size_rejects_invalid() {
        for bad in ["", "0", "0K", "-1", "abc", "1.5M", "1MB", "M"] {
            assert!(parse_size_bytes(bad).is_err(), "should reject: {bad:?}");
        }
    }

    #[test]
    fn parse_size_overflow() {
        assert!(parse_size_bytes("999999999999999999999G").is_err());
    }

    #[test]
    fn parse_count_ok() {
        assert_eq!(parse_count("0").unwrap(), 0);
        assert_eq!(parse_count("10").unwrap(), 10);
    }

    #[test]
    fn expect_no_args_ok() {
        assert!(expect_no_args(&[], "doctor").is_ok());
        let err = expect_no_args(&["x".into(), "--flag".into()], "doctor").unwrap_err();
        assert_eq!(err.code, 2);
        assert!(err.message.contains("unexpected argument: x"));
    }

    #[test]
    fn parse_count_rejects_invalid() {
        for bad in ["", "-1", "abc", "1.5"] {
            assert!(parse_count(bad).is_err(), "should reject: {bad:?}");
        }
    }

    #[test]
    fn dir_size_sums_nested_files() {
        let dir =
            std::env::temp_dir().join(format!("opencode-dbtool-dirsize-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("f1"), vec![0u8; 3]).unwrap();
        std::fs::write(dir.join("a/f2"), vec![0u8; 5]).unwrap();
        std::fs::write(dir.join("a/b/f3"), vec![0u8; 7]).unwrap();
        assert_eq!(super::dir_size(&dir), 15);
        assert_eq!(super::dir_size(&dir.join("missing")), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
