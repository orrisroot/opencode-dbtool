//! `session` subcommands: list, show, delete, purge, strip-reasoning.

use crate::db::db_status;
use crate::error::{AppError, Result};
use crate::models::{session_json, PurgeFilter};
use crate::output::print_json;
use crate::repo::{
    child_session_ids, load_sessions, reasoning_counts, reasoning_left, resolve_session_ids,
    session_exists, strip_reasoning,
};
use crate::util::{now_ms, parse_age_ms, parse_count, parse_size_bytes};
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
    let ids = parse_id_args(args)?;
    if ids.is_empty() {
        return Err(AppError::usage(
            "usage: opencode-dbtool session delete <session-id> [session-id...]",
        ));
    }
    let refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
    let mut resolved = resolve_session_ids(con, &refs)?;
    expand_children(con, &mut resolved)?;

    let mut out = db_status(db_path);
    out["dry_run"] = serde_json::json!(dry_run);
    let (total_rows, sessions_arr) = preview_impact(con, &resolved)?;
    out["total_rows"] = serde_json::json!(total_rows);
    out["sessions"] = serde_json::json!(sessions_arr);
    out["deleted"] = serde_json::json!(false);
    if dry_run {
        return print_json(&out);
    }

    execute_delete(con, &resolved)?;
    out["deleted"] = serde_json::json!(true);
    out["note"] = serde_json::json!("file size is unchanged until `opencode-dbtool vacuum` is run");
    print_json(&out)
}

/// Delete sessions selected by filters. At least one filter is required.
pub fn cmd_session_purge(
    con: &mut Connection,
    args: &[String],
    dry_run: bool,
    db_path: &Path,
) -> Result<()> {
    let filters = parse_purge_args(args)?;
    if filters.is_empty() {
        return Err(AppError::usage(
            "usage: opencode-dbtool session purge [--older-than <age>] [--subagents] [--path <dir>...] [--larger-than <size>] [--keep-latest <n>]",
        ));
    }
    let mut selected = select_ids(con, &filters, true)?;
    expand_children(con, &mut selected)?;

    let mut out = db_status(db_path);
    out["dry_run"] = serde_json::json!(dry_run);
    out["action"] = serde_json::json!("delete");
    out["filters"] = filters.json();
    let (total_rows, sessions_arr) = preview_impact(con, &selected)?;
    out["total_rows"] = serde_json::json!(total_rows);
    out["sessions"] = serde_json::json!(sessions_arr);
    out["deleted"] = serde_json::json!(false);
    if dry_run {
        return print_json(&out);
    }

    execute_delete(con, &selected)?;
    out["deleted"] = serde_json::json!(true);
    out["note"] = serde_json::json!("file size is unchanged until `opencode-dbtool vacuum` is run");
    print_json(&out)
}

/// Delete reasoning parts of the sessions selected by filters (optional
/// filters: none = every session). Conversation text is untouched.
pub fn cmd_session_strip_reasoning(
    con: &mut Connection,
    args: &[String],
    dry_run: bool,
    db_path: &Path,
) -> Result<()> {
    let filters = parse_purge_args(args)?;
    // No child expansion for strip: only matching sessions are stripped.
    let selected = select_ids(con, &filters, false)?;

    let mut out = db_status(db_path);
    out["dry_run"] = serde_json::json!(dry_run);
    out["action"] = serde_json::json!("strip-reasoning");
    out["filters"] = filters.json();
    let mut sessions_arr: Vec<serde_json::Value> = Vec::new();
    let mut total_parts: i64 = 0;
    let mut total_bytes: i64 = 0;
    for (id, n, bytes) in reasoning_counts(con, &selected)? {
        total_parts += n;
        total_bytes += bytes;
        sessions_arr.push(serde_json::json!({
            "id": id,
            "reasoning_parts": n,
            "reasoning_bytes": bytes,
        }));
    }
    out["sessions"] = serde_json::json!(sessions_arr);
    out["total_sessions"] = serde_json::json!(sessions_arr.len());
    out["total_reasoning_parts"] = serde_json::json!(total_parts);
    out["total_reasoning_bytes"] = serde_json::json!(total_bytes);
    out["stripped"] = serde_json::json!(false);
    if dry_run {
        return print_json(&out);
    }

    con.execute_batch("PRAGMA foreign_keys = ON;")?;
    let tx = con.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    strip_reasoning(&tx, &selected)?;
    tx.commit()?;

    let left = reasoning_left(con, &selected)?;
    if left != 0 {
        return Err(AppError::usage(format!(
            "reasoning parts still remain after strip: {left}"
        )));
    }
    out["stripped"] = serde_json::json!(true);
    out["note"] = serde_json::json!("file size is unchanged until `opencode-dbtool vacuum` is run");
    print_json(&out)
}

