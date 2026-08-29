//! `session` subcommands: list, show, delete.

use crate::db::db_status;
use crate::error::{AppError, Result};
use crate::models::{filter_sessions, session_json};
use crate::output::print_json;
use crate::repo::{child_session_ids, load_sessions, resolve_session_ids, session_exists};
use rusqlite::{params, Connection};
use std::collections::HashSet;
use std::path::Path;

pub fn cmd_session_list(con: &Connection) -> Result<()> {
    let sessions = load_sessions(con)?;
    let arr: Vec<serde_json::Value> = sessions.iter().map(session_json).collect();
    print_json(&serde_json::json!(arr))
}

pub fn cmd_session_show(con: &Connection, args: &[String]) -> Result<()> {
    if args.len() != 1 {
        return Err(AppError::usage(
            "usage: opencode-dbtool session show <session-id>",
        ));
    }
    let id = resolve_session_ids(con, &[args[0].trim()])?
        .into_iter()
        .next()
        .unwrap();
    let sessions = load_sessions(con)?;
    let s = sessions
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| AppError::usage(format!("session not found: {id}")))?;
    print_json(&session_json(s))
}

pub fn cmd_session_delete(
    con: &mut Connection,
    args: &[String],
    dry_run: bool,
    db_path: &Path,
) -> Result<()> {
    let (dir, id_args) = parse_target_args(args)?;
    if dir.is_none() && id_args.is_empty() {
        return Err(AppError::usage(
            "usage: opencode-dbtool session delete <session-id> [session-id...] | --path <directory>",
        ));
    }
    let mut resolved: Vec<String> = Vec::new();
    if let Some(d) = &dir {
        if !id_args.is_empty() {
            return Err(AppError::usage("cannot combine session ids with --path"));
        }
        for s in filter_sessions(&load_sessions(con)?, Some(d)) {
            resolved.push(s.id.clone());
        }
        if resolved.is_empty() {
            return Err(AppError::usage(format!(
                "no sessions found in directory: {d}"
            )));
        }
    } else {
        resolved = resolve_session_ids(con, &id_args)?;
    }

    // Expand to child sessions (recursive subagent sessions), deduping
    // duplicate ids passed on the command line.
    let mut seen: HashSet<String> = HashSet::new();
    resolved.retain(|id| seen.insert(id.clone()));
    {
        let mut queue: Vec<String> = resolved.clone();
        while let Some(id) = queue.pop() {
            for c in child_session_ids(con, &id)? {
                if seen.insert(c.clone()) {
                    queue.push(c.clone());
                    resolved.push(c);
                }
            }
        }
    }

    let mut out = db_status(db_path);
    out["dry_run"] = serde_json::json!(dry_run);
    let mut total_rows: i64 = 0;
    let mut sessions_arr: Vec<serde_json::Value> = Vec::new();
    for id in &resolved {
        let counts = preview_counts(con, id)?;
        let mut rows_map = serde_json::Map::new();
        let mut total: i64 = 1; // the session row itself
        for (table, n) in counts {
            total += n;
            rows_map.insert(table, serde_json::json!(n));
        }
        total_rows += total;
        sessions_arr.push(serde_json::json!({
            "id": id,
            "rows": rows_map,
            "total": total,
        }));
    }
    out["total_rows"] = serde_json::json!(total_rows);
    out["sessions"] = serde_json::json!(sessions_arr);
    out["deleted"] = serde_json::json!(false);
    if dry_run {
        return print_json(&out);
    }

    con.execute_batch("PRAGMA foreign_keys = ON;")?;
    let tx = con.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for id in &resolved {
        // event tables have no FK to session, so they must be deleted
        // explicitly; the rest follows via ON DELETE CASCADE.
        tx.execute("DELETE FROM event WHERE aggregate_id = ?1", params![id])?;
        tx.execute(
            "DELETE FROM event_sequence WHERE aggregate_id = ?1",
            params![id],
        )?;
        tx.execute("DELETE FROM session WHERE id = ?1", params![id])?;
    }
    tx.commit()?;

    for id in &resolved {
        if session_exists(con, id)? {
            return Err(AppError::usage(format!(
                "session still exists after delete: {id}"
            )));
        }
    }
    out["deleted"] = serde_json::json!(true);
    out["note"] = serde_json::json!("file size is unchanged until `opencode-dbtool vacuum` is run");
    print_json(&out)
}

