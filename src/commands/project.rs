//! `project` subcommands: list, show, delete.

use crate::db::db_status;
use crate::error::{AppError, Result};
use crate::models::{filter_projects, project_json, ProjectRow};
use crate::output::print_json;
use crate::repo::{
    load_projects, lookup_project, lookup_projects_by_worktree, project_impact,
};
use crate::util::{dt, round4};
use rusqlite::{params, Connection};
use std::path::Path;

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
            return Err(AppError::usage(format!(
                "unexpected argument: {}",
                args[i]
            )));
        }
    }
    let projects = load_projects(con)?;
    let arr: Vec<serde_json::Value> = filter_projects(&projects, &paths)
        .iter()
        .map(|p| project_json(p))
        .collect();
    print_json(&serde_json::json!(arr))
}

pub fn cmd_project_show(con: &Connection, args: &[String]) -> Result<()> {
    if args.len() != 1 {
        return Err(AppError::usage(
            "usage: opencode-dbtool project show <project-id>",
        ));
    }
    let id = args[0].trim();
    let proj = lookup_project(con, id, false)?
        .ok_or_else(|| AppError::usage(format!("project not found: {id}")))?;
    let all = load_projects(con)?;
    print_json(&project_detail(con, &all, proj)?)
}

/// Project object plus per-session breakdown.
fn project_detail(
    con: &Connection,
    all: &[ProjectRow],
    proj: ProjectRow,
) -> Result<serde_json::Value> {
    let full = all.iter().find(|p| p.id == proj.id).cloned().unwrap_or(proj);
    let mut out = project_json(&full);
    let sessions: Vec<(String, String, Option<String>, i64, i64, i64, i64, f64)> = {
        let mut stmt = con.prepare(
            "SELECT id, title, parent_id, time_updated, \
             (SELECT COUNT(*) FROM message m WHERE m.session_id = s.id), \
             (SELECT COUNT(*) FROM part p WHERE p.session_id = s.id), \
             (SELECT COUNT(*) FROM event e WHERE e.aggregate_id = s.id), \
             s.cost \
             FROM session s WHERE s.project_id = ?1 ORDER BY s.time_updated DESC",
        )?;
        let rows = stmt.query_map(params![full.id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, f64>(7)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let sessions_arr: Vec<serde_json::Value> = sessions
        .iter()
        .map(|(sid, title, parent_id, updated, msgs, parts, events, cost)| {
            serde_json::json!({
                "id": sid,
                "title": title,
                "parent_id": parent_id,
                "updated": dt(*updated),
                "msgs": msgs,
                "parts": parts,
                "events": events,
                "cost": round4(*cost),
            })
        })
        .collect();
    out["session_list"] = serde_json::json!(sessions_arr);
    Ok(out)
}

pub fn cmd_project_delete(
    con: &mut Connection,
    args: &[String],
    dry_run: bool,
    db_path: &Path,
) -> Result<()> {
    let mut ids: Vec<&str> = Vec::new();
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
            ids.push(args[i].as_str());
            i += 1;
        }
    }
    if ids.is_empty() && paths.is_empty() {
        return Err(AppError::usage(
            "usage: opencode-dbtool project delete <project-id> [project-id...] | --path <directory>",
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
    for p in &paths {
        let matches = lookup_projects_by_worktree(con, p.trim_end_matches('/'))?;
        if matches.is_empty() {
            return Err(AppError::usage(format!("project not found: {p}")));
        }
        for row in matches {
            if !projects.iter().any(|x| x.id == row.id) {
                projects.push(row);
            }
        }
    }
    let projects_json: Vec<serde_json::Value> = projects
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "worktree": p.worktree,
            })
        })
        .collect();
    let mut rows_map = serde_json::Map::new();
    let mut total: i64 = 0;
    for p in &projects {
        for (table, n) in project_impact(con, &p.id)? {
            let cur = rows_map.get(&table).and_then(|v| v.as_i64()).unwrap_or(0);
            rows_map.insert(table, serde_json::json!(cur + n));
            total += n;
        }
    }
    let mut out = db_status(db_path);
    out["dry_run"] = serde_json::json!(dry_run);
    out["total_rows"] = serde_json::json!(total);
    out["projects"] = serde_json::json!(projects_json);
    out["rows"] = serde_json::json!(rows_map);
    out["deleted"] = serde_json::json!(false);
    if dry_run {
        return print_json(&out);
    }

    let plist: String = (1..=projects.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let project_ids: Vec<&str> = projects.iter().map(|p| p.id.as_str()).collect();
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
    out["deleted"] = serde_json::json!(true);
    out["note"] = serde_json::json!(
        "file size is unchanged until `opencode-dbtool vacuum` is run"
    );
    print_json(&out)
}

#[cfg(test)]
mod tests {
    use super::cmd_project_delete;
    use crate::testdb;
    use std::path::Path;

    #[test]
    fn delete_path_removes_all_duplicate_worktree_projects() {
        let mut con = testdb::create();
        for (id, worktree) in [("p1", "/a"), ("p2", "/a"), ("p3", "/b")] {
            testdb::insert_project(&con, id, worktree);
        }

        cmd_project_delete(
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
}