/// Parse `session delete` args: ids only; flags are rejected.
fn parse_id_args(args: &[String]) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for a in args {
        let id = a.trim();
        if id.is_empty() {
            continue;
        }
        if id == "--path" {
            return Err(AppError::usage(
                "`--path` was removed; use `session purge --path <dir>`",
            ));
        }
        if id.starts_with("--") {
            return Err(AppError::usage(format!("unknown option: {id}")));
        }
        ids.push(id.to_string());
    }
    Ok(ids)
}

/// Parse purge/strip-reasoning filter flags: `--older-than <age>`,
/// `--subagents`, `--path <dir>` (repeatable). Positional args are
/// rejected.
fn parse_purge_args(args: &[String]) -> Result<PurgeFilter> {
    let mut f = PurgeFilter::default();
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
            "--subagents" => {
                f.subagents = true;
                i += 1;
            }
            "--path" => {
                let p = args
                    .get(i + 1)
                    .ok_or_else(|| AppError::usage("--path requires a directory"))?;
                f.paths.push(p.clone());
                i += 2;
            }
            "--larger-than" => {
                let size = args
                    .get(i + 1)
                    .ok_or_else(|| AppError::usage("--larger-than requires a size (e.g. 50M)"))?;
                let bytes = parse_size_bytes(size)?;
                f.larger_than_raw = Some(size.clone());
                f.larger_than_bytes = Some(bytes);
                i += 2;
            }
            "--keep-latest" => {
                let n = args
                    .get(i + 1)
                    .ok_or_else(|| AppError::usage("--keep-latest requires a count"))?;
                let c = parse_count(n)?;
                f.keep_latest_raw = Some(n.clone());
                f.keep_latest = Some(c);
                i += 2;
            }
            other => return Err(AppError::usage(format!("unknown option: {other}"))),
        }
    }
    Ok(f)
}

/// Sessions matching the filters. `keep_latest` keeps the N most recent
/// matches (by `time_updated`, id as tiebreaker); for purge it also
/// protects their ancestors so deleting a parent can never orphan a
/// kept session. `protect_ancestors` is off for strip-reasoning, which
/// never deletes sessions.
fn select_ids(
    con: &Connection,
    filters: &PurgeFilter,
    protect_ancestors: bool,
) -> Result<Vec<String>> {
    let sessions = load_sessions(con)?;
    let mut selected: Vec<&crate::models::SessionRow> =
        sessions.iter().filter(|s| filters.matches(s)).collect();
    if let Some(n) = filters.keep_latest {
        let mut kept: HashSet<&str> = HashSet::new();
        for s in selected.iter().take(n as usize) {
            kept.insert(s.id.as_str());
        }
        if protect_ancestors {
            // A kept session's ancestors must survive too: deleting a
            // parent would orphan (or cascade-delete) the kept child.
            let mut added = true;
            while added {
                added = false;
                for s in &sessions {
                    if kept.contains(s.id.as_str()) {
                        if let Some(pid) = &s.parent_id {
                            if !kept.contains(pid.as_str()) {
                                kept.insert(pid.as_str());
                                added = true;
                            }
                        }
                    }
                }
            }
        }
        selected.retain(|s| !kept.contains(s.id.as_str()));
    }
    Ok(selected.iter().map(|s| s.id.clone()).collect())
}

