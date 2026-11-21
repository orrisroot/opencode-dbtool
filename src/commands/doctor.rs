//! `doctor` command: integrity and consistency checks (V2-only).

use crate::db::{
    db_dir, env_status, file_size, pragma_check, quick_check, wal_path, EnvStatus, SESSION_TABLE,
};
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
struct TableOrphanCount {
    table: String,
    count: i64,
}

/// Backup presence/freshness (advisory; never affects `ok`).
#[derive(Serialize)]
struct BackupsCheck {
    count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    newest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    newest_created: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

#[derive(Serialize)]
struct Orphans {
    sessions_missing_parent: Vec<MissingParent>,
    sessions_dangling_fork: Vec<DanglingRef>,
    sessions_missing_workspace: Vec<DanglingRef>,
    orphaned_event_sequences: Vec<String>,
    orphan_instruction_blobs: Vec<BlobOrphan>,
    orphan_media_blobs: Vec<BlobOrphan>,
    orphaned_jobs: Vec<String>,
    orphaned_automations: Vec<String>,
    orphaned_search_documents: Vec<String>,
    orphaned_session_rows: Vec<TableOrphanCount>,
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
    free_bytes: u64,
    backups: BackupsCheck,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    fix: Option<FixReport>,
}

/// What `doctor --fix` repaired (or would repair, in dry-run mode).
#[derive(Serialize)]
struct FixReport {
    dry_run: bool,
    blobs_deleted: usize,
    media_blobs_deleted: usize,
    jobs_deleted: usize,
    automations_deleted: usize,
    search_documents_deleted: usize,
    event_rows_deleted: i64,
    event_sequences_deleted: usize,
    parents_cleared: usize,
    forks_cleared: usize,
    workspaces_cleared: usize,
}

pub fn cmd_doctor(
    con: &mut Connection,
    db_path: &Path,
    fix: bool,
    max_backup_age: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    let max_backup_age_ms = match max_backup_age {
        Some(age) => Some(crate::util::now_ms()? - crate::util::parse_age_ms(age)?),
        None => None,
    };
    let mut out = doctor_out(con, db_path, max_backup_age_ms)?;
    if fix {
        if dry_run {
            out.fix = Some(FixReport {
                dry_run: true,
                blobs_deleted: out.orphans.orphan_instruction_blobs.len(),
                media_blobs_deleted: out.orphans.orphan_media_blobs.len(),
                jobs_deleted: out.orphans.orphaned_jobs.len(),
                automations_deleted: out.orphans.orphaned_automations.len(),
                search_documents_deleted: out.orphans.orphaned_search_documents.len(),
                event_rows_deleted: crate::repo::orphan_event_row_count(con)?,
                event_sequences_deleted: crate::repo::orphan_event_sequence_count(con)?,
                parents_cleared: out.orphans.sessions_missing_parent.len(),
                forks_cleared: out.orphans.sessions_dangling_fork.len(),
                workspaces_cleared: out.orphans.sessions_missing_workspace.len(),
            });
        } else {
            let report = apply_fixes(con)?;
            // Re-diagnose so `ok` reflects the repaired state.
            out = doctor_out(con, db_path, max_backup_age_ms)?;
            out.fix = Some(report);
        }
    }
    if let Some(warning) = &out.backups.warning {
        if output::progress_enabled() {
            eprintln!("warning: {warning}");
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
    let kinds = crate::repo::blob_orphans(con)?.into_iter().fold(
        (0usize, 0usize),
        |(instruction, media), blob| {
            if blob.kind == crate::repo::BlobKind::Instruction {
                (instruction + 1, media)
            } else {
                (instruction, media + 1)
            }
        },
    );
    let (_blobs, _bytes) = crate::repo::delete_blob_orphans(con)?;
    let jobs = crate::repo::delete_orphan_jobs(con)?;
    let automations = crate::repo::delete_orphan_automations(con)?;
    let search_docs = crate::repo::delete_orphan_search_documents(con)?;
    let (event_rows, sequences) = crate::repo::delete_orphan_event_sequences(con)?;
    let parents = crate::repo::clear_missing_parents(con)?;
    let forks = crate::repo::clear_dangling_forks(con)?;
    let workspaces = crate::repo::clear_missing_workspaces(con)?;
    Ok(FixReport {
        dry_run: false,
        blobs_deleted: kinds.0,
        media_blobs_deleted: kinds.1,
        jobs_deleted: jobs,
        automations_deleted: automations,
        search_documents_deleted: search_docs,
        event_rows_deleted: event_rows as i64,
        event_sequences_deleted: sequences,
        parents_cleared: parents,
        forks_cleared: forks,
        workspaces_cleared: workspaces,
    })
}

/// Build the doctor result (exposed for tests).
/// Doctor output as JSON (used by `cleanup --verify`).
pub fn doctor_value(con: &Connection, db_path: &Path) -> Result<serde_json::Value> {
    Ok(serde_json::to_value(doctor_out(con, db_path, None)?)?)
}

fn doctor_out(
    con: &Connection,
    db_path: &Path,
    max_backup_age_ms: Option<i64>,
) -> Result<DoctorOut> {
    let env = env_status(db_path);
    let quick = quick_check(con);
    let integrity = pragma_check(con, "integrity_check");

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
               AND NOT EXISTS (SELECT 1 FROM \"{SESSION_TABLE}\" p WHERE p.id = s.parent_id) ORDER BY s.id"
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
               AND NOT EXISTS (SELECT 1 FROM \"{SESSION_TABLE}\" f WHERE f.id = s.fork_session_id) ORDER BY s.id"
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
               AND NOT EXISTS (SELECT 1 FROM workspace w WHERE w.id = s.workspace_id) ORDER BY s.id"
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
            "SELECT DISTINCT es.aggregate_id FROM event_sequence es \
             WHERE NOT EXISTS (SELECT 1 FROM \"{SESSION_TABLE}\" s WHERE s.id = es.aggregate_id) \
               AND NOT EXISTS (SELECT 1 FROM project p WHERE p.id = es.aggregate_id) ORDER BY 1 LIMIT 1000"
        );
        let mut stmt = con.prepare(&sql)?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    // V2 content rows whose session is gone. FK covers them, but explicit
    // counts surface rows left behind when FKs were off (or in unlinked tables).
    let orphaned_session_rows: Vec<TableOrphanCount> = crate::repo::session_row_orphans(con)?
        .into_iter()
        .map(|(table, count)| TableOrphanCount { table, count })
        .collect();
    let (instruction_blobs, media_blobs): (Vec<BlobOrphan>, Vec<BlobOrphan>) =
        crate::repo::blob_orphans(con)?.into_iter().fold(
            (Vec::new(), Vec::new()),
            |(mut instruction, mut media), b| {
                let entry = BlobOrphan {
                    hash: b.hash,
                    bytes: b.bytes,
                };
                if b.kind == crate::repo::BlobKind::Instruction {
                    instruction.push(entry);
                } else {
                    media.push(entry);
                }
                (instruction, media)
            },
        );
    let orphaned_jobs = crate::repo::job_orphans(con)?;
    let orphaned_automations = crate::repo::automation_orphans(con)?;
    let orphaned_search_documents = crate::repo::search_document_orphans(con)?;
    let orphans = Orphans {
        sessions_missing_parent,
        sessions_dangling_fork,
        sessions_missing_workspace,
        orphaned_event_sequences,
        orphan_instruction_blobs: instruction_blobs,
        orphan_media_blobs: media_blobs,
        orphaned_jobs,
        orphaned_automations,
        orphaned_search_documents,
        orphaned_session_rows,
    };

    let ok = quick == "ok"
        && integrity == "ok"
        && fk_violations.is_empty()
        && orphans.sessions_missing_parent.is_empty()
        && orphans.sessions_dangling_fork.is_empty()
        && orphans.sessions_missing_workspace.is_empty()
        && orphans.orphaned_event_sequences.is_empty()
        && orphans.orphan_instruction_blobs.is_empty()
        && orphans.orphan_media_blobs.is_empty()
        && orphans.orphaned_jobs.is_empty()
        && orphans.orphaned_automations.is_empty()
        && orphans.orphaned_search_documents.is_empty()
        && orphans.orphaned_session_rows.is_empty();

    // Advisory backup freshness: a warning never changes `ok`.
    let backups = crate::commands::vacuum::list_backup_files(db_path, false)?;
    let newest = backups.first();
    let warning = max_backup_age_ms.and_then(|cutoff| match newest {
        None => Some("no backups found (run `opencode-dbtool backup --yes`)".to_string()),
        Some(b) => {
            let mtime = crate::util::file_mtime_ms(&db_dir(db_path).join(&b.file)).unwrap_or(0);
            (mtime < cutoff).then(|| {
                format!(
                    "newest backup {} is older than the configured age (run `opencode-dbtool backup --yes`)",
                    b.file
                )
            })
        }
    });
    let backups_check = BackupsCheck {
        count: backups.len(),
        newest: newest.map(|b| b.file.clone()),
        newest_created: newest.map(|b| b.created.clone()),
        warning,
    };
    let free_bytes = fs2::available_space(db_dir(db_path)).unwrap_or(0);

    Ok(DoctorOut {
        env,
        db_bytes: file_size(db_path),
        wal_bytes: file_size(&wal_path(db_path)),
        quick_check: quick,
        integrity_check: integrity,
        foreign_key_violations: fk_violations,
        orphans,
        free_bytes,
        backups: backups_check,
        ok,
        fix: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb;
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(stem: &str) -> Self {
            TempDir(testdb::temp_data_dir(stem))
        }

        fn db(&self) -> PathBuf {
            self.0.join("x.db")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn healthy_db_reports_ok() {
        let dir = TempDir::new("doctor-healthy");
        let mut con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        cmd_doctor(&mut con, &dir.db(), false, None, false).unwrap();
    }

    #[test]
    fn doctor_output_contract() {
        let dir = TempDir::new("doctor-contract");
        let con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        let mut out = doctor_out(&con, &dir.db(), None).unwrap();
        out.env = crate::db::EnvStatus {
            opencode_running: Some(false),
            pids: Some(vec![]),
            pid_error: None,
            db: "/tmp/x.db".into(),
        };
        // Free space is environment-dependent; normalize it.
        out.free_bytes = 0;
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
                "orphan_instruction_blobs": [],
                "orphan_media_blobs": [],
                "orphaned_jobs": [],
                "orphaned_automations": [],
                "orphaned_search_documents": [],
                "orphaned_session_rows": []
            },
            "free_bytes": 0,
            "backups": { "count": 0 },
            "ok": true
        });
        assert_eq!(v, expected, "doctor JSON contract changed");
    }

    #[test]
    fn project_aggregate_is_not_orphaned() {
        let dir = TempDir::new("doctor-project-agg");
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
        cmd_doctor(&mut con, &dir.db(), false, None, false).unwrap();
    }

    #[test]
    fn dangling_fork_and_workspace_are_detected() {
        let dir = TempDir::new("doctor-dangling");
        let mut con = testdb::create();
        con.execute(
            "INSERT INTO session_v2 (id, directory, title, fork_session_id, workspace_id, time_updated, cost) \
             VALUES ('s1', '/a', 't', 'missing-parent', 'missing-ws', 0, 0)",
            [],
        )
        .unwrap();
        let err = cmd_doctor(&mut con, &dir.db(), false, None, false).unwrap_err();
        assert!(
            err.message.contains("integrity problems found"),
            "got: {err}"
        );
    }

    #[test]
    fn orphan_blobs_fail_doctor_until_cleaned() {
        let dir = TempDir::new("doctor-blobs");
        let mut con = testdb::create();
        con.execute(
            "INSERT INTO instruction_blob (hash, value) VALUES ('zzz', 'orphan')",
            [],
        )
        .unwrap();
        let err = cmd_doctor(&mut con, &dir.db(), false, None, false).unwrap_err();
        assert!(
            err.message.contains("integrity problems found"),
            "got: {err}"
        );
    }

    #[test]
    fn fix_repairs_orphans_and_dangling_references() {
        let dir = TempDir::new("doctor-fix-unit");
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
        let err = cmd_doctor(&mut con, &dir.db(), true, None, true).unwrap_err();
        assert!(
            err.message.contains("integrity problems found"),
            "got: {err}"
        );
        let blobs: i64 = con
            .query_row("SELECT COUNT(*) FROM instruction_blob", [], |r| r.get(0))
            .unwrap();
        assert_eq!(blobs, 1, "dry-run keeps the blob");

        cmd_doctor(&mut con, &dir.db(), true, None, false).unwrap();
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
        cmd_doctor(&mut con, &dir.db(), false, None, false).unwrap();
    }

    #[test]
    fn backup_freshness_warning_is_advisory() {
        let dir = testdb::temp_data_dir("doctor-backup");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);
        let cutoff = Some(crate::util::now_ms().unwrap() - 86_400_000);

        // No backups: warning present, but `ok` stays true.
        let out = doctor_out(&con, &db_path, cutoff).unwrap();
        assert!(out.backups.warning.is_some(), "expected a warning");
        assert!(out.ok, "freshness is advisory");

        // A fresh backup clears the warning.
        std::fs::write(dir.join("opencode.db.backup-20260101-000000"), b"").unwrap();
        let out = doctor_out(&con, &db_path, cutoff).unwrap();
        assert_eq!(out.backups.count, 1);
        assert!(out.backups.warning.is_none());
        assert!(out.ok);

        drop(con);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn automation_and_fts_layouts_work_with_doctor_and_fix() {
        let dir = TempDir::new("doctor-latest-opencode");
        let mut con = testdb::create();
        testdb::insert_project(&con, "p1", "/a");
        testdb::insert_project_session(&con, "s1", "/a", "p1", 0);
        testdb::insert_session_message(&con, "m1", "s1", "user", r#"{"text":"hi"}"#);
        con.execute(
            "INSERT INTO machine (id, key, time_created) VALUES ('mc1', 'key1', 0)",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO account (id, email, url) VALUES ('acc1', 'a@example.com', 'https://opencode.ai')",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO account_state (id, active_account_id) VALUES (1, 'acc1')",
            [],
        )
        .unwrap();
        // Automation whose creating session was deleted is still healthy if project_id exists.
        con.execute(
            "INSERT INTO automation (id, project_id, session_id, name) VALUES ('a_keep', 'p1', 's_deleted', 'keep')",
            [],
        )
        .unwrap();
        // Compact FTS5 search index for live session + orphaned automation/doc.
        con.execute_batch(
            "CREATE TABLE session_search_document (id INTEGER PRIMARY KEY, source_id TEXT NOT NULL UNIQUE, session_id TEXT NOT NULL);
             CREATE VIRTUAL TABLE session_search USING fts5(title, summary, user_text, assistant_text, content='', contentless_delete=1);
             INSERT INTO session_search_document (id, source_id, session_id) VALUES (1, 's1', 's1'), (2, 'm1', 's1');
             INSERT INTO session_search (rowid, title, summary, user_text, assistant_text) VALUES (1, 's1', NULL, NULL, NULL), (2, NULL, NULL, 'hi', NULL);",
        )
        .unwrap();

        let out = doctor_out(&con, &dir.db(), None).unwrap();
        assert!(
            out.ok,
            "automation with deleted session_id and live FTS5 index must stay healthy"
        );

        // Now introduce orphaned automation (missing project_id) and orphaned search doc.
        con.execute(
            "INSERT INTO automation (id, project_id, session_id, name) VALUES ('a_orph', 'p_missing', NULL, 'drop')",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO session_search_document (id, source_id, session_id) VALUES (3, 's_gone', 's_gone')",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO session_search (rowid, title, summary, user_text, assistant_text) VALUES (3, 'dead', NULL, NULL, NULL)",
            [],
        )
        .unwrap();

        let out = doctor_out(&con, &dir.db(), None).unwrap();
        assert!(!out.ok);
        assert_eq!(out.orphans.orphaned_automations, vec!["a_orph"]);
        assert_eq!(out.orphans.orphaned_search_documents, vec!["s_gone"]);

        cmd_doctor(&mut con, &dir.db(), true, None, false).unwrap();
        let after = doctor_out(&con, &dir.db(), None).unwrap();
        assert!(after.ok);
        assert!(after.orphans.orphaned_automations.is_empty());
        assert!(after.orphans.orphaned_search_documents.is_empty());

        // External-content (`text`) FTS5 layout cleanup also triggers cleanly.
        let mut text_con = testdb::create();
        testdb::insert_session(&text_con, "s1", "/a", None);
        text_con
            .execute_batch(
                "CREATE TABLE session_search_document (id INTEGER PRIMARY KEY, source_id TEXT NOT NULL UNIQUE, session_id TEXT NOT NULL, title TEXT, summary TEXT, user_text TEXT, assistant_text TEXT);
                 CREATE VIRTUAL TABLE session_search USING fts5(title, summary, user_text, assistant_text, content='session_search_document', content_rowid='id');
                 CREATE TRIGGER session_search_document_insert AFTER INSERT ON session_search_document BEGIN INSERT INTO session_search (rowid, title, summary, user_text, assistant_text) VALUES (NEW.id, NEW.title, NEW.summary, NEW.user_text, NEW.assistant_text); END;
                 CREATE TRIGGER session_search_document_delete AFTER DELETE ON session_search_document BEGIN INSERT INTO session_search (session_search, rowid, title, summary, user_text, assistant_text) VALUES ('delete', OLD.id, OLD.title, OLD.summary, OLD.user_text, OLD.assistant_text); END;
                 INSERT INTO session_search_document (id, source_id, session_id, title) VALUES (1, 's1', 's1', 'live'), (2, 'm_gone', 's_gone', 'dead');",
            )
            .unwrap();
        cmd_doctor(&mut text_con, &dir.db(), true, None, false).unwrap();
        let fts_count: i64 = text_con
            .query_row("SELECT COUNT(*) FROM session_search", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, 1);
    }

    #[test]
    fn orphaned_session_rows_are_explicitly_reported() {
        let dir = TempDir::new("doctor-session-rows");
        let con = testdb::create();
        testdb::insert_session(&con, "ses_live", "/a", None);
        con.execute_batch(
            "PRAGMA foreign_keys = OFF;
             INSERT INTO session_message (id, session_id, type, data) VALUES ('m_orph', 'ses_gone', 'user', '{}');
             INSERT INTO session_part (message_id, session_id, ordinal, type, data) VALUES ('m_missing', 'ses_live', 0, 'text', '{}');
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
        let out = doctor_out(&con, &dir.db(), None).unwrap();
        assert!(!out.ok);
        assert_eq!(out.orphans.orphaned_session_rows.len(), 2);
        assert_eq!(
            out.orphans.orphaned_session_rows[0].table,
            "session_message"
        );
        assert_eq!(out.orphans.orphaned_session_rows[0].count, 1);
        assert_eq!(out.orphans.orphaned_session_rows[1].table, "session_part");
        assert_eq!(out.orphans.orphaned_session_rows[1].count, 1);
    }

    #[test]
    fn custom_extension_wal_and_relative_db_dir_are_measured() {
        let dir = TempDir::new("doctor-wal-path");
        let db_path = dir.0.join("custom.sqlite");
        let con = testdb::create_at(&db_path);
        std::fs::write(dir.0.join("custom.sqlite-wal"), vec![0u8; 128]).unwrap();

        let out = doctor_out(&con, &db_path, None).unwrap();
        assert_eq!(out.wal_bytes, 128);
        assert!(out.free_bytes > 0);
    }
}
