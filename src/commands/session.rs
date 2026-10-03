//! `session` subcommands: list, show, delete, purge, strip-reasoning.
//! V2-only.

use crate::db::{env_status, EnvStatus, SESSION_TABLE};
use crate::error::{AppError, Result};
use crate::models::{session_json, PurgeFilter, PurgeFilterJson, SessionOut};
use crate::output::print_json;
use crate::repo::{
    assistant_messages, child_session_ids, load_session, load_session_meta, load_sessions,
    reasoning_event_counts, reasoning_events_left, reasoning_messages_left, resolve_session_ids,
    rewrite_message, session_sizes, strip_reasoning_events,
};
use rusqlite::Connection;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::util::{now_ms, parse_age_ms, parse_count, parse_size_bytes, SQL_VAR_CHUNK};

/// One session in a delete/purge preview.
#[derive(Serialize)]
struct DeleteSessionRow {
    id: String,
    rows: serde_json::Map<String, serde_json::Value>,
    total: i64,
}

#[derive(Serialize)]
struct DeleteOut {
    #[serde(flatten)]
    env: EnvStatus,
    dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    filters: Option<PurgeFilterJson>,
    total_rows: i64,
    sessions: Vec<DeleteSessionRow>,
    deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

impl DeleteOut {
    fn from_preview(
        db_path: &Path,
        dry_run: bool,
        action: Option<String>,
        filters: Option<PurgeFilterJson>,
        total_rows: i64,
        sessions: Vec<DeleteSessionRow>,
    ) -> DeleteOut {
        DeleteOut {
            env: env_status(db_path),
            dry_run,
            action,
            filters,
            total_rows,
            sessions,
            deleted: false,
            note: None,
        }
    }
}

/// One session in a strip-reasoning preview.
#[derive(Serialize)]
struct StripSession {
    id: String,
    reasoning_events: i64,
    reasoning_event_bytes: i64,
    messages_rewritten: i64,
    rewritten_bytes: i64,
}

#[derive(Serialize)]
struct StripOut {
    #[serde(flatten)]
    env: EnvStatus,
    dry_run: bool,
    action: String,
    filters: PurgeFilterJson,
    sessions: Vec<StripSession>,
    total_sessions: usize,
    total_reasoning_events: i64,
    total_reasoning_event_bytes: i64,
    total_messages_rewritten: i64,
    total_rewritten_bytes: i64,
    stripped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

pub fn cmd_session_list(con: &Connection, args: &[String]) -> Result<()> {
    print_json(&session_list_value(con, args)?)
}

/// Build the session list array (exposed for tests).
pub fn session_list_value(con: &Connection, args: &[String]) -> Result<serde_json::Value> {
    let mut limit: Option<usize> = None;
    let mut sort_size = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--limit" => {
                let n = args
                    .get(i + 1)
                    .ok_or_else(|| AppError::usage("--limit requires a count"))?;
                let c = parse_count(n)?;
                limit = Some(c as usize);
                i += 2;
            }
            "--sort" => {
                let key = args
                    .get(i + 1)
                    .ok_or_else(|| AppError::usage("--sort requires a key"))?;
                match key.as_str() {
                    "size" => sort_size = true,
                    other => {
                        return Err(AppError::usage(format!(
                            "unknown sort key: {other} (only 'size')"
                        )))
                    }
                }
                i += 2;
            }
            other => return Err(AppError::usage(format!("unknown option: {other}"))),
        }
    }
    let mut sessions = load_sessions(con)?;
    if sort_size {
        sessions.sort_by(|a, b| {
            b.size_bytes()
                .cmp(&a.size_bytes())
                .then_with(|| a.id.cmp(&b.id))
        });
    }
    if let Some(n) = limit {
        sessions.truncate(n);
    }
    let out: Vec<SessionOut> = sessions.iter().map(session_json).collect();
    Ok(serde_json::to_value(out)?)
}