/// Append all descendant sessions (recursive subagent sessions) of the
/// given ids, deduping.
fn expand_children(con: &Connection, ids: &mut Vec<String>) -> Result<()> {
    let mut seen: HashSet<String> = ids.iter().cloned().collect();
    let mut queue: Vec<String> = ids.clone();
    while let Some(id) = queue.pop() {
        for c in child_session_ids(con, &id)? {
            if seen.insert(c.clone()) {
                queue.push(c.clone());
                ids.push(c);
            }
        }
    }
    Ok(())
}

/// Per-session preview rows (id + per-table counts + total) and the
/// sum of all row counts.
fn preview_impact(con: &Connection, ids: &[String]) -> Result<(i64, Vec<serde_json::Value>)> {
    let mut total_rows: i64 = 0;
    let mut arr: Vec<serde_json::Value> = Vec::new();
    for id in ids {
        let counts = preview_counts(con, id)?;
        let mut rows_map = serde_json::Map::new();
        let mut total: i64 = 1; // the session row itself
        for (table, n) in counts {
            total += n;
            rows_map.insert(table, serde_json::json!(n));
        }
        total_rows += total;
        arr.push(serde_json::json!({
            "id": id,
            "rows": rows_map,
            "total": total,
        }));
    }
    Ok((total_rows, arr))
}

