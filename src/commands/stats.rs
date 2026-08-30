//! `stats` command: table sizes and totals. `--detail` adds part-type
//! breakdowns, creation activity, and subagent shares.

use crate::db::{db_status, file_size};
use crate::error::{AppError, Result};
use crate::output::print_json;
use crate::util::{dir_size, now_ms, quote_ident, session_diff_dir, snapshot_dir, tool_output_dir};
use rusqlite::{params, Connection};
use std::path::Path;

pub fn cmd_stats(con: &Connection, db_path: &Path, args: &[String]) -> Result<()> {
    let detail = parse_stats_args(args)?;
    print_json(&stats_value(con, db_path, detail)?)
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
pub fn stats_value(con: &Connection, db_path: &Path, detail: bool) -> Result<serde_json::Value> {
    let freelist: i64 = con
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .unwrap_or(-1);
    let mut out = db_status(db_path);
    out["db_bytes"] = serde_json::json!(file_size(db_path));
    out["wal_bytes"] = serde_json::json!(file_size(&db_path.with_extension("db-wal")));
    out["free_pages"] = serde_json::json!(freelist);

    // `part` is scanned once here (its JSON is parsed for the type
    // breakdown); the table loop below skips its count and byte sum to
    // avoid a second full pass over the table.
    let part_types = part_types(con)?;
    let (part_count, part_bytes): (i64, i64) = part_types
        .as_object()
        .map(|m| {
            (
                m.values().map(|v| v["count"].as_i64().unwrap_or(0)).sum(),
                m.values().map(|v| v["bytes"].as_i64().unwrap_or(0)).sum(),
            )
        })
        .unwrap_or((0, 0));

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
            let rows: i64 = if name == "part" {
                part_count
            } else {
                con.query_row(
                    &format!("SELECT COUNT(*) FROM {}", quote_ident(&name)),
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(-1)
            };
            let has_data = con
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name='data'",
                    params![name],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0);
            let bytes: i64 = if has_data > 0 && name != "part" {
                con.query_row(
                    &format!(
                        "SELECT COALESCE(SUM(length(CAST(data AS BLOB))),0) FROM {}",
                        quote_ident(&name)
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0)
            } else {
                0
            };
            total += bytes;
            tables.insert(name, serde_json::json!(rows));
        }
    }
    total += part_bytes;
    out["tables"] = serde_json::json!(tables);
    out["total_data_bytes"] = serde_json::json!(total);
    out["storage"] = serde_json::json!({
        "session_diff_bytes": dir_size(&session_diff_dir(db_path)),
        "snapshot_bytes": dir_size(&snapshot_dir(db_path)),
        "tool_output_bytes": dir_size(&tool_output_dir(db_path)),
    });
    out["part_types"] = part_types;
    if detail {
        out["activity"] = activity(con)?;
        out["subagent"] = subagent(con)?;
    }
    Ok(out)
}