pub fn cmd_session_show(con: &Connection, args: &[String]) -> Result<()> {
    let mut messages = false;
    let mut limit: usize = 50;
    let mut limit_given = false;
    let mut ids: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--messages" => {
                messages = true;
                i += 1;
            }
            "--limit" => {
                let n = args
                    .get(i + 1)
                    .ok_or_else(|| AppError::usage("--limit requires a count"))?;
                limit = parse_count(n)? as usize;
                limit_given = true;
                i += 2;
            }
            other if other.starts_with("--") => {
                return Err(AppError::usage(format!("unknown option: {other}")));
            }
            id => {
                ids.push(id);
                i += 1;
            }
        }
    }
    if ids.len() != 1 || (limit_given && !messages) {
        return Err(AppError::usage(
            "usage: opencode-dbtool session show <session-id> [--messages [--limit <n>]]",
        ));
    }
    let id = ids[0].trim();
    let s = load_session(con, id)?
        .ok_or_else(|| AppError::usage(format!("session not found: {id}")))?;
    let mut v = serde_json::to_value(session_json(&s))?;
    if messages {
        let (total, rows) = crate::repo::list_messages(con, id, limit)?;
        let msgs: Vec<serde_json::Value> = rows
            .iter()
            .map(|m| {
                let preview: String = m.data.chars().take(300).collect();
                serde_json::json!({
                    "id": m.id,
                    "type": m.msg_type,
                    "seq": m.seq,
                    "time": crate::util::dt(m.created),
                    "bytes": m.bytes,
                    "preview": preview,
                    "truncated": m.data.len() > preview.len(),
                })
            })
            .collect();
        v["messages"] = serde_json::Value::Array(msgs);
        v["total_messages"] = serde_json::json!(total);
    }
    print_json(&v)
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

    let (total_rows, sessions_arr) = preview_impact(con, &resolved)?;
    let mut out = DeleteOut::from_preview(db_path, dry_run, None, None, total_rows, sessions_arr);
    if dry_run {
        return print_json(&serde_json::to_value(&out)?);
    }

    execute_delete(con, &resolved)?;
    out.deleted = true;
    out.note = Some("file size is unchanged until `opencode-dbtool vacuum` is run".into());
    print_json(&serde_json::to_value(&out)?)
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
            "usage: opencode-dbtool session purge [--older-than <age>] [--subagents] [--archived] [--empty] [--path <dir>...] [--path-prefix <dir>...] [--larger-than <size>] [--keep-latest <n>] [--keep-latest-per-project <n>]",
        ));
    }
    let mut selected = select_ids(con, &filters, true)?;
    expand_children(con, &mut selected)?;

    let (total_rows, sessions_arr) = preview_impact(con, &selected)?;
    let mut out = DeleteOut::from_preview(
        db_path,
        dry_run,
        Some("delete".into()),
        Some(filters.json()),
        total_rows,
        sessions_arr,
    );
    if dry_run {
        return print_json(&serde_json::to_value(&out)?);
    }

    execute_delete(con, &selected)?;
    out.deleted = true;
    out.note = Some("file size is unchanged until `opencode-dbtool vacuum` is run".into());
    print_json(&serde_json::to_value(&out)?)
}

/// Delete reasoning content of the sessions selected by filters (optional
/// filters: none = every session). Conversation text is untouched.
///
/// Reasoning lives in durable `event` rows
/// (`session.next.reasoning.started` / `.ended`; the full text lives in
/// `.ended`) and in `session_message` assistant `content[]`. Removing the
/// events also prevents reasoning from being re-projected from the event
/// log. Token/cost aggregates are kept.
pub fn cmd_session_strip_reasoning(
    con: &mut Connection,
    args: &[String],
    dry_run: bool,
    db_path: &Path,
) -> Result<()> {
    let filters = parse_purge_args(args)?;
    // No child expansion for strip: only matching sessions are stripped.
    let selected = select_ids(con, &filters, false)?;

    // Durable reasoning events, per session.
    let mut event_map: std::collections::HashMap<String, (i64, i64)> =
        std::collections::HashMap::new();
    for (id, n, bytes) in reasoning_event_counts(con, &selected)? {
        event_map.insert(id, (n, bytes));
    }
    // session_message reasoning (requires parsing the JSON).
    let mut message_map: std::collections::HashMap<String, (i64, i64)> =
        std::collections::HashMap::new();
    let mut rewrites: Vec<(String, String, String)> = Vec::new(); // (id, session_id, new data)
    for (id, session_id, data) in assistant_messages(con, &selected)? {
        if let Some(new_data) = sanitize_assistant_data(&data) {
            let old_bytes = data.len() as i64;
            let new_bytes = new_data.len() as i64;
            let e = message_map.entry(session_id.clone()).or_insert((0, 0));
            e.0 += 1;
            e.1 += (old_bytes - new_bytes).max(0);
            rewrites.push((id, session_id, new_data));
        }
    }

    let mut all_ids: Vec<String> = event_map.keys().cloned().collect();
    for id in message_map.keys() {
        if !all_ids.contains(id) {
            all_ids.push(id.clone());
        }
    }
    all_ids.sort();

    let mut sessions_arr: Vec<StripSession> = Vec::new();
    let mut total_events: i64 = 0;
    let mut total_event_bytes: i64 = 0;
    let mut total_messages: i64 = 0;
    let mut total_rewritten_bytes: i64 = 0;
    for id in &all_ids {
        let (events, event_bytes) = event_map.get(id).copied().unwrap_or((0, 0));
        let (messages, rewritten_bytes) = message_map.get(id).copied().unwrap_or((0, 0));
        total_events += events;
        total_event_bytes += event_bytes;
        total_messages += messages;
        total_rewritten_bytes += rewritten_bytes;
        sessions_arr.push(StripSession {
            id: id.clone(),
            reasoning_events: events,
            reasoning_event_bytes: event_bytes,
            messages_rewritten: messages,
            rewritten_bytes,
        });
    }
    let mut out = StripOut {
        env: env_status(db_path),
        dry_run,
        action: "strip-reasoning".into(),
        filters: filters.json(),
        sessions: sessions_arr,
        total_sessions: 0,
        total_reasoning_events: total_events,
        total_reasoning_event_bytes: total_event_bytes,
        total_messages_rewritten: total_messages,
        total_rewritten_bytes,
        stripped: false,
        note: None,
    };
    out.total_sessions = out.sessions.len();
    if dry_run {
        return print_json(&serde_json::to_value(&out)?);
    }

    con.execute_batch("PRAGMA foreign_keys = ON;")?;
    let tx = con.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    strip_reasoning_events(&tx, &selected)?;
    for (id, _session_id, new_data) in &rewrites {
        rewrite_message(&tx, id, new_data)?;
    }
    tx.commit()?;

    let events_left = reasoning_events_left(con, &selected)?;
    if events_left != 0 {
        return Err(AppError::usage(format!(
            "reasoning events still remain after strip: {events_left}"
        )));
    }
    let messages_left = reasoning_messages_left(con, &selected)?;
    if messages_left != 0 {
        return Err(AppError::usage(format!(
            "reasoning content still remains in session messages: {messages_left}"
        )));
    }
    out.stripped = true;
    out.note = Some("file size is unchanged until `opencode-dbtool vacuum` is run".into());
    print_json(&serde_json::to_value(&out)?)
}

