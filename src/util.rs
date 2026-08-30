//! Small formatting and SQL helpers.

use crate::error::{AppError, Result};
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

/// Format a unix-millis timestamp as UTC ISO-8601; "-" for 0.
pub fn dt(ms: i64) -> String {
    if ms == 0 {
        return "-".to_string();
    }
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
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
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
}
