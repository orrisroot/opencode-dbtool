//! `stats` command: table sizes and totals.

use crate::db::{db_status, file_size};
use crate::error::Result;
use crate::output::print_json;
use crate::util::quote_ident;
use rusqlite::{params, Connection};
use std::path::Path;

pub fn cmd_stats(con: &Connection, db_path: &Path) -> Result<()> {
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
    print_json(&out)
}