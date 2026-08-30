//! `stats` command: table sizes and totals.

use crate::db::{db_status, file_size};
use crate::error::Result;
use crate::output::print_json;
use crate::util::{dir_size, quote_ident, session_diff_dir, snapshot_dir, tool_output_dir};
use rusqlite::{params, Connection};
use std::path::Path;

pub fn cmd_stats(con: &Connection, db_path: &Path) -> Result<()> {
    print_json(&stats_value(con, db_path)?)
}

/// Build the stats object (exposed for tests).
pub fn stats_value(con: &Connection, db_path: &Path) -> Result<serde_json::Value> {
    let freelist: i64 = con
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .unwrap_or(-1);
    let mut out = db_status(db_path);
    out["db_bytes"] = serde_json::json!(file_size(db_path));
    out["wal_bytes"] = serde_json::json!(file_size(&db_path.with_extension("db-wal")));
    out["free_pages"] = serde_json::json!(freelist);

    let mut tables: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    let mut total: i64 = 0;
    {
        let mut stmt = con.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?;
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for name in names {
            let rows: i64 = con
                .query_row(
                    &format!("SELECT COUNT(*) FROM {}", quote_ident(&name)),
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(-1);
            let has_data = con
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name='data'",
                    params![name],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0);
            let bytes: i64 = if has_data > 0 {
                con.query_row(
                    &format!(
                        "SELECT COALESCE(SUM(length(CAST(data AS BLOB))),0) FROM {}",
                        quote_ident(&name)
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0)
            } else {
                0
            };
            total += bytes;
            tables.insert(name, serde_json::json!(rows));
        }
    }
    out["tables"] = serde_json::json!(tables);
    out["total_data_bytes"] = serde_json::json!(total);
    out["storage"] = serde_json::json!({
        "session_diff_bytes": dir_size(&session_diff_dir(db_path)),
        "snapshot_bytes": dir_size(&snapshot_dir(db_path)),
        "tool_output_bytes": dir_size(&tool_output_dir(db_path)),
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::stats_value;
    use crate::testdb;
    use std::fs;

    #[test]
    fn storage_field_reports_directory_sizes() {
        let dir = testdb::temp_data_dir("stats-storage");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);
        fs::create_dir_all(dir.join("storage/session_diff")).unwrap();
        fs::write(dir.join("storage/session_diff/s1.json"), vec![0u8; 3]).unwrap();
        fs::create_dir_all(dir.join("snapshot/p")).unwrap();
        fs::write(dir.join("snapshot/p/o"), vec![0u8; 5]).unwrap();
        fs::create_dir_all(dir.join("tool-output")).unwrap();
        fs::write(dir.join("tool-output/x"), vec![0u8; 7]).unwrap();

        let out = stats_value(&con, &db_path).unwrap();
        assert_eq!(out["storage"]["session_diff_bytes"], 3);
        assert_eq!(out["storage"]["snapshot_bytes"], 5);
        assert_eq!(out["storage"]["tool_output_bytes"], 7);

        fs::remove_dir_all(&dir).unwrap();
    }
}
