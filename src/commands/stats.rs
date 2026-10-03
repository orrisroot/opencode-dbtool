//! `stats` command: table sizes and totals (V2-only). `--detail` adds
//! message-type breakdowns, creation activity, and subagent shares.

use crate::db::{env_status, file_size, EnvStatus};
use crate::error::{AppError, Result};
use crate::output::print_json;
use crate::util::{dir_size, log_dir, now_ms, quote_ident, repos_dir, shell_dir, snapshot_dir};
use rusqlite::{params, Connection};
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
struct StorageSizes {
    snapshot_bytes: u64,
    shell_bytes: u64,
    repos_bytes: u64,
    log_bytes: u64,
}

#[derive(Serialize)]
struct ActivityDay {
    day: String,
    messages: i64,
    message_bytes: i64,
}

#[derive(Serialize)]
struct Activity {
    days: i64,
    created: Vec<ActivityDay>,
}

#[derive(Serialize)]
struct Subagent {
    sessions: i64,
    total_sessions: i64,
    size_bytes: i64,
    total_size_bytes: i64,
}

#[derive(Serialize)]
struct StatsOut {
    #[serde(flatten)]
    env: EnvStatus,
    db_bytes: u64,
    wal_bytes: u64,
    free_pages: i64,
    /// Table name -> row count.
    tables: serde_json::Map<String, serde_json::Value>,
    total_data_bytes: i64,
    storage: StorageSizes,
    /// `session_message` rows grouped by `type`, largest first (requires
    /// serde_json's `preserve_order`).
    message_types: serde_json::Map<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    activity: Option<Activity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subagent: Option<Subagent>,
}

pub fn cmd_stats(con: &Connection, data_dir: &Path, db_path: &Path, args: &[String]) -> Result<()> {
    let detail = parse_stats_args(args)?;
    print_json(&stats_value(con, data_dir, db_path, detail)?)
}

/// Parse `stats` flags: `--detail`. Unknown options are rejected.
fn parse_stats_args(args: &[String]) -> Result<bool> {
    let mut detail = false;
    for a in args {
        match a.as_str() {
            "--detail" => detail = true,
            other => return Err(AppError::usage(format!("unknown option: {other}"))),
        }
    }
    Ok(detail)
}

/// Build the stats object (exposed for tests).
pub fn stats_value(
    con: &Connection,
    data_dir: &Path,
    db_path: &Path,
    detail: bool,
) -> Result<serde_json::Value> {
    let out = stats_out(con, data_dir, db_path, detail)?;
    Ok(serde_json::to_value(&out)?)
}

fn stats_out(con: &Connection, data_dir: &Path, db_path: &Path, detail: bool) -> Result<StatsOut> {
    let env = env_status(db_path);
    let freelist: i64 = con.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;

    let message_types = message_types(con)?;

    let mut tables: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    let mut total: i64 = 0;
    {
        let mut stmt = con.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?;
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for name in names {
            let rows: i64 = con.query_row(
                &format!("SELECT COUNT(*) FROM {}", quote_ident(&name)),
                [],
                |r| r.get(0),
            )?;
            total += content_bytes(con, &name)?;
            tables.insert(name, serde_json::json!(rows));
        }
    }

    Ok(StatsOut {
        env,
        db_bytes: file_size(db_path),
        wal_bytes: file_size(&db_path.with_extension("db-wal")),
        free_pages: freelist,
        tables,
        total_data_bytes: total,
        storage: StorageSizes {
            snapshot_bytes: dir_size(&snapshot_dir(data_dir)),
            shell_bytes: dir_size(&shell_dir(data_dir)),
            repos_bytes: dir_size(&repos_dir(data_dir)),
            log_bytes: dir_size(&log_dir(data_dir)),
        },
        message_types,
        activity: if detail { Some(activity(con)?) } else { None },
        subagent: if detail { Some(subagent(con)?) } else { None },
    })
}

