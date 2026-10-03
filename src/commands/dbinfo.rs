//! `db`: path/pragma inspection, `PRAGMA optimize`, and read-only queries.

use crate::db::{env_status, file_size, EnvStatus};
use crate::error::{AppError, Result};
use crate::output;
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
struct DbPathOut {
    #[serde(flatten)]
    env: EnvStatus,
    db: String,
    journal_mode: String,
    page_size: i64,
    auto_vacuum: i64,
    busy_timeout: i64,
    user_version: i64,
    wal_bytes: u64,
}

pub fn cmd_db_path(con: &Connection, db_path: &Path) -> Result<()> {
    let journal_mode: String = con.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
    let page_size: i64 = con.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let auto_vacuum: i64 = con.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
    let busy_timeout: i64 = con.query_row("PRAGMA busy_timeout", [], |r| r.get(0))?;
    let user_version: i64 = con.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    output::emit(&serde_json::to_value(DbPathOut {
        env: env_status(db_path),
        db: db_path.to_string_lossy().to_string(),
        journal_mode,
        page_size,
        auto_vacuum,
        busy_timeout,
        user_version,
        wal_bytes: file_size(&db_path.with_extension("db-wal")),
    })?)
}

#[derive(Serialize)]
struct DbOptimizeOut {
    #[serde(flatten)]
    env: EnvStatus,
    db: String,
    optimized: bool,
}

/// `PRAGMA optimize`: refresh query-planner statistics. Safe online.
pub fn cmd_db_optimize(con: &Connection, db_path: &Path) -> Result<()> {
    con.execute_batch("PRAGMA optimize;")
        .map_err(|e| crate::error::AppError::db(format!("PRAGMA optimize failed: {e}")))?;
    output::emit(&serde_json::to_value(DbOptimizeOut {
        env: env_status(db_path),
        db: db_path.to_string_lossy().to_string(),
        optimized: true,
    })?)
}

#[derive(Serialize)]
struct DbQueryOut {
    #[serde(flatten)]
    env: EnvStatus,
    db: String,
    sql: String,
    columns: Vec<String>,
    rows: Vec<serde_json::Value>,
    row_count: usize,
    limit: usize,
    truncated: bool,
}

/// Run a single read-only statement and return the full result shape.
pub fn query_value(
    db_path: &Path,
    sql: &str,
    limit: usize,
) -> Result<(serde_json::Value, Vec<String>, serde_json::Value)> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let keyword = trimmed
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    if !matches!(keyword.as_str(), "SELECT" | "WITH" | "EXPLAIN") {
        return Err(AppError::usage(
            "only read-only queries are allowed (SELECT, WITH, EXPLAIN)",
        ));
    }
    let con = crate::db::open_conn(db_path, true)?;
    // Belt and braces: even a writable statement cannot change anything.
    con.execute_batch("PRAGMA query_only = ON;")?;
    let mut stmt = con.prepare(trimmed)?;
    let columns: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|c| (*c).to_string())
        .collect();
    let cap = if limit == 0 { usize::MAX } else { limit };
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut truncated = false;
    {
        let mut result = stmt.query([])?;
        while let Some(row) = result.next()? {
            if rows.len() >= cap {
                truncated = true;
                break;
            }
            let mut obj = serde_json::Map::new();
            for (i, name) in columns.iter().enumerate() {
                obj.insert(name.clone(), cell_json(row.get_ref(i)?));
            }
            rows.push(serde_json::Value::Object(obj));
        }
    }
    let row_count = rows.len();
    let rows_value = serde_json::Value::Array(rows);
    let out = DbQueryOut {
        env: env_status(db_path),
        db: db_path.to_string_lossy().to_string(),
        sql: trimmed.to_string(),
        columns: columns.clone(),
        rows: rows_value.as_array().cloned().unwrap_or_default(),
        row_count,
        limit,
        truncated,
    };
    Ok((serde_json::to_value(&out)?, columns, rows_value))
}

/// `db query`: table/CSV render the dynamic rows; JSON keeps the metadata.
pub fn cmd_db_query(db_path: &Path, sql: &str, limit: usize) -> Result<()> {
    let (full, columns, rows) = query_value(db_path, sql, limit)?;
    let refs: Vec<&str> = columns.iter().map(String::as_str).collect();
    output::emit_dynamic(&full, &rows, &refs)
}

fn cell_json(v: ValueRef<'_>) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        ValueRef::Null => J::Null,
        ValueRef::Integer(i) => J::from(i),
        ValueRef::Real(f) => J::from(f),
        ValueRef::Text(t) => J::from(String::from_utf8_lossy(t).to_string()),
        ValueRef::Blob(b) => J::from(format!("<blob {} bytes>", b.len())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb;

    #[test]
    fn reports_pragmas() {
        let dir = testdb::temp_data_dir("dbinfo");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);
        cmd_db_path(&con, &db_path).unwrap();
        drop(con);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn query_is_read_only_and_reports_rows() {
        let dir = testdb::temp_data_dir("dbquery");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_session(&con, "s2", "/a", None);
        drop(con);

        // Only read-only statements are accepted.
        assert!(query_value(&db_path, "DELETE FROM session_v2", 100).is_err());
        assert!(query_value(&db_path, "PRAGMA journal_mode", 100).is_err());

        // A trailing semicolon is fine.
        let (v, columns, rows) = query_value(&db_path, "SELECT id FROM session_v2;", 100).unwrap();
        assert_eq!(columns, vec!["id"]);
        assert_eq!(v["row_count"], 2);
        assert_eq!(v["truncated"], false);
        assert_eq!(rows[0]["id"], "s1");

        // The limit marks the result as truncated.
        let (v, _, rows) = query_value(&db_path, "SELECT id FROM session_v2", 1).unwrap();
        assert_eq!(v["row_count"], 1);
        assert_eq!(v["truncated"], true);
        assert_eq!(rows.as_array().unwrap().len(), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
