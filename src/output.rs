//! Output helpers: pretty JSON (default when stdout is piped) or a human
//! table (default on a terminal, or with `--format table`).
//!
//! The JSON shapes are the stable contract; the table rendering is a
//! generic view over the same values, with optional per-command column
//! selection and human-readable byte sizes.

use crate::cli::Format;
use crate::error::Result;
use serde_json::Value;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::OnceLock;

const FORMAT_JSON: u8 = 0;
const FORMAT_TABLE: u8 = 1;
const FORMAT_CSV: u8 = 2;

/// Active output format; JSON by default so unit tests and piped runs
/// keep seeing the stable contract.
static FORMAT: AtomicU8 = AtomicU8::new(FORMAT_JSON);
/// `--fields`: explicit columns for table/CSV output.
static FIELDS: OnceLock<Vec<String>> = OnceLock::new();
/// Longest string shown in a table cell (adjusted to `COLUMNS`).
static CELL_LIMIT: AtomicUsize = AtomicUsize::new(80);
/// Auto-pager on a terminal (`--no-pager` disables).
static PAGER: AtomicBool = AtomicBool::new(false);
/// Relative timestamps in tables (`--absolute` disables).
static RELATIVE_TIME: AtomicBool = AtomicBool::new(true);
/// ANSI colors in tables (TTY only, `--no-color` / `NO_COLOR` disable).
static COLOR: AtomicBool = AtomicBool::new(false);
/// `--quiet`: suppress progress and confirmation hints on stderr.
static QUIET: AtomicBool = AtomicBool::new(false);

/// Effective format for this process: an explicit `--format` wins,
/// otherwise table on a terminal and JSON when piped.
pub fn effective_format(explicit: Option<Format>) -> Format {
    match explicit {
        Some(format) => format,
        None => {
            if std::io::stdout().is_terminal() {
                Format::Table
            } else {
                Format::Json
            }
        }
    }
}

pub fn set_format(format: Format) {
    let code = match format {
        Format::Json => FORMAT_JSON,
        Format::Table => FORMAT_TABLE,
        Format::Csv => FORMAT_CSV,
    };
    FORMAT.store(code, Ordering::Relaxed);
}

pub fn set_fields(fields: Vec<String>) {
    if !fields.is_empty() {
        let _ = FIELDS.set(fields);
    }
}

pub fn set_pager(enabled: bool) {
    PAGER.store(enabled, Ordering::Relaxed);
}

pub fn set_relative(enabled: bool) {
    RELATIVE_TIME.store(enabled, Ordering::Relaxed);
}

pub fn set_color(enabled: bool) {
    COLOR.store(enabled, Ordering::Relaxed);
}

pub fn set_quiet(quiet: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
}

pub fn table_mode() -> bool {
    FORMAT.load(Ordering::Relaxed) == FORMAT_TABLE
}

fn csv_mode() -> bool {
    FORMAT.load(Ordering::Relaxed) == FORMAT_CSV
}

/// Progress output is on when stderr is a terminal and `--quiet` is off.
pub fn progress_enabled() -> bool {
    !QUIET.load(Ordering::Relaxed) && std::io::stderr().is_terminal()
}

/// One progress line on stderr.
pub fn progress(message: &str) {
    if progress_enabled() {
        eprintln!("{message}");
    }
}

/// In-place progress counter on stderr; finish with `progress_finish`.
pub fn progress_replace(message: &str) {
    if progress_enabled() {
        eprint!("\r{message}\x1b[K");
    }
}

/// Clear the in-place progress counter.
pub fn progress_finish() {
    if progress_enabled() {
        eprint!("\r\x1b[K");
    }
}

/// Print a command result in the active format.
pub fn emit(v: &Value) -> Result<()> {
    emit_cols(v, &[])
}

/// Print pre-rendered text (curated table summaries).
pub fn emit_text(text: &str) -> Result<()> {
    write_stdout(text)
}

/// Print a command result; `columns` selects (and orders) the table/CSV
/// columns. JSON mode always prints the full value.
pub fn emit_cols(v: &Value, columns: &[&str]) -> Result<()> {
    let effective = effective_columns(columns);
    let text = if table_mode() {
        set_cell_limit_for(effective.len());
        let refs: Vec<&str> = effective.iter().map(String::as_str).collect();
        render(v, &refs)
    } else if csv_mode() {
        render_csv(v, &effective)
    } else {
        serde_json::to_string_pretty(v)?
    };
    write_stdout(&text)
}

