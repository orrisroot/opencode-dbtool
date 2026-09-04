//! `project` subcommands: list, show, delete, purge.

use crate::db::{env_status, EnvStatus};
use crate::error::{AppError, Result};
use crate::models::{
    filter_projects, project_json, ProjectFilter, ProjectFilterJson, ProjectOut, ProjectRow,
};
use crate::output::print_json;
use crate::repo::{load_project, load_projects, lookup_project, project_impact};
use crate::util::{now_ms, parse_age_ms};
use rusqlite::{params, Connection};
use serde::Serialize;
use std::path::Path;

/// One session row in `project show`'s `session_list`.
#[derive(Serialize)]
struct SessionDetailOut {
    id: String,
    title: String,
    parent_id: Option<String>,
    updated: String,
    msgs: i64,
    parts: i64,
    events: i64,
    cost: f64,
}

/// One project in a delete/purge preview.
#[derive(Serialize)]
struct ProjectBriefOut {
    id: String,
    worktree: String,
}

#[derive(Serialize)]
struct ProjectDeleteOut {
    #[serde(flatten)]
    env: EnvStatus,
    dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    filters: Option<ProjectFilterJson>,
    total_rows: i64,
    projects: Vec<ProjectBriefOut>,
    rows: serde_json::Map<String, serde_json::Value>,
    deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff_files_removed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff_bytes_removed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

pub fn cmd_project_list(con: &Connection, args: &[String]) -> Result<()> {
    let mut paths: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--path" {
            match args.get(i + 1) {
                Some(p) => paths.push(p.as_str()),
                None => return Err(AppError::usage("--path requires a directory")),
            }
            i += 2;
        } else {
            return Err(AppError::usage(format!("unexpected argument: {}", args[i])));
        }
    }
    let projects = load_projects(con)?;
    let arr: Vec<ProjectOut> = filter_projects(&projects, &paths)
        .iter()
        .map(|p| project_json(p))
        .collect();
    print_json(&serde_json::to_value(arr)?)
}

pub fn cmd_project_show(con: &Connection, args: &[String]) -> Result<()> {
    if args.len() != 1 {
        return Err(AppError::usage(
            "usage: opencode-dbtool project show <project-id>",
        ));
    }
    let id = args[0].trim();
    let proj = load_project(con, id)?
        .ok_or_else(|| AppError::usage(format!("project not found: {id}")))?;
    print_json(&project_detail(con, proj)?)
}