/// Content columns counted toward `total_data_bytes`; id, type, seq,
/// timestamps etc. are never content.
fn content_columns(con: &Connection, table: &str) -> Result<Vec<String>> {
    const CANDIDATES: &[&str] = &[
        "data",
        "payload",
        "value",
        "initial_values",
        "current_values",
        "metadata",
        "revert",
        "summary_diffs",
        "model",
        "sandboxes",
        "commands",
        "binding",
        "extra",
    ];
    let mut out = Vec::new();
    for c in CANDIDATES {
        let n: i64 = con.query_row(
            "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name=?2",
            params![table, c],
            |r| r.get(0),
        )?;
        if n > 0 {
            out.push(c.to_string());
        }
    }
    Ok(out)
}

fn content_bytes(con: &Connection, table: &str) -> Result<i64> {
    let cols = content_columns(con, table)?;
    if cols.is_empty() {
        return Ok(0);
    }
    let expr = cols
        .iter()
        .map(|c| format!("COALESCE(length(CAST(\"{c}\" AS BLOB)),0)"))
        .collect::<Vec<_>>()
        .join("+");
    Ok(con.query_row(
        &format!("SELECT COALESCE(SUM({expr}),0) FROM {}", quote_ident(table)),
        [],
        |r| r.get(0),
    )?)
}