/// Remove `type: "reasoning"` elements from an assistant message's
/// `content` array. Returns the rewritten data, or None when the
/// message has no reasoning content.
fn sanitize_assistant_data(data: &str) -> Option<String> {
    let mut v: serde_json::Value = serde_json::from_str(data).ok()?;
    let arr = v.get_mut("content")?.as_array_mut()?;
    let before = arr.len();
    arr.retain(|item| item.get("type").and_then(|t| t.as_str()) != Some("reasoning"));
    if arr.len() == before {
        return None;
    }
    Some(v.to_string())
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
/// `--subagents`, `--archived`, `--empty`, `--path <dir>` (repeatable,
/// exact), `--path-prefix <dir>` (repeatable), `--larger-than`,
/// `--keep-latest`, `--keep-latest-per-project`. Positional args are
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
            "--archived" => {
                f.archived = true;
                i += 1;
            }
            "--empty" => {
                f.empty = true;
                i += 1;
            }
            "--path" => {
                let p = args
                    .get(i + 1)
                    .ok_or_else(|| AppError::usage("--path requires a directory"))?;
                f.paths.push(p.clone());
                i += 2;
            }
            "--path-prefix" => {
                let p = args
                    .get(i + 1)
                    .ok_or_else(|| AppError::usage("--path-prefix requires a directory"))?;
                f.path_prefixes.push(p.clone());
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
            "--keep-latest-per-project" => {
                let n = args
                    .get(i + 1)
                    .ok_or_else(|| AppError::usage("--keep-latest-per-project requires a count"))?;
                let c = parse_count(n)?;
                f.keep_latest_per_project_raw = Some(n.clone());
                f.keep_latest_per_project = Some(c);
                i += 2;
            }
            other => return Err(AppError::usage(format!("unknown option: {other}"))),
        }
    }
    if f.keep_latest.is_some() && f.keep_latest_per_project.is_some() {
        return Err(AppError::usage(
            "--keep-latest and --keep-latest-per-project are mutually exclusive",
        ));
    }
    Ok(f)
}