/// Project object plus per-session breakdown.
fn project_detail(con: &Connection, full: ProjectRow) -> Result<serde_json::Value> {
    let mut out = serde_json::to_value(project_json(&full))?;
    let sessions: Vec<SessionDetailOut> = {
        let mut stmt = con.prepare(
            "SELECT id, title, parent_id, time_updated, \
             (SELECT COUNT(*) FROM message m WHERE m.session_id = s.id), \
             (SELECT COUNT(*) FROM part p WHERE p.session_id = s.id), \
             (SELECT COUNT(*) FROM event e WHERE e.aggregate_id = s.id), \
             s.cost \
             FROM session s WHERE s.project_id = ?1 ORDER BY s.time_updated DESC",
        )?;
        let rows = stmt.query_map(params![full.id], |r| {
            Ok(SessionDetailOut {
                id: r.get(0)?,
                title: r.get(1)?,
                parent_id: r.get(2)?,
                updated: crate::util::dt(r.get::<_, i64>(3)?),
                msgs: r.get(4)?,
                parts: r.get(5)?,
                events: r.get(6)?,
                cost: crate::util::round4(r.get::<_, f64>(7)?),
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    out["session_list"] = serde_json::to_value(sessions)?;
    Ok(out)
}

pub fn cmd_project_delete(
    con: &mut Connection,
    args: &[String],
    dry_run: bool,
    db_path: &Path,
) -> Result<()> {
    let ids = parse_id_args(args)?;
    if ids.is_empty() {
        return Err(AppError::usage(
            "usage: opencode-dbtool project delete <project-id> [project-id...]",
        ));
    }
    let mut projects: Vec<ProjectRow> = Vec::new();
    for id in &ids {
        match lookup_project(con, id, false)? {
            Some(p) if !projects.iter().any(|x| x.id == p.id) => projects.push(p),
            Some(_) => {}
            None => return Err(AppError::usage(format!("project not found: {id}"))),
        }
    }
    delete_output(con, &projects, dry_run, db_path, None)
}

/// Delete the projects selected by filters. At least one filter is
/// required.
pub fn cmd_project_purge(
    con: &mut Connection,
    args: &[String],
    dry_run: bool,
    db_path: &Path,
) -> Result<()> {
    let filters = parse_purge_args(args)?;
    if filters.is_empty() {
        return Err(AppError::usage(
            "usage: opencode-dbtool project purge [--older-than <age>] [--path <dir>...]",
        ));
    }
    let projects: Vec<ProjectRow> = load_projects(con)?
        .into_iter()
        .filter(|p| filters.matches(p))
        .collect();
    delete_output(con, &projects, dry_run, db_path, Some(&filters))
}

/// Parse `project delete` args: ids only; flags are rejected.
fn parse_id_args(args: &[String]) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for a in args {
        let id = a.trim();
        if id.is_empty() {
            continue;
        }
        if id == "--path" {
            return Err(AppError::usage(
                "`--path` was removed; use `project purge --path <dir>`",
            ));
        }
        if id.starts_with("--") {
            return Err(AppError::usage(format!("unknown option: {id}")));
        }
        ids.push(id.to_string());
    }
    Ok(ids)
}

/// Parse purge filter flags: `--older-than <age>`, `--path <dir>`
/// (repeatable). Positional args and `--subagents` are rejected.
fn parse_purge_args(args: &[String]) -> Result<ProjectFilter> {
    let mut f = ProjectFilter {
        older_than_raw: None,
        cutoff_ms: None,
        paths: Vec::new(),
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--older-than" => {
                let age = args
                    .get(i + 1)
                    .ok_or_else(|| AppError::usage("--older-than requires an age (e.g. 30d)"))?;
                let ms = parse_age_ms(age)?;
                f.older_than_raw = Some(age.clone());
                f.cutoff_ms = Some(now_ms()? - ms);
                i += 2;
            }
            "--path" => {
                let p = args
                    .get(i + 1)
                    .ok_or_else(|| AppError::usage("--path requires a directory"))?;
                f.paths.push(p.clone());
                i += 2;
            }
            "--subagents" | "--larger-than" | "--keep-latest" => {
                return Err(AppError::usage(format!(
                    "{} does not apply to projects",
                    args[i]
                )));
            }
            other => return Err(AppError::usage(format!("unknown option: {other}"))),
        }
    }
    Ok(f)
}

/// Delete the given projects: preview/execute, with an optional purge
/// filter block (`action`/`filters`) in the output.
fn delete_output(
    con: &mut Connection,
    projects: &[ProjectRow],
    dry_run: bool,
    db_path: &Path,
    filters: Option<&ProjectFilter>,
) -> Result<()> {
    let projects_json: Vec<ProjectBriefOut> = projects
        .iter()
        .map(|p| ProjectBriefOut {
            id: p.id.clone(),
            worktree: p.worktree.clone(),
        })
        .collect();
    let mut rows_map = serde_json::Map::new();
    let mut total: i64 = 0;
    for p in projects {
        for (table, n) in project_impact(con, &p.id)? {
            let cur = rows_map.get(&table).and_then(|v| v.as_i64()).unwrap_or(0);
            rows_map.insert(table, serde_json::json!(cur + n));
            total += n;
        }
    }
    let mut out = ProjectDeleteOut {
        env: env_status(db_path),
        dry_run,
        action: filters.map(|_| "delete".to_string()),
        filters: filters.map(|f| f.json()),
        total_rows: total,
        projects: projects_json,
        rows: rows_map,
        deleted: false,
        diff_files_removed: None,
        diff_bytes_removed: None,
        note: None,
    };
    if dry_run {
        return print_json(&serde_json::to_value(&out)?);
    }

    let plist: String = (1..=projects.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let project_ids: Vec<&str> = projects.iter().map(|p| p.id.as_str()).collect();
    let session_ids: Vec<String> = {
        let mut stmt = con.prepare(&format!(
            "SELECT id FROM session WHERE project_id IN ({plist})"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(&project_ids), |r| {
            r.get::<_, String>(0)
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    con.execute_batch("PRAGMA foreign_keys = ON;")?;
    let tx = con.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // event tables have no FK to session/project, so they must be deleted
    // explicitly; sessions and their rows follow via ON DELETE CASCADE.
    tx.execute(
        &format!("DELETE FROM event WHERE aggregate_id IN (SELECT id FROM session WHERE project_id IN ({plist}))"),
        rusqlite::params_from_iter(&project_ids),
    )?;
    tx.execute(
        &format!("DELETE FROM event_sequence WHERE aggregate_id IN (SELECT id FROM session WHERE project_id IN ({plist}))"),
        rusqlite::params_from_iter(&project_ids),
    )?;
    tx.execute(
        &format!("DELETE FROM project WHERE id IN ({plist})"),
        rusqlite::params_from_iter(&project_ids),
    )?;
    tx.commit()?;

    // Verify the full blast radius is gone (cascade may not cover every
    // table, so a project row disappearing alone is not proof of success).
    let remaining_projects: i64 = con.query_row(
        &format!("SELECT COUNT(*) FROM project WHERE id IN ({plist})"),
        rusqlite::params_from_iter(&project_ids),
        |r| r.get(0),
    )?;
    let remaining_sessions: i64 = con.query_row(
        &format!("SELECT COUNT(*) FROM session WHERE project_id IN ({plist})"),
        rusqlite::params_from_iter(&project_ids),
        |r| r.get(0),
    )?;
    let remaining_workspaces: i64 = con.query_row(
        &format!("SELECT COUNT(*) FROM workspace WHERE project_id IN ({plist})"),
        rusqlite::params_from_iter(&project_ids),
        |r| r.get(0),
    )?;
    if remaining_projects > 0 || remaining_sessions > 0 || remaining_workspaces > 0 {
        return Err(AppError::usage(
            "some project data still exists after delete",
        ));
    }
    let (diff_files, diff_bytes) = remove_diff_files(db_path, &session_ids);
    out.deleted = true;
    out.diff_files_removed = Some(diff_files);
    out.diff_bytes_removed = Some(diff_bytes);
    out.note = Some("file size is unchanged until `opencode-dbtool vacuum` is run".into());
    print_json(&serde_json::to_value(&out)?)
}

/// Best-effort removal of `storage/session_diff/<id>.json` files;
/// missing files are ignored. Returns (files removed, bytes removed).
fn remove_diff_files(db_path: &Path, ids: &[String]) -> (usize, u64) {
    let dir = crate::util::session_diff_dir(db_path);
    let mut removed = 0;
    let mut bytes = 0;
    for id in ids {
        let p = dir.join(format!("{id}.json"));
        if let Ok(meta) = std::fs::metadata(&p) {
            if std::fs::remove_file(&p).is_ok() {
                removed += 1;
                bytes += meta.len();
            }
        }
    }
    (removed, bytes)
}

#[cfg(test)]
mod tests {
    use super::{cmd_project_delete, cmd_project_purge, parse_id_args, parse_purge_args};
    use crate::testdb;
    use std::path::Path;

    #[test]
    fn delete_removes_project_and_sessions() {
        let mut con = testdb::create();
        testdb::insert_project(&con, "p1", "/a");
        testdb::insert_project_session(&con, "s1", "/a", "p1", 0);

        cmd_project_delete(&mut con, &["p1".to_string()], false, Path::new("/tmp/x.db")).unwrap();

        assert_eq!(testdb::project_count(&con), 0);
        assert_eq!(testdb::session_count(&con), 0);
    }

    #[test]
    fn delete_rejects_path() {
        let mut con = testdb::create();
        testdb::insert_project(&con, "p1", "/a");

        let err = cmd_project_delete(
            &mut con,
            &["--path".to_string(), "/a".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap_err();
        assert_eq!(err.code, 2);
        assert!(err.message.contains("purge"));
    }

    #[test]
    fn purge_path_removes_all_duplicate_worktree_projects() {
        let mut con = testdb::create();
        for (id, worktree) in [("p1", "/a"), ("p2", "/a"), ("p3", "/b")] {
            testdb::insert_project(&con, id, worktree);
        }

        cmd_project_purge(
            &mut con,
            &["--path".to_string(), "/a".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::project_count(&con), 1);
        let remaining: String = con
            .query_row("SELECT id FROM project", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, "p3");
    }

    #[test]
    fn purge_no_filters_is_usage_error() {
        let mut con = testdb::create();
        testdb::insert_project(&con, "p1", "/a");

        let err = cmd_project_purge(&mut con, &[], false, Path::new("/tmp/x.db")).unwrap_err();
        assert_eq!(err.code, 2);
    }

    #[test]
    fn purge_older_than_uses_latest_session_activity() {
        let mut con = testdb::create();
        let now = super::now_ms().unwrap();
        testdb::insert_project(&con, "inactive", "/a");
        testdb::insert_project_session(&con, "s-old", "/a", "inactive", 0);
        testdb::insert_project(&con, "active", "/b");
        testdb::insert_project_session(&con, "s-old1", "/b", "active", 0);
        testdb::insert_project_session(&con, "s-recent", "/b", "active", now);

        cmd_project_purge(
            &mut con,
            &["--older-than".to_string(), "30d".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::project_count(&con), 1);
        let remaining: String = con
            .query_row("SELECT id FROM project", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, "active");
        assert_eq!(testdb::session_count(&con), 2);
    }

    #[test]
    fn purge_dry_run_changes_nothing() {
        let mut con = testdb::create();
        testdb::insert_project(&con, "p1", "/a");

        cmd_project_purge(
            &mut con,
            &["--path".to_string(), "/a".to_string()],
            true,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::project_count(&con), 1);
    }

    #[test]
    fn parse_purge_args_ok() {
        let f = parse_purge_args(&[
            "--older-than".to_string(),
            "30d".to_string(),
            "--path".to_string(),
            "/a".to_string(),
        ])
        .unwrap();
        assert_eq!(f.older_than_raw.as_deref(), Some("30d"));
        assert!(f.cutoff_ms.is_some());
        assert_eq!(f.paths, vec!["/a"]);
    }

    #[test]
    fn parse_purge_args_rejects_bad_input() {
        for args in [
            vec!["--older-than".to_string()],
            vec!["--older-than".to_string(), "xyz".to_string()],
            vec!["--path".to_string()],
            vec!["--subagents".to_string()],
            vec!["p1".to_string()],
            vec!["--unknown".to_string()],
        ] {
            assert!(parse_purge_args(&args).is_err(), "should reject: {args:?}");
        }
    }

    #[test]
    fn delete_removes_session_diff_files() {
        let dir = testdb::temp_data_dir("proj-diff");
        let db_path = dir.join("opencode.db");
        let mut con = testdb::create_at(&db_path);
        testdb::insert_project(&con, "p1", "/a");
        testdb::insert_project_session(&con, "s1", "/a", "p1", 0);
        testdb::insert_project_session(&con, "s2", "/a", "p1", 0);
        let diff = dir.join("storage/session_diff");
        std::fs::create_dir_all(&diff).unwrap();
        std::fs::write(diff.join("s1.json"), vec![0u8; 4]).unwrap();
        std::fs::write(diff.join("s2.json"), vec![0u8; 6]).unwrap();
        std::fs::write(diff.join("other.json"), vec![0u8; 8]).unwrap();

        cmd_project_delete(&mut con, &["p1".to_string()], false, &db_path).unwrap();

        assert!(!diff.join("s1.json").exists());
        assert!(!diff.join("s2.json").exists());
        assert!(diff.join("other.json").exists(), "unrelated file kept");
        drop(con);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parse_id_args_ok() {
        assert_eq!(
            parse_id_args(&["p1".to_string(), "  p2  ".to_string()]).unwrap(),
            vec!["p1", "p2"]
        );
        assert!(parse_id_args(&[]).unwrap().is_empty());
    }
}
