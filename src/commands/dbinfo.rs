//! `db path`: the resolved database path and key pragmas.

use crate::db::{env_status, file_size, EnvStatus};
use crate::error::Result;
use crate::output;
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
}