/// Sessions matching the filters. `keep_latest` keeps the N most recent
/// matches (by `time_updated`, id as tiebreaker); `keep_latest_per_project`
/// keeps the N most recent matches *per project* instead. For purge both
/// also protect the ancestors of kept sessions so deleting a parent can
/// never orphan a kept session. `protect_ancestors` is off for
/// strip-reasoning, which never deletes sessions.
fn select_ids(
    con: &Connection,
    filters: &PurgeFilter,
    protect_ancestors: bool,
) -> Result<Vec<String>> {
    // Light selection: every session's filter fields, no aggregate
    // counts. Size aggregates are only computed (batched) when a
    // size-dependent filter actually needs them.
    let meta = load_session_meta(con)?;
    let mut selected: Vec<&crate::models::SessionMeta> =
        meta.iter().filter(|m| filters.matches_meta(m)).collect();
    if filters.larger_than_bytes.is_some() || filters.empty {
        let ids: Vec<String> = selected.iter().map(|m| m.id.clone()).collect();
        let sizes = session_sizes(con, &ids)?;
        if let Some(min) = filters.larger_than_bytes {
            selected.retain(|m| sizes.get(&m.id).copied().unwrap_or(0) > min);
        }
        if filters.empty {
            selected.retain(|m| sizes.get(&m.id).copied().unwrap_or(0) == 0);
        }
    }
    // Kept sessions are excluded from the result; ancestor protection
    // applies to both keep flavours on purge.
    let mut kept: HashSet<&str> = HashSet::new();
    if let Some(n) = filters.keep_latest {
        for s in selected.iter().take(n as usize) {
            kept.insert(s.id.as_str());
        }
    } else if let Some(n) = filters.keep_latest_per_project {
        // `meta` (and therefore `selected`) is ordered newest-first, so
        // the first N matches per project are the ones to keep.
        let mut per_project: HashMap<Option<&str>, usize> = HashMap::new();
        for s in selected.iter() {
            let count = per_project.entry(s.project_id.as_deref()).or_insert(0);
            if (*count as i64) < n {
                kept.insert(s.id.as_str());
                *count += 1;
            }
        }
    }
    if protect_ancestors && !kept.is_empty() {
        // A kept session's ancestors must survive too: deleting a
        // parent would orphan (or cascade-delete) the kept child.
        let parents: HashMap<&str, Option<&str>> = meta
            .iter()
            .map(|m| (m.id.as_str(), m.parent_id.as_deref()))
            .collect();
        let mut stack: Vec<&str> = kept.iter().copied().collect();
        while let Some(id) = stack.pop() {
            if let Some(Some(pid)) = parents.get(id) {
                if kept.insert(pid) {
                    stack.push(pid);
                }
            }
        }
    }
    if filters.keep_latest.is_some() || filters.keep_latest_per_project.is_some() {
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

/// Tables counted in a session delete preview, in stable output order.
/// `event_sequence` rows reference aggregates (not sessions) and are deleted
/// explicitly alongside `event` rows, so they are counted separately here to
/// keep the dry-run total exact.
const PREVIEW_TABLES: [(&str, &str); 7] = [
    ("session_message", "session_id"),
    ("session_inbox", "session_id"),
    ("session_pending", "session_id"),
    ("instruction_entry", "session_id"),
    ("instruction_state", "session_id"),
    ("event", "aggregate_id"),
    ("event_sequence", "aggregate_id"),
];

/// Per-session preview rows (id + per-table counts + total) and the
/// sum of all row counts. Counts are fetched with one batched GROUP BY
/// query per table (chunked to stay under SQLite's variable limit)
/// instead of per-session queries; every table key is present, zero
/// counts included.
fn preview_impact(con: &Connection, ids: &[String]) -> Result<(i64, Vec<DeleteSessionRow>)> {
    let mut per_session = HashMap::<String, Vec<(&str, i64)>>::new();
    for (table, id_col) in PREVIEW_TABLES {
        for chunk in ids.chunks(SQL_VAR_CHUNK) {
            if chunk.is_empty() {
                continue;
            }
            let sql = format!(
                "SELECT \"{id_col}\", COUNT(*) FROM \"{table}\" \
                 WHERE \"{id_col}\" IN ({}) GROUP BY \"{id_col}\"",
                placeholders(chunk.len())
            );
            let mut stmt = con.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            for r in rows {
                let (sid, n) = r?;
                per_session.entry(sid).or_default().push((table, n));
            }
        }
    }
    let mut total_rows: i64 = 0;
    let mut sessions: Vec<DeleteSessionRow> = Vec::new();
    for id in ids {
        let mut rows_map = serde_json::Map::new();
        for (table, _) in PREVIEW_TABLES {
            rows_map.insert(table.to_string(), serde_json::json!(0));
        }
        let mut total: i64 = 1; // the session row itself
        for (table, n) in per_session.remove(id).unwrap_or_default() {
            total += n;
            rows_map.insert(table.to_string(), serde_json::json!(n));
        }
        total_rows += total;
        sessions.push(DeleteSessionRow {
            id: id.clone(),
            rows: rows_map,
            total,
        });
    }
    Ok((total_rows, sessions))
}

/// `?` placeholders for an IN clause.
fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// Delete the given sessions in a single immediate transaction and verify
/// they are gone. Related rows follow via `ON DELETE CASCADE`; `event` /
/// `event_sequence` rows (which reference aggregates, not sessions) are
/// deleted explicitly.
fn execute_delete(con: &mut Connection, resolved: &[String]) -> Result<()> {
    con.execute_batch("PRAGMA foreign_keys = ON;")?;
    let tx = con.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for chunk in resolved.chunks(SQL_VAR_CHUNK) {
        let in_sql = placeholders(chunk.len());
        tx.execute(
            &format!("DELETE FROM event WHERE aggregate_id IN ({in_sql})"),
            rusqlite::params_from_iter(chunk),
        )?;
        tx.execute(
            &format!("DELETE FROM event_sequence WHERE aggregate_id IN ({in_sql})"),
            rusqlite::params_from_iter(chunk),
        )?;
        tx.execute(
            &format!("DELETE FROM \"{SESSION_TABLE}\" WHERE id IN ({in_sql})"),
            rusqlite::params_from_iter(chunk),
        )?;
    }
    tx.commit()?;

    let mut remaining: i64 = 0;
    for chunk in resolved.chunks(SQL_VAR_CHUNK) {
        let in_sql = placeholders(chunk.len());
        let left: i64 = con.query_row(
            &format!("SELECT COUNT(*) FROM \"{SESSION_TABLE}\" WHERE id IN ({in_sql})"),
            rusqlite::params_from_iter(chunk),
            |r| r.get(0),
        )?;
        remaining += left;
    }
    if remaining != 0 {
        return Err(AppError::usage(format!(
            "sessions still exist after delete: {remaining}"
        )));
    }
    Ok(())
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
            .query_row("SELECT id FROM session_v2", [], |r| r.get(0))
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
            .query_row("SELECT id FROM session_v2", [], |r| r.get(0))
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
    fn preview_rows_include_zero_tables() {
        let con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_session_message(&con, "m1", "s1", "assistant", "{}");

        let (total, arr) = preview_impact(&con, &["s1".to_string()]).unwrap();
        assert_eq!(total, 2); // session row + session_message row
        let row = serde_json::to_value(&arr[0]).unwrap();
        let rows = row["rows"].as_object().unwrap();
        assert_eq!(rows.len(), 7, "all counted tables present");
        assert_eq!(rows["session_message"], 1);
        assert_eq!(rows["session_inbox"], 0);
        assert_eq!(rows["event"], 0);
        assert_eq!(rows["event_sequence"], 0);
    }

    fn fixed_env(db: &str) -> crate::db::EnvStatus {
        crate::db::EnvStatus {
            opencode_running: Some(false),
            pids: Some(vec![]),
            pid_error: None,
            db: db.into(),
        }
    }

    #[test]
    fn delete_output_contract() {
        let mut rows = serde_json::Map::new();
        rows.insert("session_message".into(), serde_json::json!(1));
        rows.insert("session_inbox".into(), serde_json::json!(0));
        let out = DeleteOut {
            env: fixed_env("/tmp/x.db"),
            dry_run: true,
            action: Some("delete".into()),
            filters: None,
            total_rows: 2,
            sessions: vec![DeleteSessionRow {
                id: "ses_1".into(),
                rows,
                total: 2,
            }],
            deleted: false,
            note: None,
        };
        let v = serde_json::to_value(&out).unwrap();
        let expected = serde_json::json!({
            "opencode_running": false, "pids": [], "db": "/tmp/x.db",
            "dry_run": true, "action": "delete", "total_rows": 2,
            "sessions": [ { "id": "ses_1", "rows": { "session_message": 1, "session_inbox": 0 }, "total": 2 } ],
            "deleted": false
        });
        assert_eq!(v, expected, "delete JSON contract changed");
        assert!(v.get("filters").is_none(), "no filters on plain delete");
        assert!(v.get("note").is_none(), "no note on dry-run");
    }

    #[test]
    fn strip_output_contract() {
        let out = StripOut {
            env: fixed_env("/tmp/x.db"),
            dry_run: true,
            action: "strip-reasoning".into(),
            filters: crate::models::PurgeFilterJson {
                older_than: Some("30d".into()),
                subagents: true,
                paths: vec!["/a".into()],
                path_prefixes: vec![],
                archived: false,
                empty: false,
                larger_than: None,
                keep_latest: Some("1".into()),
                keep_latest_per_project: None,
            },
            sessions: vec![StripSession {
                id: "ses_1".into(),
                reasoning_events: 1,
                reasoning_event_bytes: 50,
                messages_rewritten: 2,
                rewritten_bytes: 5,
            }],
            total_sessions: 1,
            total_reasoning_events: 1,
            total_reasoning_event_bytes: 50,
            total_messages_rewritten: 2,
            total_rewritten_bytes: 5,
            stripped: false,
            note: None,
        };
        let v = serde_json::to_value(&out).unwrap();
        let expected = serde_json::json!({
            "opencode_running": false, "pids": [], "db": "/tmp/x.db",
            "dry_run": true, "action": "strip-reasoning",
            "filters": { "older_than": "30d", "subagents": true, "paths": ["/a"],
                         "path_prefixes": [], "archived": false, "empty": false,
                         "larger_than": null, "keep_latest": "1",
                         "keep_latest_per_project": null },
            "sessions": [ { "id": "ses_1",
                            "reasoning_events": 1, "reasoning_event_bytes": 50,
                            "messages_rewritten": 2, "rewritten_bytes": 5 } ],
            "total_sessions": 1,
            "total_reasoning_events": 1, "total_reasoning_event_bytes": 50,
            "total_messages_rewritten": 2, "total_rewritten_bytes": 5,
            "stripped": false
        });
        assert_eq!(v, expected, "strip-reasoning JSON contract changed");
    }

    #[test]
    fn bulk_preview_and_delete_with_many_sessions() {
        // More sessions than one IN chunk (900): exercises the chunked
        // batch queries and stays under SQLite's variable limit.
        const N: usize = 1500;
        let mut con = testdb::create();
        for i in 0..N {
            testdb::insert_session(&con, &format!("s{i}"), "/a", None);
        }
        let ids: Vec<String> = (0..N).map(|i| format!("s{i}")).collect();

        let (total, arr) = preview_impact(&con, &ids).unwrap();
        assert_eq!(arr.len(), N);
        assert_eq!(total, N as i64);

        execute_delete(&mut con, &ids).unwrap();
        assert_eq!(testdb::session_count(&con), 0);
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
            .query_row("SELECT id FROM session_v2", [], |r| r.get(0))
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
            .query_row("SELECT id FROM session_v2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, "recent");
    }

    #[test]
    fn purge_older_than_boundary_is_strict() {
        let mut con = testdb::create();
        let now = now_ms().unwrap();
        // One hour above/below the cutoff; the re-computed cutoff at
        // purge time may drift by milliseconds, so the exact-boundary
        // semantics are asserted in models::tests instead.
        testdb::insert_session_at(&con, "above", "/a", None, now - 30 * 86_400_000 + 3_600_000);
        testdb::insert_session_at(&con, "below", "/a", None, now - 30 * 86_400_000 - 3_600_000);

        cmd_session_purge(
            &mut con,
            &["--older-than".to_string(), "30d".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 1);
        let remaining: String = con
            .query_row("SELECT id FROM session_v2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, "above");
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
            .prepare("SELECT id FROM session_v2 ORDER BY id")
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
        testdb::insert_session_message(&con, "m1", "small", "assistant", "x");
        testdb::insert_session(&con, "big", "/a", None);
        testdb::insert_session_message(&con, "m2", "big", "assistant", "xxxxxxxx");

        cmd_session_purge(
            &mut con,
            &["--larger-than".to_string(), "4".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 1);
        let remaining: String = con
            .query_row("SELECT id FROM session_v2", [], |r| r.get(0))
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
            .prepare("SELECT id FROM session_v2 ORDER BY id")
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
            .query_row("SELECT id FROM session_v2", [], |r| r.get(0))
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
            .prepare("SELECT id FROM session_v2 ORDER BY id")
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
        testdb::insert_session_message(
            &con,
            "m-old",
            "old",
            "assistant",
            r#"{"type":"assistant","content":[{"type":"reasoning","text":"o"}]}"#,
        );
        testdb::insert_session_message(
            &con,
            "m-new",
            "new",
            "assistant",
            r#"{"type":"assistant","content":[{"type":"reasoning","text":"n"}]}"#,
        );

        cmd_session_strip_reasoning(
            &mut con,
            &["--keep-latest".to_string(), "1".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        let left: i64 = con
            .query_row(
                "SELECT COUNT(*) FROM session_message WHERE json_extract(data, '$.content') LIKE '%reasoning%' AND json_valid(data)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // Only the old session was stripped; the new one keeps reasoning.
        assert_eq!(
            crate::repo::reasoning_messages_left(&con, &["old".to_string()]).unwrap(),
            0
        );
        assert_eq!(left, 1);
    }

    #[test]
    fn strip_reasoning_sanitizes_session_messages() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_session_message(
            &con,
            "m1",
            "s1",
            "assistant",
            r#"{"type":"assistant","content":[
                {"type":"reasoning","text":"think think","id":"r1"},
                {"type":"text","text":"hello","id":"t1"}
            ],"tokens":{"reasoning":100,"output":5}}"#,
        );
        testdb::insert_session_message(
            &con,
            "m2",
            "s1",
            "user",
            r#"{"type":"user","content":"hi"}"#,
        );

        cmd_session_strip_reasoning(&mut con, &[], false, Path::new("/tmp/x.db")).unwrap();

        let data: String = con
            .query_row(
                "SELECT data FROM session_message WHERE id = 'm1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(v["content"].as_array().unwrap().len(), 1);
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["tokens"]["reasoning"], 100, "token aggregates kept");

        let user_data: String = con
            .query_row(
                "SELECT data FROM session_message WHERE id = 'm2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(user_data.contains("hi"), "user message untouched");
    }

    #[test]
    fn strip_reasoning_skips_messages_without_reasoning() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_session_message(
            &con,
            "m1",
            "s1",
            "assistant",
            r#"{"type":"assistant","content":[{"type":"text","text":"hi","id":"t1"}]}"#,
        );

        cmd_session_strip_reasoning(&mut con, &[], false, Path::new("/tmp/x.db")).unwrap();

        let data: String = con
            .query_row(
                "SELECT data FROM session_message WHERE id = 'm1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(data.contains("hi"));
    }

    #[test]
    fn sanitize_assistant_data_helpers() {
        let kept = sanitize_assistant_data(
            r#"{"type":"assistant","content":[
                {"type":"reasoning","text":"x"},
                {"type":"text","text":"y"},
                {"type":"tool","tool":"bash"}
            ]}"#,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&kept).unwrap();
        let types: Vec<&str> = v["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, vec!["text", "tool"]);

        assert!(sanitize_assistant_data(
            r#"{"type":"assistant","content":[{"type":"text","text":"y"}]}"#
        )
        .is_none());
        assert!(sanitize_assistant_data(r#"{"type":"user"}"#).is_none());
        assert!(sanitize_assistant_data("not json").is_none());
    }

    #[test]
    fn verification_detects_reasoning_by_json_semantics() {
        let con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_session_message(
            &con,
            "m1",
            "s1",
            "assistant",
            r#"{"type":"assistant","content":[{"type": "reasoning", "text": "x"}]}"#,
        );
        testdb::insert_session_message(
            &con,
            "m2",
            "s1",
            "assistant",
            r#"{"type":"assistant","content":[{"type":"text","text":"say {\"type\":\"reasoning\"}"}]}"#,
        );
        testdb::insert_session_message(
            &con,
            "m3",
            "s1",
            "assistant",
            r#"not json but "type":"reasoning""#,
        );

        let ids = vec!["s1".to_string()];
        assert_eq!(reasoning_messages_left(&con, &ids).unwrap(), 2);
    }

    #[test]
    fn strip_reasoning_removes_reasoning_events() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_event(
            &con,
            "s1",
            "session.next.reasoning.started",
            r#"{"reasoningID":"r1"}"#,
        );
        testdb::insert_event(
            &con,
            "s1",
            "session.next.reasoning.ended",
            r#"{"reasoningID":"r1","text":"chain of thought"}"#,
        );
        testdb::insert_event(&con, "s1", "session.next.text.ended", r#"{"text":"hello"}"#);

        cmd_session_strip_reasoning(&mut con, &[], false, Path::new("/tmp/x.db")).unwrap();

        let remaining: Vec<String> = con
            .prepare("SELECT type FROM event ORDER BY type")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(remaining, vec!["session.next.text.ended"]);
    }

    #[test]
    fn strip_reasoning_dry_run_changes_nothing() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_session_message(
            &con,
            "m1",
            "s1",
            "assistant",
            r#"{"type":"assistant","content":[{"type":"reasoning","text":"x"}]}"#,
        );

        cmd_session_strip_reasoning(&mut con, &[], true, Path::new("/tmp/x.db")).unwrap();

        assert_eq!(
            reasoning_messages_left(&con, &["s1".to_string()]).unwrap(),
            1
        );
    }

    #[test]
    fn strip_reasoning_applies_filters() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "parent", "/a", None);
        testdb::insert_session(&con, "child", "/a", Some("parent"));
        testdb::insert_session_message(
            &con,
            "m-p",
            "parent",
            "assistant",
            r#"{"type":"assistant","content":[{"type":"reasoning","text":"p"}]}"#,
        );
        testdb::insert_session_message(
            &con,
            "m-c",
            "child",
            "assistant",
            r#"{"type":"assistant","content":[{"type":"reasoning","text":"c"}]}"#,
        );

        cmd_session_strip_reasoning(
            &mut con,
            &["--subagents".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(
            reasoning_messages_left(&con, &["parent".to_string()]).unwrap(),
            1
        );
        assert_eq!(
            reasoning_messages_left(&con, &["child".to_string()]).unwrap(),
            0
        );
    }

    #[test]
    fn session_list_sorts_by_size_and_limits() {
        let con = testdb::create();
        testdb::insert_session(&con, "small", "/a", None);
        testdb::insert_session_message(&con, "m1", "small", "assistant", "x");
        testdb::insert_session(&con, "big", "/a", None);
        testdb::insert_session_message(&con, "m2", "big", "assistant", "xxxxxxxxxxxxxxxx");
        testdb::insert_session(&con, "medium", "/a", None);
        testdb::insert_session_message(&con, "m3", "medium", "assistant", "xxxxxxxx");

        let all = session_list_value(&con, &[]).unwrap();
        let ids: Vec<&str> = all
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 3);

        let sized = session_list_value(&con, &["--sort".to_string(), "size".to_string()]).unwrap();
        let ids: Vec<&str> = sized
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["big", "medium", "small"]);

        let limited = session_list_value(
            &con,
            &[
                "--sort".to_string(),
                "size".to_string(),
                "--limit".to_string(),
                "2".to_string(),
            ],
        )
        .unwrap();
        let ids: Vec<&str> = limited
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["big", "medium"]);

        assert!(session_list_value(&con, &["--sort".to_string(), "nope".to_string()]).is_err());
        assert!(session_list_value(&con, &["--limit".to_string()]).is_err());
        assert!(session_list_value(&con, &["--unknown".to_string()]).is_err());
    }

    #[test]
    fn delete_cascades_to_messages_and_inbox() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_session_message(&con, "m1", "s1", "assistant", "{}");
        testdb::insert_inbox(&con, "i1", "s1", "payload");
        con.execute_batch("PRAGMA foreign_keys = ON;").unwrap();

        cmd_session_delete(&mut con, &["s1".to_string()], false, Path::new("/tmp/x.db")).unwrap();

        assert_eq!(testdb::session_count(&con), 0);
        let sm: i64 = con
            .query_row("SELECT COUNT(*) FROM session_message", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sm, 0);
        let inbox: i64 = con
            .query_row("SELECT COUNT(*) FROM session_inbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(inbox, 0);
    }

    #[test]
    fn preview_counts_v2_tables() {
        let con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_session_message(&con, "m1", "s1", "assistant", "{}");
        let (total, arr) = preview_impact(&con, &["s1".to_string()]).unwrap();
        assert_eq!(total, 2, "session row + session_message row");
        let rows = &arr[0].rows;
        assert_eq!(rows["session_message"], 1);
        assert_eq!(rows["event_sequence"], 0);
        assert_eq!(rows.len(), 7);
    }

    #[test]
    fn larger_than_uses_v2_bytes() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "small", "/a", None);
        testdb::insert_session_message(&con, "m1", "small", "assistant", "x");
        testdb::insert_session(&con, "big", "/a", None);
        testdb::insert_session_message(&con, "m2", "big", "assistant", "xxxxxxxx");

        cmd_session_purge(
            &mut con,
            &["--larger-than".to_string(), "4".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 1);
        let remaining: String = con
            .query_row("SELECT id FROM session_v2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, "small");
    }

    #[test]
    fn parse_purge_args_ok() {
        let f = parse_purge_args(&[
            "--older-than".to_string(),
            "30d".to_string(),
            "--subagents".to_string(),
            "--archived".to_string(),
            "--empty".to_string(),
            "--path".to_string(),
            "/a".to_string(),
            "--path-prefix".to_string(),
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
        assert!(f.archived);
        assert!(f.empty);
        assert_eq!(f.paths, vec!["/a"]);
        assert_eq!(f.path_prefixes, vec!["/b"]);
        assert_eq!(f.larger_than_raw.as_deref(), Some("50M"));
        assert_eq!(f.larger_than_bytes, Some(50 * 1024 * 1024));
        assert_eq!(f.keep_latest_raw.as_deref(), Some("10"));
        assert_eq!(f.keep_latest, Some(10));
    }

    #[test]
    fn keep_latest_variants_are_mutually_exclusive() {
        for args in [
            vec!["--keep-latest".to_string()],
            vec!["--keep-latest".to_string(), "0".to_string()],
            vec!["--keep-latest".to_string(), "-1".to_string()],
            vec!["--keep-latest-per-project".to_string()],
            vec![
                "--keep-latest".to_string(),
                "1".to_string(),
                "--keep-latest-per-project".to_string(),
                "1".to_string(),
            ],
        ] {
            // Bare flags are rejected; the pair is rejected together.
            if args.len() == 2 && args[0] == "--keep-latest" && args[1] == "0" {
                // --keep-latest 0 is valid (deletes all).
                assert!(parse_purge_args(&args).is_ok());
            } else {
                assert!(parse_purge_args(&args).is_err(), "should reject: {args:?}");
            }
        }
        assert!(
            parse_purge_args(&["--keep-latest-per-project".to_string(), "2".to_string()]).is_ok()
        );
    }

    #[test]
    fn purge_path_prefix_matches_subtree() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "root", "/a", None);
        testdb::insert_session(&con, "child", "/a/b/c", None);
        testdb::insert_session(&con, "other", "/ab", None);

        cmd_session_purge(
            &mut con,
            &["--path-prefix".to_string(), "/a".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 1);
        let remaining: String = con
            .query_row("SELECT id FROM session_v2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, "other");
    }

    #[test]
    fn purge_empty_selects_only_contentless_sessions() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "empty", "/a", None);
        testdb::insert_session(&con, "full", "/a", None);
        testdb::insert_session_message(&con, "m1", "full", "assistant", "x");

        cmd_session_purge(
            &mut con,
            &["--empty".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 1);
        let remaining: String = con
            .query_row("SELECT id FROM session_v2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, "full");
    }

    #[test]
    fn purge_archived_selects_only_archived() {
        let mut con = testdb::create();
        testdb::insert_session(&con, "live", "/a", None);
        testdb::insert_session(&con, "old", "/a", None);
        con.execute(
            "UPDATE session_v2 SET time_archived = 1 WHERE id = 'old'",
            [],
        )
        .unwrap();

        cmd_session_purge(
            &mut con,
            &["--archived".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        assert_eq!(testdb::session_count(&con), 1);
        let remaining: String = con
            .query_row("SELECT id FROM session_v2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, "live");
    }

    #[test]
    fn purge_keep_latest_per_project_keeps_newest_each() {
        let mut con = testdb::create();
        testdb::insert_project(&con, "p1", "/a");
        testdb::insert_project(&con, "p2", "/b");
        for (i, id) in ["a1", "a2", "a3"].iter().enumerate() {
            con.execute(
                "INSERT INTO session_v2 (id, directory, title, project_id, time_updated, cost) \
                 VALUES (?1, '/a', ?1, 'p1', ?2, 0)",
                rusqlite::params![id, i as i64],
            )
            .unwrap();
        }
        for (i, id) in ["b1", "b2"].iter().enumerate() {
            con.execute(
                "INSERT INTO session_v2 (id, directory, title, project_id, time_updated, cost) \
                 VALUES (?1, '/b', ?1, 'p2', ?2, 0)",
                rusqlite::params![id, i as i64],
            )
            .unwrap();
        }

        cmd_session_purge(
            &mut con,
            &["--keep-latest-per-project".to_string(), "1".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();

        // Newest per project survive: a3 and b2.
        let mut remaining: Vec<String> = con
            .prepare("SELECT id FROM session_v2 ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        remaining.sort();
        assert_eq!(remaining, vec!["a3", "b2"]);
    }

    #[test]
    fn parse_purge_args_rejects_bad_input() {
        for args in [
            vec!["--older-than".to_string()],
            vec!["--older-than".to_string(), "xyz".to_string()],
            vec!["--older-than".to_string(), "0d".to_string()],
            vec!["--path".to_string()],
            vec!["--path-prefix".to_string()],
            vec!["--larger-than".to_string()],
            vec!["--larger-than".to_string(), "0".to_string()],
            vec!["--larger-than".to_string(), "1.5M".to_string()],
            vec!["--keep-latest".to_string()],
            vec!["--keep-latest".to_string(), "-1".to_string()],
            vec!["--keep-latest-per-project".to_string()],
            vec!["ses_1".to_string()],
            vec!["--unknown".to_string()],
        ] {
            assert!(parse_purge_args(&args).is_err(), "should reject: {args:?}");
        }
    }

    #[test]
    fn show_messages_previews_content() {
        let con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_session_message(&con, "m1", "s1", "assistant", &"x".repeat(500));
        testdb::insert_session_message(&con, "m2", "s1", "user", "hi");

        // Without the flag there is no messages block.
        let s = crate::repo::load_session(&con, "s1").unwrap().unwrap();
        let v = serde_json::to_value(crate::models::session_json(&s)).unwrap();
        assert!(v.get("messages").is_none());

        cmd_session_show(&con, &["s1".to_string(), "--messages".to_string()]).unwrap();
        cmd_session_show(
            &con,
            &[
                "s1".to_string(),
                "--messages".to_string(),
                "--limit".to_string(),
                "1".to_string(),
            ],
        )
        .unwrap();
        assert!(cmd_session_show(&con, &["s1".to_string(), "--limit".to_string()]).is_err());
    }

    #[test]
    fn parse_id_args_ok() {
        let ids = parse_id_args(&["ses_1".to_string(), "  ses_2  ".to_string()]).unwrap();
        assert_eq!(ids, vec!["ses_1", "ses_2"]);
        assert!(parse_id_args(&[]).unwrap().is_empty());
    }
}
