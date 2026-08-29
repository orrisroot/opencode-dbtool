//! Small formatting and SQL helpers.

/// Escape a SQLite identifier for safe interpolation in dynamic queries.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
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
    #[test]
    fn dt_formats_known_dates() {
        assert_eq!(super::dt(0), "-");
        assert_eq!(super::dt(1136214245000), "2006-01-02T15:04:05Z");
        assert_eq!(super::dt(1451606400000), "2016-01-01T00:00:00Z");
        assert_eq!(super::dt(1582934400000), "2020-02-29T00:00:00Z");
        assert_eq!(super::dt(1592611200000), "2020-06-20T00:00:00Z");
    }
}
