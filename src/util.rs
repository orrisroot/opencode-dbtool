//! Small formatting and SQL helpers.

use crate::error::{AppError, Result};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// SQLite's per-statement variable limit depends on the build (commonly
/// 32,766); keep `IN (...)` chunks well below the common limits so
/// batch queries never hit "too many SQL variables".
pub const SQL_VAR_CHUNK: usize = 900;

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

/// `snapshot` dir under the data dir (git object packs per project).
pub fn snapshot_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("snapshot")
}

/// `log` dir under the data dir.
pub fn log_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("log")
}

/// `log/opencode.log` file under the data dir.
pub fn log_file(data_dir: &Path) -> PathBuf {
    log_dir(data_dir).join("opencode.log")
}

/// `shell` dir under the data dir (per-project shell command outputs,
/// `<project-id>/sh_*.out`).
pub fn shell_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("shell")
}

/// `repos` dir under the data dir (cached git repositories).
pub fn repos_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("repos")
}

/// File modification time in epoch milliseconds; `None` when unavailable.
pub fn file_mtime_ms(path: &Path) -> Option<i64> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    Some(duration.as_millis() as i64)
}

/// Parse an opencode log line's `timestamp=` prefix
/// (`timestamp=2026-09-16T13:51:28.659Z ...`) into epoch milliseconds.
/// Returns `None` when the line carries no parseable timestamp; callers
/// keep such lines (never delete what cannot be dated).
pub fn parse_log_ts(line: &str) -> Option<i64> {
    let rest = line.strip_prefix("timestamp=")?;
    // Fixed shape: `YYYY-MM-DDTHH:MM:SS.mmmZ` (millis optional).
    let (date, time) = rest.split_once('T')?;
    let (y, m, d) = (
        date.get(0..4)?.parse::<i64>().ok()?,
        date.get(5..7)?.parse::<i64>().ok()?,
        date.get(8..10)?.parse::<i64>().ok()?,
    );
    let (hh, mm, ss_millis) = (
        time.get(0..2)?.parse::<i64>().ok()?,
        time.get(3..5)?.parse::<i64>().ok()?,
        time.get(6..)?,
    );
    // Seconds, then optional `.mmm`, then a zone (`Z` or end).
    let sec_end = ss_millis
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(ss_millis.len());
    let ss: i64 = ss_millis.get(..sec_end)?.parse().ok()?;
    let mut millis: i64 = 0;
    let mut tail = ss_millis.get(sec_end..).unwrap_or("");
    if let Some(frac) = tail.strip_prefix('.') {
        let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }
        let scale = 10i64.pow(3u32.saturating_sub(digits.len() as u32));
        millis = digits.parse::<i64>().ok()?.saturating_mul(scale).min(999);
        tail = &frac[digits.len()..];
    }
    if !tail.is_empty() && !tail.starts_with('Z') {
        return None;
    }
    let days = days_from_civil(y, m, d)?;
    Some((days * 86_400 + hh * 3600 + mm * 60 + ss) * 1000 + millis)
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's
/// algorithm); `None` on out-of-range input.
fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || d < 1 {
        return None;
    }
    // Reject impossible dates (e.g. Feb 30) instead of rolling over:
    // callers treat `None` as "keep the line".
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let dim = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if d > dim {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
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
    fn parse_count_rejects_invalid() {
        for bad in ["", "-1", "abc", "1.5"] {
            assert!(parse_count(bad).is_err(), "should reject: {bad:?}");
        }
    }

    #[test]
    fn parse_log_ts_reads_opencode_lines() {
        // 2026-09-16T13:51:28.659Z -> epoch millis.
        assert_eq!(
            super::parse_log_ts("timestamp=2026-09-16T13:51:28.659Z level=INFO foo"),
            Some(1789566688659)
        );
        // No fractional seconds.
        assert_eq!(
            super::parse_log_ts("timestamp=2026-09-16T13:51:28Z x"),
            Some(1789566688000)
        );
        // Garbage and dateless lines are kept by callers (None).
        assert_eq!(super::parse_log_ts("no timestamp here"), None);
        assert_eq!(super::parse_log_ts("timestamp=bogus"), None);
        assert_eq!(super::parse_log_ts("timestamp=2026-13-99T99:99:99Z"), None);
        // Impossible calendar dates are rejected, not rolled over.
        assert_eq!(super::parse_log_ts("timestamp=2026-02-30T00:00:00Z"), None);
        assert_eq!(
            super::parse_log_ts("timestamp=2024-02-29T00:00:00Z"),
            super::parse_log_ts("timestamp=2024-02-29T00:00:00.000Z")
        );
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
