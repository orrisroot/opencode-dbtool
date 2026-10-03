//! `db checkpoint`: checkpoint the WAL (PASSIVE or TRUNCATE).
//!
//! Checkpointing is safe while opencode runs: PASSIVE never blocks and
//! moves committed frames into the database file; TRUNCATE additionally
//! shrinks the WAL when no readers hold snapshots. Both are best-effort
//! and report the lock state instead of failing.

use crate::db::{env_status, file_size, EnvStatus};
use crate::error::Result;
use crate::output;
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
struct CheckpointOut {
    #[serde(flatten)]
    env: EnvStatus,
    mode: &'static str,
    /// 1 when another connection prevented the checkpoint from completing.
    busy: i64,
    log_frames: i64,
    checkpointed_frames: i64,
    wal_bytes_before: u64,
    wal_bytes_after: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

pub fn cmd_checkpoint(db_path: &Path, truncate: bool) -> Result<()> {
    let wal_path = db_path.with_extension("db-wal");
    let wal_before = file_size(&wal_path);
    let con = crate::db::open_conn(db_path, false)?;
    let mode = if truncate { "truncate" } else { "passive" };
    let pragma = if truncate {
        "PRAGMA wal_checkpoint(TRUNCATE)"
    } else {
        "PRAGMA wal_checkpoint(PASSIVE)"
    };
    let (busy, log_frames, checkpointed_frames): (i64, i64, i64) =
        con.query_row(pragma, [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let wal_after = file_size(&wal_path);
    let note = if busy != 0 {
        Some(
            "checkpoint could not complete while readers are active; retry or stop opencode"
                .to_string(),
        )
    } else if truncate && wal_after > 0 {
        Some("WAL not fully truncated; retry when the database is idle".to_string())
    } else {
        None
    };
    output::emit(&serde_json::to_value(CheckpointOut {
        env: env_status(db_path),
        mode,
        busy,
        log_frames,
        checkpointed_frames,
        wal_bytes_before: wal_before,
        wal_bytes_after: wal_after,
        note,
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb;

    #[test]
    fn checkpoint_reports_wal_state() {
        let dir = testdb::temp_data_dir("checkpoint");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);
        testdb::insert_session(&con, "s1", "/a", None);
        // Force a WAL write so there is something to checkpoint.
        let _: String = con
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .unwrap();
        testdb::insert_session(&con, "s2", "/a", None);
        drop(con);

        cmd_checkpoint(&db_path, true).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
