//! `project` subcommands: list, show, delete, purge.

use crate::db::{env_status, EnvStatus};
use crate::error::{AppError, Result};
use crate::models::{
    filter_projects, project_json, ProjectFilter, ProjectFilterJson, ProjectOut, ProjectRow,
};
use crate::output;
use crate::repo::{load_project, load_projects, project_impact, resolve_project};
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
    archived: bool,
    events: i64,
    session_messages: i64,
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
    note: Option<String>,
}

pub fn cmd_project_list(con: &Connection, paths: &[String]) -> Result<()> {
    let projects = load_projects(con)?;
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let arr: Vec<ProjectOut> = filter_projects(&projects, &refs)
        .iter()
        .map(|p| project_json(p))
        .collect();
    output::emit_cols(
        &serde_json::to_value(arr)?,
        &[
            "id",
            "worktree",
            "name",
            "sessions",
            "size_bytes",
            "cost",
            "updated",
        ],
    )
}

pub fn cmd_project_show(con: &Connection, reference: &str) -> Result<()> {
    let brief = resolve_project(con, reference)?;
    let proj = load_project(con, &brief.id)?
        .ok_or_else(|| AppError::usage(format!("project not found: {reference}")))?;
    output::emit(&project_detail(con, proj)?)
}

/// Project object plus per-session breakdown.
fn project_detail(con: &Connection, full: ProjectRow) -> Result<serde_json::Value> {
    use crate::db::SESSION_TABLE;
    let mut out = serde_json::to_value(project_json(&full))?;
    let sql = format!(
        "SELECT id, COALESCE(title,''), parent_id, time_updated, \
         time_archived IS NOT NULL, \
         (SELECT COUNT(*) FROM event e WHERE e.aggregate_id = s.id), \
         (SELECT COUNT(*) FROM session_message sm WHERE sm.session_id = s.id), \
         s.cost \
         FROM \"{SESSION_TABLE}\" s WHERE s.project_id = ?1 ORDER BY s.time_updated DESC"
    );
    let sessions: Vec<SessionDetailOut> = {
        let mut stmt = con.prepare(&sql)?;
        let rows = stmt.query_map(params![full.id], |r| {
            Ok(SessionDetailOut {
                id: r.get(0)?,
                title: r.get(1)?,
                parent_id: r.get(2)?,
                updated: crate::util::dt(r.get::<_, i64>(3)?),
                archived: r.get(4)?,
                events: r.get(5)?,
                session_messages: r.get(6)?,
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
    ids: &[String],
    dry_run: bool,
    db_path: &Path,
) -> Result<()> {
    let mut projects: Vec<ProjectRow> = Vec::new();
    for reference in ids {
        let p = resolve_project(con, reference)?;
        if !projects.iter().any(|x| x.id == p.id) {
            projects.push(p);
        }
    }
    delete_output(con, &projects, dry_run, db_path, None)
}

/// Delete the projects selected by filters. At least one filter is
/// required.
pub fn cmd_project_purge(
    con: &mut Connection,
    filters: &ProjectFilter,
    dry_run: bool,
    db_path: &Path,
) -> Result<()> {
    if filters.is_empty() {
        return Err(AppError::usage(
            "usage: opencode-dbtool project purge [--older-than <age>] [--path <dir>...] [--empty]",
        ));
    }
    let projects: Vec<ProjectRow> = load_projects(con)?
        .into_iter()
        .filter(|p| filters.matches(p))
        .collect();
    delete_output(con, &projects, dry_run, db_path, Some(filters))
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
    use crate::db::SESSION_TABLE;
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
        note: None,
    };
    if dry_run {
        return output::emit(&serde_json::to_value(&out)?);
    }

    let plist: String = (1..=projects.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let project_ids: Vec<&str> = projects.iter().map(|p| p.id.as_str()).collect();
    con.execute_batch("PRAGMA foreign_keys = ON;")?;
    let tx = con.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // event tables reference aggregates, not projects, so they must be
    // deleted explicitly; sessions and their rows follow via ON DELETE
    // CASCADE on the project row.
    tx.execute(
        &format!("DELETE FROM event WHERE aggregate_id IN (SELECT id FROM \"{SESSION_TABLE}\" WHERE project_id IN ({plist}))"),
        rusqlite::params_from_iter(&project_ids),
    )?;
    tx.execute(
        &format!("DELETE FROM event_sequence WHERE aggregate_id IN (SELECT id FROM \"{SESSION_TABLE}\" WHERE project_id IN ({plist}))"),
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
        &format!("SELECT COUNT(*) FROM \"{SESSION_TABLE}\" WHERE project_id IN ({plist})"),
        rusqlite::params_from_iter(&project_ids),
        |r| r.get(0),
    )?;
    if remaining_projects > 0 || remaining_sessions > 0 {
        return Err(AppError::usage(
            "some project data still exists after delete",
        ));
    }
    out.deleted = true;
    out.note = Some("file size is unchanged until `opencode-dbtool vacuum` is run".into());
    output::emit(&serde_json::to_value(&out)?)
}

#[cfg(test)]
mod tests {
    use super::{cmd_project_delete, cmd_project_purge};
    use crate::models::ProjectFilter;
    use crate::testdb;
    use std::path::Path;

    fn path_filter(path: &str) -> ProjectFilter {
        ProjectFilter {
            older_than_raw: None,
            cutoff_ms: None,
            paths: vec![path.to_string()],
            empty: false,
        }
    }

    fn no_filter() -> ProjectFilter {
        ProjectFilter {
            older_than_raw: None,
            cutoff_ms: None,
            paths: Vec::new(),
            empty: false,
        }
    }

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
    fn delete_accepts_worktree_and_unique_prefix() {
        let mut con = testdb::create();
        testdb::insert_project(&con, "p1", "/a");

        cmd_project_delete(&mut con, &["p".to_string()], true, Path::new("/tmp/x.db")).unwrap();
        cmd_project_delete(&mut con, &["/a".to_string()], true, Path::new("/tmp/x.db")).unwrap();
        assert_eq!(testdb::project_count(&con), 1, "dry-run changes nothing");

        cmd_project_delete(&mut con, &["/a".to_string()], false, Path::new("/tmp/x.db")).unwrap();
        assert_eq!(testdb::project_count(&con), 0);
    }

    #[test]
    fn purge_path_removes_all_duplicate_worktree_projects() {
        let mut con = testdb::create();
        for (id, worktree) in [("p1", "/a"), ("p2", "/a"), ("p3", "/b")] {
            testdb::insert_project(&con, id, worktree);
        }

        cmd_project_purge(&mut con, &path_filter("/a"), false, Path::new("/tmp/x.db")).unwrap();

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

        let err =
            cmd_project_purge(&mut con, &no_filter(), false, Path::new("/tmp/x.db")).unwrap_err();
        assert_eq!(err.code, 2);
    }

    #[test]
    fn purge_older_than_uses_latest_session_activity() {
        let mut con = testdb::create();
        let now = crate::util::now_ms().unwrap();
        testdb::insert_project(&con, "inactive", "/a");
        testdb::insert_project_session(&con, "s-old", "/a", "inactive", 0);
        testdb::insert_project(&con, "active", "/b");
        testdb::insert_project_session(&con, "s-old1", "/b", "active", 0);
        testdb::insert_project_session(&con, "s-recent", "/b", "active", now);

        let filters = ProjectFilter {
            older_than_raw: Some("30d".to_string()),
            cutoff_ms: Some(now - 30 * 86_400_000),
            paths: Vec::new(),
            empty: false,
        };
        cmd_project_purge(&mut con, &filters, false, Path::new("/tmp/x.db")).unwrap();

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

        cmd_project_purge(&mut con, &path_filter("/a"), true, Path::new("/tmp/x.db")).unwrap();

        assert_eq!(testdb::project_count(&con), 1);
    }

    #[test]
    fn purge_empty_selects_projects_without_sessions() {
        let mut con = testdb::create();
        testdb::insert_project(&con, "empty", "/a");
        testdb::insert_project(&con, "full", "/b");
        testdb::insert_project_session(&con, "s1", "/b", "full", 0);

        let filters = ProjectFilter {
            older_than_raw: None,
            cutoff_ms: None,
            paths: Vec::new(),
            empty: true,
        };
        cmd_project_purge(&mut con, &filters, false, Path::new("/tmp/x.db")).unwrap();

        assert_eq!(testdb::project_count(&con), 1);
        let remaining: String = con
            .query_row("SELECT id FROM project", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, "full");
    }
}