/// Per-part-type row counts and data bytes (keyed by `data.type`).
///
/// The object is built in `ORDER BY bytes DESC` (largest first) order;
/// the JSON output keeps that order only while serde_json's
/// `preserve_order` feature is enabled (Cargo.toml), asserted by the
/// `part_types_sorted_by_bytes_desc` test.
fn part_types(con: &Connection) -> Result<serde_json::Value> {
    let mut stmt = con.prepare(
        "SELECT COALESCE(json_extract(data, '$.type'), 'unknown'), \
         COUNT(*), COALESCE(SUM(length(CAST(data AS BLOB))),0) \
         FROM part GROUP BY 1 ORDER BY 3 DESC",
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
    Ok(serde_json::Value::Object(types))
}

/// Daily creation activity of parts and messages over the last 30 days,
/// grouped by the user's local calendar day (`'localtime'`).
fn activity(con: &Connection) -> Result<serde_json::Value> {
    let cutoff = now_ms()? - 30 * 86_400_000;
    let mut days: std::collections::BTreeMap<String, (i64, i64, i64, i64)> =
        std::collections::BTreeMap::new();
    let mut stmt = con.prepare(
        "SELECT date(time_created/1000, 'unixepoch', 'localtime'), COUNT(*), \
         COALESCE(SUM(length(CAST(data AS BLOB))),0) \
         FROM part WHERE time_created >= ?1 GROUP BY 1",
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
        let e = days.entry(day).or_insert((0, 0, 0, 0));
        e.0 += count;
        e.1 += bytes;
    }
    let mut stmt = con.prepare(
        "SELECT date(time_created/1000, 'unixepoch', 'localtime'), COUNT(*), \
         COALESCE(SUM(length(CAST(data AS BLOB))),0) \
         FROM message WHERE time_created >= ?1 GROUP BY 1",
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
        let e = days.entry(day).or_insert((0, 0, 0, 0));
        e.2 += count;
        e.3 += bytes;
    }
    let created: Vec<serde_json::Value> = days
        .into_iter()
        .map(|(day, (parts, part_bytes, msgs, msg_bytes))| {
            serde_json::json!({
                "day": day,
                "parts": parts,
                "part_bytes": part_bytes,
                "msgs": msgs,
                "msg_bytes": msg_bytes,
            })
        })
        .collect();
    Ok(serde_json::json!({ "days": 30, "created": created }))
}

/// Subagent session counts and bytes versus all sessions.
///
/// One pass over message/part/event each (grouped per session), instead
/// of per-session correlated subqueries, which degrade to a full scan
/// per session when there is no index on `session_id`.
fn subagent(con: &Connection) -> Result<serde_json::Value> {
    let (total_count, total_bytes, sub_count, sub_bytes): (i64, i64, i64, i64) = con.query_row(
        "SELECT COUNT(*), \
         COALESCE(SUM(mb + pb + eb), 0), \
         COALESCE(SUM(parent_id IS NOT NULL), 0), \
         COALESCE(SUM(CASE WHEN parent_id IS NOT NULL THEN mb + pb + eb END), 0) \
         FROM session s \
         LEFT JOIN (SELECT session_id, SUM(length(CAST(data AS BLOB))) AS mb \
                    FROM message GROUP BY session_id) m ON m.session_id = s.id \
         LEFT JOIN (SELECT session_id, SUM(length(CAST(data AS BLOB))) AS pb \
                    FROM part GROUP BY session_id) p ON p.session_id = s.id \
         LEFT JOIN (SELECT aggregate_id, SUM(length(CAST(data AS BLOB))) AS eb \
                    FROM event GROUP BY aggregate_id) e ON e.aggregate_id = s.id",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    Ok(serde_json::json!({
        "sessions": sub_count,
        "total_sessions": total_count,
        "size_bytes": sub_bytes,
        "total_size_bytes": total_bytes,
    }))
}

#[cfg(test)]
mod tests {
    use super::{parse_stats_args, stats_value};
    use crate::testdb;
    use std::fs;

    #[test]
    fn storage_field_reports_directory_sizes() {
        let dir = testdb::temp_data_dir("stats-storage");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);
        fs::create_dir_all(dir.join("storage/session_diff")).unwrap();
        fs::write(dir.join("storage/session_diff/s1.json"), vec![0u8; 3]).unwrap();
        fs::create_dir_all(dir.join("snapshot/p")).unwrap();
        fs::write(dir.join("snapshot/p/o"), vec![0u8; 5]).unwrap();
        fs::create_dir_all(dir.join("tool-output")).unwrap();
        fs::write(dir.join("tool-output/x"), vec![0u8; 7]).unwrap();

        let out = stats_value(&con, &db_path, false).unwrap();
        assert_eq!(out["storage"]["session_diff_bytes"], 3);
        assert_eq!(out["storage"]["snapshot_bytes"], 5);
        assert_eq!(out["storage"]["tool_output_bytes"], 7);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn part_types_breakdown() {
        let con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_part(&con, "s1", r#"{"type":"reasoning","text":"r"}"#);
        testdb::insert_part(&con, "s1", r#"{"type":"reasoning","text":"rr"}"#);
        testdb::insert_part(&con, "s1", r#"{"type":"text","text":"t"}"#);
        testdb::insert_part(&con, "s1", r#"{"type":"tool","text":"o"}"#);

        let out = stats_value(&con, std::path::Path::new("/tmp/x.db"), false).unwrap();
        assert_eq!(out["part_types"]["reasoning"]["count"], 2);
        assert_eq!(
            out["part_types"]["reasoning"]["bytes"],
            r#"{"type":"reasoning","text":"r"}"#.len() as i64
                + r#"{"type":"reasoning","text":"rr"}"#.len() as i64
        );
        assert_eq!(out["part_types"]["text"]["count"], 1);
        assert_eq!(out["part_types"]["tool"]["count"], 1);
    }

    #[test]
    fn part_types_sorted_by_bytes_desc() {
        let con = testdb::create();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_part(&con, "s1", r#"{"type":"a","text":"x"}"#);
        testdb::insert_part(&con, "s1", r#"{"type":"b","text":"xxxxxxxxxx"}"#);
        testdb::insert_part(&con, "s1", r#"{"type":"c","text":"xxxxxxxxxxxxxxxxxxxx"}"#);

        let out = stats_value(&con, std::path::Path::new("/tmp/x.db"), false).unwrap();
        let keys: Vec<&str> = out["part_types"]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        // Largest first ("c" > "b" > "a" by bytes; alphabetical would
        // be the reverse). Depends on serde_json's `preserve_order`.
        assert_eq!(keys, vec!["c", "b", "a"]);
    }

    #[test]
    fn detail_adds_activity_and_subagent() {
        let con = testdb::create();
        let now = crate::util::now_ms().unwrap();
        testdb::insert_session(&con, "s1", "/a", None);
        testdb::insert_part_at(&con, "s1", r#"{"type":"text","text":"x"}"#, now);
        testdb::insert_part_at(
            &con,
            "s1",
            r#"{"type":"text","text":"yy"}"#,
            now - 2 * 86_400_000,
        );
        testdb::insert_session(&con, "root", "/a", None);
        testdb::insert_session(&con, "child", "/a", Some("root"));
        testdb::insert_part_at(&con, "child", r#"{"type":"text","text":"child"}"#, now);

        let out = stats_value(&con, std::path::Path::new("/tmp/x.db"), false).unwrap();
        assert!(out.get("activity").is_none());
        assert!(out.get("subagent").is_none());

        let out = stats_value(&con, std::path::Path::new("/tmp/x.db"), true).unwrap();
        assert_eq!(out["activity"]["days"], 30);
        let days = out["activity"]["created"].as_array().unwrap();
        // now and now-2d are always on distinct local days, even across
        // a DST transition (local days are at most 25h long).
        assert_eq!(days.len(), 2);
        let last = &days[days.len() - 1];
        assert_eq!(last["parts"], 2);
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