/// `--fields` overrides the command's column selection.
fn effective_columns(columns: &[&str]) -> Vec<String> {
    match FIELDS.get() {
        Some(fields) if !fields.is_empty() => fields.clone(),
        _ => columns.iter().map(|s| s.to_string()).collect(),
    }
}

/// Keep tables within `COLUMNS` by shortening cells.
fn set_cell_limit_for(ncols: usize) {
    let width = std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<usize>().ok())
        .unwrap_or(0);
    CELL_LIMIT.store(cell_limit_for(width, ncols), Ordering::Relaxed);
}

/// Longest cell length that keeps `ncols` columns inside `width`
/// (columns are separated by two spaces).
fn cell_limit_for(width: usize, ncols: usize) -> usize {
    if width < 40 || ncols == 0 {
        return 80;
    }
    let overhead = 2 * ncols.saturating_sub(1);
    ((width.saturating_sub(overhead)) / ncols).clamp(12, 80)
}

/// Survive closed pipes (`head`, `less`, ...), optionally through a pager.
fn write_stdout(text: &str) -> Result<()> {
    if PAGER.load(Ordering::Relaxed) && (table_mode() || csv_mode()) && try_page(text) {
        return Ok(());
    }
    let mut stdout = std::io::stdout().lock();
    if let Err(e) = writeln!(stdout, "{text}") {
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            return Err(e.into());
        }
    }
    Ok(())
}

/// Pipe the text through `$PAGER` (or `less -FRX`); `false` when the pager
/// could not be started, so the caller can write directly.
fn try_page(text: &str) -> bool {
    let (program, args): (String, Vec<String>) = match std::env::var("PAGER") {
        Ok(pager) if !pager.trim().is_empty() => {
            let mut parts = pager.split_whitespace();
            let program = parts.next().unwrap_or("less").to_string();
            (program, parts.map(str::to_string).collect())
        }
        _ => ("less".to_string(), vec!["-FRX".to_string()]),
    };
    let child = std::process::Command::new(&program)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    let _ = child.wait();
    true
}

/// Render a value as the table view (used for curated renderings).
pub fn render_value_text(v: &Value) -> String {
    render(v, &[])
}

// ---------------------------------------------------------------------------
// table rendering
// ---------------------------------------------------------------------------

fn pad(indent: usize) -> String {
    " ".repeat(indent)
}

fn render(v: &Value, columns: &[&str]) -> String {
    let mut out = String::new();
    render_value(v, columns, 0, &mut out);
    out.trim_end().to_string()
}

fn render_value(v: &Value, columns: &[&str], indent: usize, out: &mut String) {
    match v {
        Value::Object(map) => render_object(map, indent, out),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str(&format!("{}-\n", pad(indent)));
            } else if items.iter().all(Value::is_object) {
                render_rows(items, columns, indent, out);
            } else {
                for item in items {
                    render_value(item, &[], indent, out);
                }
            }
        }
        scalar => out.push_str(&format!("{}{}\n", pad(indent), scalar_text("", scalar))),
    }
}

fn render_object(map: &serde_json::Map<String, Value>, indent: usize, out: &mut String) {
    let p = pad(indent);
    let mut scalars: Vec<(&String, &Value)> = Vec::new();
    let mut compounds: Vec<(&String, &Value)> = Vec::new();
    for (k, v) in map {
        match v {
            Value::Object(_) | Value::Array(_) => compounds.push((k, v)),
            _ => scalars.push((k, v)),
        }
    }
    let width = scalars
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    for (k, v) in &scalars {
        out.push_str(&format!("{p}{k:width$}  {}\n", scalar_text(k, v)));
    }
    for (k, v) in &compounds {
        match v {
            Value::Array(items) if items.is_empty() => {
                out.push_str(&format!("{p}{k}: -\n"));
            }
            Value::Array(items) if items.iter().all(|i| !i.is_object() && !i.is_array()) => {
                let joined = items
                    .iter()
                    .map(|i| scalar_text("", i))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!("{p}{k}: {}\n", truncate(&joined)));
            }
            Value::Array(items) => {
                out.push_str(&format!("{p}{k}:\n"));
                if items.iter().all(Value::is_object) {
                    render_rows(items, &[], indent + 2, out);
                } else {
                    for item in items {
                        render_value(item, &[], indent + 2, out);
                    }
                }
            }
            Value::Object(inner) => {
                out.push_str(&format!("{p}{k}:\n"));
                render_object(inner, indent + 2, out);
            }
            scalar => out.push_str(&format!("{p}{k}: {}\n", scalar_text(k, scalar))),
        }
    }
}

