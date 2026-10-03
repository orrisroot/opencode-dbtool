//! `doctor` command: integrity and consistency checks (V2-only).

use crate::db::{env_status, file_size, quick_check, EnvStatus, SESSION_TABLE};
use crate::error::{AppError, Result};
use crate::output::print_json;
use crate::util::expect_no_args;
use rusqlite::Connection;
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
struct FkViolation {
    table: String,
    rowid: i64,
    ref_table: String,
    fk_id: i64,
}

#[derive(Serialize)]
struct MissingParent {
    id: String,
    parent_id: String,
    title: String,
}

#[derive(Serialize)]
struct DanglingRef {
    id: String,
    ref_id: String,
}

#[derive(Serialize)]
struct BlobOrphan {
    hash: String,
    bytes: i64,
}

#[derive(Serialize)]
struct Orphans {
    sessions_missing_parent: Vec<MissingParent>,
    sessions_dangling_fork: Vec<DanglingRef>,
    sessions_missing_workspace: Vec<DanglingRef>,
    orphaned_event_sequences: Vec<String>,
    orphan_instruction_blobs: Vec<BlobOrphan>,
}

#[derive(Serialize)]
struct DoctorOut {
    #[serde(flatten)]
    env: EnvStatus,
    db_bytes: u64,
    wal_bytes: u64,
    quick_check: String,
    integrity_check: String,
    foreign_key_violations: Vec<FkViolation>,
    orphans: Orphans,
    ok: bool,
}

pub fn cmd_doctor(con: &Connection, db_path: &Path, args: &[String]) -> Result<()> {
    expect_no_args(args, "doctor")?;
    let out = doctor_out(con, db_path)?;
    let v = serde_json::to_value(&out)?;
    print_json(&v)?;
    if !out.ok {
        return Err(AppError::db("integrity problems found (see JSON output)"));
    }
    Ok(())
}