/// Parse `[--path <dir>] [ids...]`; combining both is rejected by the caller.
fn parse_target_args(args: &[String]) -> Result<(Option<String>, Vec<&str>)> {
    let mut dir: Option<String> = None;
    let mut id_args: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--path" {
            match args.get(i + 1) {
                Some(p) => dir = Some(p.clone()),
                None => return Err(AppError::usage("--path requires a directory")),
            }
            i += 2;
        } else {
            let id = args[i].trim();
            if !id.is_empty() {
                id_args.push(id);
            }
            i += 1;
        }
    }
    Ok((dir, id_args))
}

/// Per-table row counts for a session's delete preview.
fn preview_counts(con: &Connection, id: &str) -> Result<Vec<(String, i64)>> {
    let mut stmt = con.prepare(
        "SELECT 'message', COUNT(*) FROM message WHERE session_id = ?1 \
          UNION ALL SELECT 'part', COUNT(*) FROM part WHERE session_id = ?1 \
          UNION ALL SELECT 'todo', COUNT(*) FROM todo WHERE session_id = ?1 \
          UNION ALL SELECT 'event', COUNT(*) FROM event WHERE aggregate_id = ?1 \
          UNION ALL SELECT 'event_sequence', COUNT(*) FROM event_sequence WHERE aggregate_id = ?1 \
          UNION ALL SELECT 'session_share', COUNT(*) FROM session_share WHERE session_id = ?1 \
          UNION ALL SELECT 'session_input', COUNT(*) FROM session_input WHERE session_id = ?1 \
          UNION ALL SELECT 'session_message', COUNT(*) FROM session_message WHERE session_id = ?1 \
          UNION ALL SELECT 'session_context_epoch', COUNT(*) FROM session_context_epoch WHERE session_id = ?1",
    )?;
    let rows = stmt.query_map(params![id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::{cmd_session_delete, parse_target_args};
    use crate::testdb;
    use std::path::Path;

    #[test]
    fn delete_parent_removes_children_recursively() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "parent", "/a", None);
        testdb::insert_session(&con, "child", "/a", Some("parent"));
        testdb::insert_session(&con, "grandchild", "/a", Some("child"));
        testdb::insert_session(&con, "unrelated", "/b", None);

        cmd_session_delete(
            &mut con,
            &["parent".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 1);
        let remaining: String = con
            .query_row("SELECT id FROM session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, "unrelated");
    }

    #[test]
    fn delete_path_removes_children_in_other_dirs() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "parent", "/a", None);
        testdb::insert_session(&con, "child", "/b", Some("parent"));

        cmd_session_delete(
            &mut con,
            &["--path".to_string(), "/a".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 0);
    }

    #[test]
    fn delete_leaf_keeps_parent() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "parent", "/a", None);
        testdb::insert_session(&con, "child", "/a", Some("parent"));

        cmd_session_delete(
            &mut con,
            &["child".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 1);
        let remaining: String = con
            .query_row("SELECT id FROM session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, "parent");
    }

    #[test]
    fn parse_target_args_ok() {
        let args = vec![
            "--path".to_string(),
            "/a/b".to_string(),
            "ses_1".to_string(),
        ];
        let (dir, ids) = parse_target_args(&args).unwrap();
        assert_eq!(dir.as_deref(), Some("/a/b"));
        assert_eq!(ids, vec!["ses_1"]);
    }

    #[test]
    fn parse_target_args_dangling_path() {
        let args = vec!["ses_1".to_string(), "--path".to_string()];
        let err = parse_target_args(&args).unwrap_err();
        assert_eq!(err.code, 2);
        assert_eq!(err.message, "--path requires a directory");
    }
}