fn render_rows(items: &[Value], columns: &[&str], indent: usize, out: &mut String) {
    let keys: Vec<String> = if columns.is_empty() {
        let mut ks: Vec<String> = Vec::new();
        for item in items {
            if let Value::Object(m) = item {
                for k in m.keys() {
                    if !ks.iter().any(|x| x == k) {
                        ks.push(k.clone());
                    }
                }
            }
        }
        ks
    } else {
        columns.iter().map(|s| s.to_string()).collect()
    };
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|item| {
            keys.iter()
                .map(|k| item.get(k).map(|v| scalar_text(k, v)).unwrap_or_default())
                .collect()
        })
        .collect();
    let mut widths: Vec<usize> = keys.iter().map(|k| k.chars().count()).collect();
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let p = pad(indent);
    let header = format_row(&keys, &widths);
    let separator = widths
        .iter()
        .map(|w| "-".repeat((*w).max(1)))
        .collect::<Vec<_>>()
        .join("  ");
    if COLOR.load(Ordering::Relaxed) {
        out.push_str(&format!("{p}\x1b[1m{header}\x1b[0m\n"));
        out.push_str(&format!("{p}\x1b[2m{separator}\x1b[0m\n"));
    } else {
        out.push_str(&format!("{p}{header}\n"));
        out.push_str(&format!("{p}{separator}\n"));
    }
    for row in &rows {
        out.push_str(&format!("{p}{}\n", format_row(row, &widths)));
    }
}

fn format_row(cells: &[String], widths: &[usize]) -> String {
    cells
        .iter()
        .enumerate()
        .map(|(i, cell)| {
            let len = cell.chars().count();
            format!("{cell}{}", " ".repeat(widths[i].saturating_sub(len)))
        })
        .collect::<Vec<_>>()
        .join("  ")
        .trim_end()
        .to_string()
}

fn scalar_text(key: &str, v: &Value) -> String {
    match v {
        Value::Null => "-".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::String(s) => {
            if RELATIVE_TIME.load(Ordering::Relaxed) && is_time_key(key) {
                if let Some(ms) = crate::util::parse_iso_utc(s) {
                    return crate::util::human_age(crate::util::now_ms().unwrap_or(0), ms);
                }
            }
            truncate(s)
        }
        Value::Number(n) => {
            if is_byte_key(key) {
                if let Some(i) = n.as_i64() {
                    return human_bytes(i);
                }
            }
            n.to_string()
        }
        compound => truncate(&serde_json::to_string(compound).unwrap_or_default()),
    }
}

fn is_time_key(key: &str) -> bool {
    matches!(key, "updated" | "time" | "created")
}

fn is_byte_key(key: &str) -> bool {
    key == "bytes" || key.ends_with("_bytes")
}

