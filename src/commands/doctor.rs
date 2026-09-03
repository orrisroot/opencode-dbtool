//! `doctor` command: integrity and consistency checks.

use crate::db::{env_status, file_size, quick_check, EnvStatus};
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
struct MissingWorkspace {
    id: String,
    workspace_id: String,
}

#[derive(Serialize)]
struct Orphans {
    sessions_missing_parent: Vec<MissingParent>,
    sessions_missing_workspace: Vec<MissingWorkspace>,
    orphaned_event_sequences: Vec<String>,
    mismatched_parts: i64,
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
        let mut stmt = con.prepare(
            "SELECT s.id, s.parent_id, s.title FROM session s \
             WHERE s.parent_id IS NOT NULL AND s.parent_id != '' \
               AND s.parent_id NOT IN (SELECT id FROM session) ORDER BY s.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(MissingParent {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                title: r.get(2)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let sessions_missing_workspace: Vec<MissingWorkspace> = {
        let mut stmt = con.prepare(
            "SELECT s.id, s.workspace_id FROM session s \
             WHERE s.workspace_id IS NOT NULL AND s.workspace_id != '' \
               AND s.workspace_id NOT IN (SELECT id FROM workspace) ORDER BY s.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(MissingWorkspace {
                id: r.get(0)?,
                workspace_id: r.get(1)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let orphaned_event_sequences: Vec<String> = {
        let mut stmt = con.prepare(
            "SELECT DISTINCT aggregate_id FROM event_sequence \
             WHERE aggregate_id NOT IN (SELECT id FROM session) ORDER BY 1",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mismatched_parts: i64 = con.query_row(
        "SELECT COUNT(*) FROM part p \
         JOIN message m ON p.message_id = m.id \
         WHERE p.session_id != m.session_id",
        [],
        |r| r.get(0),
    )?;
    let orphans = Orphans {
        sessions_missing_parent,
        sessions_missing_workspace,
        orphaned_event_sequences,
        mismatched_parts,
    };

    let ok = quick == "ok"
        && integrity == "ok"
        && fk_violations.is_empty()
        && orphans.sessions_missing_parent.is_empty()
        && orphans.sessions_missing_workspace.is_empty()
        && orphans.orphaned_event_sequences.is_empty()
        && mismatched_parts == 0;

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
        // env is host-dependent; fix it for the golden comparison.
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
                "sessions_missing_workspace": [],
                "orphaned_event_sequences": [],
                "mismatched_parts": 0
            },
            "ok": true
        });
        assert_eq!(v, expected, "doctor JSON contract changed");
    }

    #[test]
    fn missing_workspace_is_detected() {
        let con = testdb::create();
        con.execute(
            "INSERT INTO session (id, directory, title, parent_id, workspace_id, time_updated, cost) \
             VALUES ('s1', '/a', 't', NULL, 'ws_missing', 0, 0)",
            [],
        )
        .unwrap();
        assert!(cmd_doctor(&con, Path::new("/tmp/x.db"), &[]).is_err());
    }
}
