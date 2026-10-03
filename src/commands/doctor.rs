//! `doctor` command: integrity and consistency checks (V2-only).

use crate::db::{env_status, file_size, quick_check, EnvStatus, SESSION_TABLE};
use crate::error::{AppError, Result};
use crate::output;
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
    #[serde(skip_serializing_if = "Option::is_none")]
    fix: Option<FixReport>,
}

/// What `doctor --fix` repaired (or would repair, in dry-run mode).
#[derive(Serialize)]
struct FixReport {
    dry_run: bool,
    blobs_deleted: usize,
    event_rows_deleted: i64,
    event_sequences_deleted: usize,
    parents_cleared: usize,
    forks_cleared: usize,
    workspaces_cleared: usize,
}

pub fn cmd_doctor(con: &mut Connection, db_path: &Path, fix: bool, dry_run: bool) -> Result<()> {
    let mut out = doctor_out(con, db_path)?;
    if fix {
        if dry_run {
            out.fix = Some(FixReport {
                dry_run: true,
                blobs_deleted: out.orphans.orphan_instruction_blobs.len(),
                event_rows_deleted: crate::repo::orphan_event_row_count(con)?,
                event_sequences_deleted: out.orphans.orphaned_event_sequences.len(),
                parents_cleared: out.orphans.sessions_missing_parent.len(),
                forks_cleared: out.orphans.sessions_dangling_fork.len(),
                workspaces_cleared: out.orphans.sessions_missing_workspace.len(),
            });
        } else {
            let report = apply_fixes(con)?;
            // Re-diagnose so `ok` reflects the repaired state.
            out = doctor_out(con, db_path)?;
            out.fix = Some(report);
        }
    }
    let v = serde_json::to_value(&out)?;
    output::emit(&v)?;
    if !out.ok {
        return Err(AppError::db("integrity problems found (see JSON output)"));
    }
    Ok(())
}

/// Apply every conservative repair `doctor` knows about. Integrity/FK
/// failures are never touched.
fn apply_fixes(con: &mut Connection) -> Result<FixReport> {
    let (blobs, _bytes) = crate::repo::delete_blob_orphans(con)?;
    let (event_rows, sequences) = crate::repo::delete_orphan_event_sequences(con)?;
    let parents = crate::repo::clear_missing_parents(con)?;
    let forks = crate::repo::clear_dangling_forks(con)?;
    let workspaces = crate::repo::clear_missing_workspaces(con)?;
    Ok(FixReport {
        dry_run: false,
        blobs_deleted: blobs,
        event_rows_deleted: event_rows as i64,
        event_sequences_deleted: sequences,
        parents_cleared: parents,
        forks_cleared: forks,
        workspaces_cleared: workspaces,
    })
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
        fix: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb;

    #[test]
    fn healthy_db_reports_ok() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        cmd_doctor(&mut con, Path::new("/tmp/x.db"), false, false).unwrap();
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
        let mut con = testdb::create();
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
        cmd_doctor(&mut con, Path::new("/tmp/x.db"), false, false).unwrap();
    }

    #[test]
    fn dangling_fork_and_workspace_are_detected() {
        let mut con = testdb::create();
        con.execute(
            "INSERT INTO session_v2 (id, directory, title, fork_session_id, workspace_id, time_updated, cost) \
             VALUES ('s1', '/a', 't', 'missing-parent', 'missing-ws', 0, 0)",
            [],
        )
        .unwrap();
        assert!(cmd_doctor(&mut con, Path::new("/tmp/x.db"), false, false).is_err());
    }

    #[test]
    fn orphan_blobs_fail_doctor_until_cleaned() {
        let mut con = testdb::create();
        con.execute(
            "INSERT INTO instruction_blob (hash, value) VALUES ('zzz', 'orphan')",
            [],
        )
        .unwrap();
        assert!(cmd_doctor(&mut con, Path::new("/tmp/x.db"), false, false).is_err());
    }

    #[test]
    fn fix_repairs_orphans_and_dangling_references() {
        let mut con = testdb::create();
        con.execute(
            "INSERT INTO instruction_blob (hash, value) VALUES ('zzz', 'orphan')",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO event (aggregate_id, type, data) VALUES ('gone', 'x', '{}')",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO event_sequence (aggregate_id, seq) VALUES ('gone', 1)",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO session_v2 (id, directory, title, parent_id, fork_session_id, workspace_id, time_updated, cost) \
             VALUES ('s1', '/a', 't', 'missing', 'missing-fork', 'missing-ws', 0, 0)",
            [],
        )
        .unwrap();

        // Dry-run reports the plan but changes nothing.
        assert!(cmd_doctor(&mut con, Path::new("/tmp/x.db"), true, true).is_err());
        let blobs: i64 = con
            .query_row("SELECT COUNT(*) FROM instruction_blob", [], |r| r.get(0))
            .unwrap();
        assert_eq!(blobs, 1, "dry-run keeps the blob");

        cmd_doctor(&mut con, Path::new("/tmp/x.db"), true, false).unwrap();
        let blobs: i64 = con
            .query_row("SELECT COUNT(*) FROM instruction_blob", [], |r| r.get(0))
            .unwrap();
        assert_eq!(blobs, 0);
        let events: i64 = con
            .query_row("SELECT COUNT(*) FROM event", [], |r| r.get(0))
            .unwrap();
        let sequences: i64 = con
            .query_row("SELECT COUNT(*) FROM event_sequence", [], |r| r.get(0))
            .unwrap();
        assert_eq!(events, 0);
        assert_eq!(sequences, 0);
        let parent: Option<String> = con
            .query_row(
                "SELECT parent_id FROM session_v2 WHERE id = 's1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(parent, None);

        // Now healthy without any fixes.
        cmd_doctor(&mut con, Path::new("/tmp/x.db"), false, false).unwrap();
    }
}