/// Per-`session_message.type` row counts and data bytes, largest first.
fn message_types(con: &Connection) -> Result<serde_json::Map<String, serde_json::Value>> {
    let mut stmt = con.prepare(
        "SELECT type, COUNT(*), COALESCE(SUM(length(CAST(data AS BLOB))),0) \
         FROM session_message GROUP BY 1 ORDER BY 3 DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let mut types = serde_json::Map::new();
    for r in rows {
        let (t, count, bytes) = r?;
        types.insert(t, serde_json::json!({ "count": count, "bytes": bytes }));
    }
    Ok(types)
}

/// Daily `session_message` creation activity over the last 30 days,
/// grouped by the user's local calendar day (`'localtime'`).
fn activity(con: &Connection) -> Result<Activity> {
    let cutoff = now_ms()? - 30 * 86_400_000;
    let mut days: std::collections::BTreeMap<String, (i64, i64)> =
        std::collections::BTreeMap::new();
    let mut stmt = con.prepare(
        "SELECT date(time_created/1000, 'unixepoch', 'localtime'), COUNT(*), \
         COALESCE(SUM(length(CAST(data AS BLOB))),0) \
         FROM session_message WHERE time_created >= ?1 GROUP BY 1",
    )?;
    let rows = stmt.query_map(params![cutoff], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for r in rows {
        let (day, count, bytes) = r?;
        let e = days.entry(day).or_insert((0, 0));
        e.0 += count;
        e.1 += bytes;
    }
    let created: Vec<ActivityDay> = days
        .into_iter()
        .map(|(day, (messages, message_bytes))| ActivityDay { day, messages, message_bytes })
        .collect();
    Ok(Activity { days: 30, created })
}

/// Subagent session counts and bytes versus all sessions.
fn subagent(con: &Connection) -> Result<Subagent> {
    let meta = crate::repo::load_session_meta(con)?;
    let ids: Vec<String> = meta.iter().map(|m| m.id.clone()).collect();
    let sizes = crate::repo::session_sizes(con, &ids)?;
    let mut total_bytes: i64 = 0;
    let mut sub_bytes: i64 = 0;
    let mut sub_count: i64 = 0;
    for m in &meta {
        let b = sizes.get(&m.id).copied().unwrap_or(0);
        total_bytes += b;
        if m.parent_id.is_some() {
            sub_count += 1;
            sub_bytes += b;
        }
    }
    Ok(Subagent {
        sessions: sub_count,
        total_sessions: meta.len() as i64,
        size_bytes: sub_bytes,
        total_size_bytes: total_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_stats_args, stats_out, stats_value};
    use crate::testdb;
    use std::fs;
    use std::path::Path;

    #[test]
    fn stats_output_contract() {
        let con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        let msg = r#"{"type":"assistant"}"#;
        testdb::insert_session_message(&con, "m1", "s1", "assistant", msg);
        let mut out = stats_out(&con, Path::new("/tmp"), Path::new("/tmp/x.db"), false).unwrap();
        out.env = crate::db::EnvStatus {
            opencode_running: Some(false),
            pids: Some(vec![]),
            pid_error: None,
            db: "/tmp/x.db".into(),
        };
        out.db_bytes = 0;
        out.wal_bytes = 0;
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["tables"]["session_v2"], 1);
        assert_eq!(v["tables"]["session_message"], 1);
        assert_eq!(v["message_types"]["assistant"]["count"], 1);
        assert_eq!(v["message_types"]["assistant"]["bytes"], msg.len() as i64);
        assert!(v["total_data_bytes"].as_i64().unwrap() >= msg.len() as i64);
        assert!(v.get("part_types").is_none(), "part_types removed");
    }

    #[test]
    fn storage_field_reports_directory_sizes() {
        let dir = testdb::temp_data_dir("stats-storage");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);
        fs::create_dir_all(dir.join("snapshot/p")).unwrap();
        fs::write(dir.join("snapshot/p/o"), vec![0u8; 5]).unwrap();
        fs::create_dir_all(dir.join("log")).unwrap();
        fs::write(dir.join("log/opencode.log"), vec![0u8; 7]).unwrap();

        let out = stats_value(&con, &dir, &db_path, false).unwrap();
        assert_eq!(out["storage"]["snapshot_bytes"], 5);
        assert_eq!(out["storage"]["log_bytes"], 7);
        assert!(out["storage"].get("session_diff_bytes").is_none());
        assert!(out["storage"].get("tool_output_bytes").is_none());

        drop(con);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn message_types_breakdown() {
        let con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_session_message(&con, "m1", "s1", "assistant", "xx");
        testdb::insert_session_message(&con, "m2", "s1", "user", "y");

        let out = stats_value(
            &con,
            std::path::Path::new("/tmp"),
            std::path::Path::new("/tmp/x.db"),
            false,
        )
        .unwrap();
        assert_eq!(out["message_types"]["assistant"]["count"], 1);
        assert_eq!(out["message_types"]["user"]["count"], 1);
    }

    #[test]
    fn message_types_sorted_by_bytes_desc() {
        let con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_session_message(&con, "m1", "s1", "b", "xxxxxxxxxx");
        testdb::insert_session_message(&con, "m2", "s1", "a", "x");
        // Longest first regardless of alphabetical order.
        let out = stats_value(
            &con,
            std::path::Path::new("/tmp"),
            std::path::Path::new("/tmp/x.db"),
            false,
        )
        .unwrap();
        let keys: Vec<&str> = out["message_types"]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(keys, vec!["b", "a"]);
    }

    #[test]
    fn detail_adds_activity_and_subagent() {
        let con = testdb::create();
        let now = crate::util::now_ms().unwrap();
        testdb::insert_session(&con, "s1", "/a", None);
        con.execute(
            "INSERT INTO session_message (id, session_id, type, data, time_created) VALUES ('m1','s1','assistant','xx',?1)",
            rusqlite::params![now],
        )
        .unwrap();
        con.execute(
            "INSERT INTO session_message (id, session_id, type, data, time_created) VALUES ('m2','s1','assistant','yy',?1)",
            rusqlite::params![now - 2 * 86_400_000],
        )
        .unwrap();
        testdb::insert_session(&con, "root", "/a", None);
        testdb::insert_session(&con, "child", "/a", Some("root"));

        let out = stats_value(
            &con,
            std::path::Path::new("/tmp"),
            std::path::Path::new("/tmp/x.db"),
            false,
        )
        .unwrap();
        assert!(out.get("activity").is_none());
        assert!(out.get("subagent").is_none());

        let out = stats_value(
            &con,
            std::path::Path::new("/tmp"),
            std::path::Path::new("/tmp/x.db"),
            true,
        )
        .unwrap();
        assert_eq!(out["activity"]["days"], 30);
        let days = out["activity"]["created"].as_array().unwrap();
        assert_eq!(days.len(), 2);
        assert_eq!(out["subagent"]["sessions"], 1);
        assert_eq!(out["subagent"]["total_sessions"], 3);
    }

    #[test]
    fn parse_stats_args_ok() {
        assert!(!parse_stats_args(&[]).unwrap());
        assert!(parse_stats_args(&["--detail".into()]).unwrap());
        assert!(parse_stats_args(&["--nope".into()]).is_err());
        assert!(parse_stats_args(&["x".into()]).is_err());
    }
}