fn truncate(s: &str) -> String {
    let limit = CELL_LIMIT.load(Ordering::Relaxed);
    if s.chars().count() <= limit {
        return s.to_string();
    }
    let head: String = s.chars().take(limit.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Human-readable byte size (1024-based), e.g. `8.4 MB`, `123 B`.
pub fn human_bytes(n: i64) -> String {
    const KB: f64 = 1024.0;
    let negative = n < 0;
    let v = (n as f64).abs();
    let (value, unit) = if v < KB {
        (v, "B")
    } else if v < KB * KB {
        (v / KB, "KB")
    } else if v < KB * KB * KB {
        (v / KB / KB, "MB")
    } else if v < KB * KB * KB * KB {
        (v / KB / KB / KB, "GB")
    } else {
        (v / KB / KB / KB / KB, "TB")
    };
    let value = if unit == "B" {
        format!("{}", value as u64)
    } else {
        format!("{value:.1}")
    };
    format!("{}{} {}", if negative { "-" } else { "" }, value, unit)
}

// ---------------------------------------------------------------------------
// CSV rendering
// ---------------------------------------------------------------------------

fn render_csv(v: &Value, columns: &[String]) -> String {
    let mut out = String::new();
    match v {
        Value::Array(items) if items.iter().all(Value::is_object) => {
            let keys = csv_keys(items, columns);
            out.push_str(&csv_row(&keys));
            for item in items {
                let cells: Vec<String> = keys.iter().map(|k| csv_cell(item.get(k))).collect();
                out.push_str(&csv_row(&cells));
            }
        }
        Value::Object(map) => {
            for (k, val) in map {
                out.push_str(&csv_row(&[k.clone(), csv_cell(Some(val))]));
            }
        }
        other => out.push_str(&csv_row(&[csv_cell(Some(other))])),
    }
    out.trim_end().to_string()
}

fn csv_keys(items: &[Value], columns: &[String]) -> Vec<String> {
    if !columns.is_empty() {
        return columns.to_vec();
    }
    let mut keys: Vec<String> = Vec::new();
    for item in items {
        if let Value::Object(map) = item {
            for k in map.keys() {
                if !keys.contains(k) {
                    keys.push(k.clone());
                }
            }
        }
    }
    keys
}

fn csv_cell(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(compound) => serde_json::to_string(compound).unwrap_or_default(),
    }
}

fn csv_row(cells: &[String]) -> String {
    let mut line = cells
        .iter()
        .map(|c| csv_escape(c))
        .collect::<Vec<_>>()
        .join(",");
    line.push('\n');
    line
}

fn csv_escape(s: &str) -> String {
    if s.contains(['"', ',', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(123), "123 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(8_388_608), "8.0 MB");
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024), "2.0 GB");
        assert_eq!(human_bytes(-2048), "-2.0 KB");
    }

    #[test]
    fn renders_list_as_table_with_selected_columns() {
        let v = json!([
            {"id": "ses_1", "title": "hello", "size_bytes": 2048, "cost": 0.42, "events": 3},
            {"id": "ses_2", "title": "bye", "size_bytes": 512, "cost": 0.0, "events": 1}
        ]);
        let text = render(&v, &["id", "title", "size_bytes"]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "header + separator + 2 rows: {text}");
        assert!(lines[0].starts_with("id"));
        assert!(lines[0].contains("size_bytes"));
        assert!(lines[1].starts_with("--"));
        assert!(lines[2].contains("ses_1"));
        assert!(lines[2].contains("2.0 KB"));
        assert!(!lines[2].contains("0.42"), "unselected column hidden");
    }

    #[test]
    fn renders_objects_as_key_value_and_nested_sections() {
        let v = json!({
            "db": "/tmp/opencode.db",
            "db_bytes": 8192,
            "tables": {"session_v2": 3, "event": 10},
            "orphans": {"a": [], "b": ["x"]}
        });
        let text = render(&v, &[]);
        assert!(text.contains("db_bytes"), "got: {text}");
        assert!(text.contains("8.0 KB"), "got: {text}");
        assert!(text.contains("tables:"), "got: {text}");
        assert!(text.contains("session_v2"), "got: {text}");
        assert!(text.contains("a:"), "got: {text}");
    }

    #[test]
    fn truncates_long_cells() {
        let long = "x".repeat(500);
        let v = json!([{"id": "s", "title": long}]);
        let text = render(&v, &[]);
        assert!(text.contains('…'));
        assert!(text.lines().count() <= 4);
    }

    #[test]
    fn json_mode_still_pretty_prints() {
        let v = json!({"a": 1});
        assert_eq!(
            serde_json::to_string_pretty(&v).unwrap(),
            "{\n  \"a\": 1\n}"
        );
    }

    #[test]
    fn csv_renders_arrays_with_selected_columns_and_escaping() {
        let v = json!([
            {"id": "a", "title": "hello, world", "size_bytes": 2048},
            {"id": "b", "title": "say \"hi\"", "size_bytes": 0}
        ]);
        let text = render_csv(&v, &["id".to_string(), "title".to_string()]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "id,title");
        assert_eq!(lines[1], "a,\"hello, world\"");
        assert_eq!(lines[2], "b,\"say \"\"hi\"\"\"");
    }

    #[test]
    fn csv_renders_objects_as_key_value_rows() {
        let v = json!({"db": "/x/opencode.db", "ok": true});
        let text = render_csv(&v, &[]);
        assert!(text.contains("db,/x/opencode.db"), "got: {text}");
        assert!(text.contains("ok,true"), "got: {text}");
    }

    #[test]
    fn cell_limit_scales_with_width() {
        assert_eq!(cell_limit_for(0, 4), 80);
        assert_eq!(cell_limit_for(30, 4), 80);
        assert_eq!(cell_limit_for(120, 3), 38);
        assert_eq!(cell_limit_for(50, 10), 12);
    }
}