/// Delete the given sessions in a single immediate transaction and
/// verify they are gone afterwards.
fn execute_delete(con: &mut Connection, resolved: &[String]) -> Result<()> {
    con.execute_batch("PRAGMA foreign_keys = ON;")?;
    let tx = con.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for id in resolved {
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

    for id in resolved {
        if session_exists(con, id)? {
            return Err(AppError::usage(format!(
                "session still exists after delete: {id}"
            )));
        }
    }
    Ok(())
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
    use super::*;
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
    fn delete_rejects_path() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);

        let err = cmd_session_delete(
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
    fn purge_path_expands_children_in_other_dirs() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "parent", "/a", None);
        testdb::insert_session(&con, "child", "/b", Some("parent"));

        cmd_session_purge(
            &mut con,
            &["--path".to_string(), "/a".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 0);
    }

    #[test]
    fn purge_no_filters_is_usage_error() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);

        let err = cmd_session_purge(&mut con, &[], false, Path::new("/tmp/x.db")).unwrap_err();
        assert_eq!(err.code, 2);
    }

    #[test]
    fn purge_subagents_only() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "root", "/a", None);
        testdb::insert_session(&con, "child", "/a", Some("root"));

        cmd_session_purge(
            &mut con,
            &["--subagents".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 1);
        let remaining: String = con
            .query_row("SELECT id FROM session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, "root");
    }

    #[test]
    fn purge_older_than_selects_only_old() {
        let mut con = testdb::create();
        let now = now_ms().unwrap();
        testdb::insert_session_at(&con, "old", "/a", None, 0);
        testdb::insert_session_at(&con, "recent", "/a", None, now);

        cmd_session_purge(
            &mut con,
            &["--older-than".to_string(), "30d".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 1);
        let remaining: String = con
            .query_row("SELECT id FROM session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, "recent");
    }

    #[test]
    fn purge_older_than_boundary_is_strict() {
        let mut con = testdb::create();
        let now = now_ms().unwrap();
        let cutoff = now - 30 * 86_400_000;
        testdb::insert_session_at(&con, "at-cutoff", "/a", None, cutoff);
        testdb::insert_session_at(&con, "just-before", "/a", None, cutoff - 1);

        cmd_session_purge(
            &mut con,
            &["--older-than".to_string(), "30d".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 1);
        let remaining: String = con
            .query_row("SELECT id FROM session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, "at-cutoff");
    }

    #[test]
    fn purge_filters_combine_with_and() {
        let mut con = testdb::create();
        let now = now_ms().unwrap();
        testdb::insert_session_at(&con, "old-root", "/a", None, 0);
        testdb::insert_session_at(&con, "old-child", "/a", Some("old-root"), 0);
        testdb::insert_session_at(&con, "recent-child", "/a", Some("old-root"), now);

        cmd_session_purge(
            &mut con,
            &[
                "--older-than".to_string(),
                "30d".to_string(),
                "--subagents".to_string(),
            ],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 2);
        let remaining: Vec<String> = con
            .prepare("SELECT id FROM session ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(remaining, vec!["old-root", "recent-child"]);
    }

    #[test]
    fn purge_dry_run_changes_nothing() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);

        cmd_session_purge(
            &mut con,
            &["--path".to_string(), "/a".to_string()],
            true,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 1);
    }

    #[test]
    fn purge_larger_than_selects_big_sessions() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "small", "/a", None);
        testdb::insert_part(&con, "small", "x");
        testdb::insert_session(&con, "big", "/a", None);
        testdb::insert_part(&con, "big", "xxxxxxxx");

        cmd_session_purge(
            &mut con,
            &["--larger-than".to_string(), "4".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 1);
        let remaining: String = con
            .query_row("SELECT id FROM session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, "small");
    }

    #[test]
    fn purge_keep_latest_keeps_newest() {
        let mut con = testdb::create();
        for (i, id) in ["s1", "s2", "s3", "s4", "s5"].iter().enumerate() {
            testdb::insert_session_at(&con, id, "/a", None, i as i64);
        }

        cmd_session_purge(
            &mut con,
            &["--keep-latest".to_string(), "2".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 2);
        let remaining: Vec<String> = con
            .prepare("SELECT id FROM session ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(remaining, vec!["s4", "s5"]);
    }

    #[test]
    fn purge_keep_latest_protects_ancestors() {
        let mut con = testdb::create();
        testdb::insert_session_at(&con, "parent", "/a", None, 0);
        testdb::insert_session_at(&con, "child", "/a", Some("parent"), 1);

        cmd_session_purge(
            &mut con,
            &["--keep-latest".to_string(), "1".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        // The newest session is the child; its parent must survive too.
        assert_eq!(testdb::session_count(&con), 2);
    }

    #[test]
    fn purge_keep_latest_keeps_parent_deletes_old_child() {
        let mut con = testdb::create();
        testdb::insert_session_at(&con, "parent", "/a", None, 1);
        testdb::insert_session_at(&con, "child", "/a", Some("parent"), 0);

        cmd_session_purge(
            &mut con,
            &["--keep-latest".to_string(), "1".to_string()],
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
    fn purge_keep_latest_zero_deletes_all() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_session(&con, "s2", "/a", None);

        cmd_session_purge(
            &mut con,
            &["--keep-latest".to_string(), "0".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 0);
    }

    #[test]
    fn purge_keep_latest_exceeding_count_deletes_nothing() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);

        cmd_session_purge(
            &mut con,
            &["--keep-latest".to_string(), "5".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 1);
    }

    #[test]
    fn purge_keep_latest_composes_with_other_filters() {
        let mut con = testdb::create();
        testdb::insert_session_at(&con, "root1", "/a", None, 0);
        testdb::insert_session_at(&con, "child-old", "/a", Some("root1"), 1);
        testdb::insert_session_at(&con, "child-new", "/a", Some("root1"), 2);

        cmd_session_purge(
            &mut con,
            &[
                "--subagents".to_string(),
                "--keep-latest".to_string(),
                "1".to_string(),
            ],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 2);
        let remaining: Vec<String> = con
            .prepare("SELECT id FROM session ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(remaining, vec!["child-new", "root1"]);
    }

    #[test]
    fn strip_reasoning_respects_keep_latest() {
        let mut con = testdb::create();
        testdb::insert_session_at(&con, "old", "/a", None, 0);
        testdb::insert_session_at(&con, "new", "/a", None, 1);
        testdb::insert_part(&con, "old", r#"{"type":"reasoning","text":"o"}"#);
        testdb::insert_part(&con, "new", r#"{"type":"reasoning","text":"n"}"#);

        cmd_session_strip_reasoning(
            &mut con,
            &["--keep-latest".to_string(), "1".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::reasoning_part_count(&con), 1);
        let remaining: String = con
            .query_row("SELECT session_id FROM part", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, "new");
    }

    #[test]
    fn strip_reasoning_removes_only_reasoning_parts() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_part(
            &con,
            "s1",
            r#"{"type":"reasoning","text":"chain of thought"}"#,
        );
        testdb::insert_part(&con, "s1", r#"{"type":"reasoning","text":"more thinking"}"#);
        testdb::insert_part(&con, "s1", r#"{"type":"text","text":"hello"}"#);
        testdb::insert_part(&con, "s1", r#"{"type":"tool","text":"{}"}"#);

        cmd_session_strip_reasoning(&mut con, &[], false, Path::new("/tmp/x.db")).unwrap();

        assert_eq!(testdb::session_count(&con), 1);
        assert_eq!(testdb::part_count(&con), 2);
        assert_eq!(testdb::reasoning_part_count(&con), 0);
    }

    #[test]
    fn strip_reasoning_applies_filters() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "parent", "/a", None);
        testdb::insert_session(&con, "child", "/a", Some("parent"));
        testdb::insert_part(&con, "parent", r#"{"type":"reasoning","text":"p"}"#);
        testdb::insert_part(&con, "child", r#"{"type":"reasoning","text":"c"}"#);

        cmd_session_strip_reasoning(
            &mut con,
            &["--subagents".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::reasoning_part_count(&con), 1);
        let remaining: String = con
            .query_row("SELECT session_id FROM part", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, "parent");
    }

    #[test]
    fn strip_reasoning_dry_run_changes_nothing() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_part(&con, "s1", r#"{"type":"reasoning","text":"x"}"#);

        cmd_session_strip_reasoning(&mut con, &[], true, Path::new("/tmp/x.db")).unwrap();

        assert_eq!(testdb::reasoning_part_count(&con), 1);
    }

    #[test]
    fn parse_purge_args_ok() {
        let f = parse_purge_args(&[
            "--older-than".to_string(),
            "30d".to_string(),
            "--subagents".to_string(),
            "--path".to_string(),
            "/a".to_string(),
            "--path".to_string(),
            "/b".to_string(),
            "--larger-than".to_string(),
            "50M".to_string(),
            "--keep-latest".to_string(),
            "10".to_string(),
        ])
        .unwrap();
        assert_eq!(f.older_than_raw.as_deref(), Some("30d"));
        assert!(f.cutoff_ms.is_some());
        assert!(f.subagents);
        assert_eq!(f.paths, vec!["/a", "/b"]);
        assert_eq!(f.larger_than_raw.as_deref(), Some("50M"));
        assert_eq!(f.larger_than_bytes, Some(50 * 1024 * 1024));
        assert_eq!(f.keep_latest_raw.as_deref(), Some("10"));
        assert_eq!(f.keep_latest, Some(10));
    }

    #[test]
    fn parse_purge_args_rejects_bad_input() {
        for args in [
            vec!["--older-than".to_string()],
            vec!["--older-than".to_string(), "xyz".to_string()],
            vec!["--older-than".to_string(), "0d".to_string()],
            vec!["--path".to_string()],
            vec!["--larger-than".to_string()],
            vec!["--larger-than".to_string(), "0".to_string()],
            vec!["--larger-than".to_string(), "1.5M".to_string()],
            vec!["--keep-latest".to_string()],
            vec!["--keep-latest".to_string(), "-1".to_string()],
            vec!["ses_1".to_string()],
            vec!["--unknown".to_string()],
        ] {
            assert!(parse_purge_args(&args).is_err(), "should reject: {args:?}");
        }
    }

    #[test]
    fn parse_id_args_ok() {
        let ids = parse_id_args(&["ses_1".to_string(), "  ses_2  ".to_string()]).unwrap();
        assert_eq!(ids, vec!["ses_1", "ses_2"]);
        assert!(parse_id_args(&[]).unwrap().is_empty());
    }
}