/// Build the doctor result (exposed for tests).
fn doctor_out(con: &Connection, db_path: &Path) -> Result<DoctorOut> {
    let env = env_status(db_path);
    let quick = quick_check(con);
    let integrity: String = con
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap_or_else(|e| e.to_string());

    let fk_violations: Vec<FkViolation> = {
        let mut stmt = con.prepare("PRAGMA foreign_key_check")?;
        let rows = stmt.query_map([], |r| {
            Ok(FkViolation {
                table: r.get(0)?,
                rowid: r.get(1)?,
                ref_table: r.get(2)?,
                fk_id: r.get(3)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };

    // References without FK constraints.
    let sessions_missing_parent: Vec<MissingParent> = {
        let sql = format!(
            "SELECT s.id, s.parent_id, COALESCE(s.title,'') FROM \"{SESSION_TABLE}\" s \
             WHERE s.parent_id IS NOT NULL AND s.parent_id != '' \
               AND s.parent_id NOT IN (SELECT id FROM \"{SESSION_TABLE}\") ORDER BY s.id"
        );
        let mut stmt = con.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            Ok(MissingParent {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                title: r.get(2)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    // fork_session_id carries no FK: a deleted fork source leaves a
    // dangling reference behind.
    let sessions_dangling_fork: Vec<DanglingRef> = {
        let sql = format!(
            "SELECT s.id, s.fork_session_id FROM \"{SESSION_TABLE}\" s \
             WHERE s.fork_session_id IS NOT NULL AND s.fork_session_id != '' \
               AND s.fork_session_id NOT IN (SELECT id FROM \"{SESSION_TABLE}\") ORDER BY s.id"
        );
        let mut stmt = con.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            Ok(DanglingRef {
                id: r.get(0)?,
                ref_id: r.get(1)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    // workspace_id carries no FK and `workspace` is provider-scoped.
    let sessions_missing_workspace: Vec<DanglingRef> = {
        let sql = format!(
            "SELECT s.id, s.workspace_id FROM \"{SESSION_TABLE}\" s \
             WHERE s.workspace_id IS NOT NULL AND s.workspace_id != '' \
               AND s.workspace_id NOT IN (SELECT id FROM workspace) ORDER BY s.id"
        );
        let mut stmt = con.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            Ok(DanglingRef {
                id: r.get(0)?,
                ref_id: r.get(1)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    // Event aggregates are sessions or projects; anything else is orphaned.
    let orphaned_event_sequences: Vec<String> = {
        let sql = format!(
            "SELECT DISTINCT aggregate_id FROM event_sequence \
             WHERE aggregate_id NOT IN (SELECT id FROM \"{SESSION_TABLE}\") \
               AND aggregate_id NOT IN (SELECT id FROM project) ORDER BY 1 LIMIT 1000"
        );
        let mut stmt = con.prepare(&sql)?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    // V2 content rows whose session is gone. FK covers them, but explicit
    // counts help when FKs were off.
    let mut v2_orphans: i64 = 0;
    for (table, id_col) in [
        ("session_message", "session_id"),
        ("session_inbox", "session_id"),
        ("session_pending", "session_id"),
        ("instruction_entry", "session_id"),
        ("instruction_state", "session_id"),
    ] {
        let sql = format!(
            "SELECT COUNT(*) FROM \"{table}\" t WHERE t.\"{id_col}\" NOT IN (SELECT id FROM \"{SESSION_TABLE}\")"
        );
        let n: i64 = con.query_row(&sql, [], |r| r.get(0))?;
        v2_orphans += n;
    }
    let orphans = Orphans {
        sessions_missing_parent,
        sessions_dangling_fork,
        sessions_missing_workspace,
        orphaned_event_sequences,
        orphan_instruction_blobs: crate::repo::blob_orphans(con)?
            .into_iter()
            .map(|(hash, bytes)| BlobOrphan { hash, bytes })
            .collect(),
    };

    let ok = quick == "ok"
        && integrity == "ok"
        && fk_violations.is_empty()
        && orphans.sessions_missing_parent.is_empty()
        && orphans.sessions_dangling_fork.is_empty()
        && orphans.sessions_missing_workspace.is_empty()
        && orphans.orphaned_event_sequences.is_empty()
        && orphans.orphan_instruction_blobs.is_empty()
        && v2_orphans == 0;

    Ok(DoctorOut {
        env,
        db_bytes: file_size(db_path),
        wal_bytes: file_size(&db_path.with_extension("db-wal")),
        quick_check: quick,
        integrity_check: integrity,
        foreign_key_violations: fk_violations,
        orphans,
        ok,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb;

    #[test]
    fn healthy_db_reports_ok() {
        let con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        cmd_doctor(&con, Path::new("/tmp/x.db"), &[]).unwrap();
    }

    #[test]
    fn doctor_output_contract() {
        let con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        let mut out = doctor_out(&con, Path::new("/tmp/x.db")).unwrap();
        out.env = crate::db::EnvStatus {
            opencode_running: Some(false),
            pids: Some(vec![]),
            pid_error: None,
            db: "/tmp/x.db".into(),
        };
        let v = serde_json::to_value(&out).unwrap();
        let expected = serde_json::json!({
            "opencode_running": false,
            "pids": [],
            "db": "/tmp/x.db",
            "db_bytes": 0,
            "wal_bytes": 0,
            "quick_check": "ok",
            "integrity_check": "ok",
            "foreign_key_violations": [],
            "orphans": {
                "sessions_missing_parent": [],
                "sessions_dangling_fork": [],
                "sessions_missing_workspace": [],
                "orphaned_event_sequences": [],
                "orphan_instruction_blobs": []
            },
            "ok": true
        });
        assert_eq!(v, expected, "doctor JSON contract changed");
    }

    #[test]
    fn project_aggregate_is_not_orphaned() {
        let con = testdb::create();
        con.execute(
            "INSERT INTO project (id, worktree, name) VALUES ('p1','/a','p1')",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO event_sequence (aggregate_id, seq) VALUES ('p1', 1)",
            [],
        )
        .unwrap();
        cmd_doctor(&con, Path::new("/tmp/x.db"), &[]).unwrap();
    }

    #[test]
    fn dangling_fork_and_workspace_are_detected() {
        let con = testdb::create();
        con.execute(
            "INSERT INTO session_v2 (id, directory, title, fork_session_id, workspace_id, time_updated, cost) \
             VALUES ('s1', '/a', 't', 'missing-parent', 'missing-ws', 0, 0)",
            [],
        )
        .unwrap();
        assert!(cmd_doctor(&con, Path::new("/tmp/x.db"), &[]).is_err());
    }

    #[test]
    fn orphan_blobs_fail_doctor_until_cleaned() {
        let con = testdb::create();
        con.execute("INSERT INTO instruction_blob (hash, value) VALUES ('zzz', 'orphan')", [])
            .unwrap();
        assert!(cmd_doctor(&con, Path::new("/tmp/x.db"), &[]).is_err());
    }
}
