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
use std::sync::atomic::{AtomicBool, Ordering};

/// Table rendering is off by default so unit tests and piped runs keep
/// seeing JSON.
static TABLE_MODE: AtomicBool = AtomicBool::new(false);

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
    TABLE_MODE.store(format == Format::Table, Ordering::Relaxed);
}

pub fn table_mode() -> bool {
    TABLE_MODE.load(Ordering::Relaxed)
}

/// Print a command result in the active format.
pub fn emit(v: &Value) -> Result<()> {
    emit_cols(v, &[])
}

/// Print pre-rendered text (curated table summaries).
pub fn emit_text(text: &str) -> Result<()> {
    write_stdout(text)
}

/// Print a command result; `columns` selects (and orders) the table
/// columns in table mode. JSON mode always prints the full value.
pub fn emit_cols(v: &Value, columns: &[&str]) -> Result<()> {
    let text = if table_mode() {
        render(v, columns)
    } else {
        serde_json::to_string_pretty(v)?
    };
    write_stdout(&text)
}

/// Survive closed pipes (`head`, `less`, ...).
fn write_stdout(text: &str) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    if let Err(e) = writeln!(stdout, "{text}") {
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            return Err(e.into());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// table rendering
// ---------------------------------------------------------------------------

/// Longest string shown in a table cell before truncation.
const CELL_MAX: usize = 80;

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
    out.push_str(&format!("{p}{}\n", format_row(&keys, &widths)));
    out.push_str(&format!(
        "{p}{}\n",
        widths
            .iter()
            .map(|w| "-".repeat((*w).max(1)))
            .collect::<Vec<_>>()
            .join("  ")
    ));
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
        Value::String(s) => truncate(s),
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

fn is_byte_key(key: &str) -> bool {
    key == "bytes" || key.ends_with("_bytes")
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= CELL_MAX {
        return s.to_string();
    }
    let head: String = s.chars().take(CELL_MAX - 1).collect();
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
}
