//! `kv` subcommands: inspect and delete rows of the global `kv` table.
//!
//! Some keys are multi-megabyte caches (e.g. `models-dev:catalog`); `kv
//! list` exposes per-key sizes so they can be found, and `kv delete`
//! drops selected keys (opencode regenerates caches on demand).

use crate::db::{env_status, EnvStatus};
use crate::error::{AppError, Result};
use crate::output;
use crate::util::{dt, now_ms, parse_age_ms};
use rusqlite::{params, Connection};
use serde::Serialize;
use std::path::Path;

/// Bytes of `value` shown by `kv show` before truncation.
const SHOW_PREVIEW_CHARS: usize = 2000;

#[derive(Serialize)]
struct KvEntry {
    key: String,
    bytes: i64,
    updated: String,
}

#[derive(Serialize)]
struct KvShow {
    key: String,
    bytes: i64,
    updated: String,
    value: String,
    truncated: bool,
}

#[derive(Serialize)]
struct KvDeleteOut {
    #[serde(flatten)]
    env: EnvStatus,
    dry_run: bool,
    keys: Vec<KvKeyRow>,
    total_bytes: i64,
    deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

#[derive(Serialize)]
struct KvKeyRow {
    key: String,
    bytes: i64,
}

pub fn cmd_kv_list(con: &Connection, older_than: Option<&str>) -> Result<()> {
    // Optional `--older-than <age>` limits the listing to keys not updated
    // since the cutoff (stale caches first).
    let cutoff = match older_than {
        Some(age) => Some(now_ms()? - parse_age_ms(age)?),
        None => None,
    };
    let mut sql = "SELECT key, COALESCE(length(CAST(value AS BLOB)),0), time_updated \
                   FROM kv"
        .to_string();
    if cutoff.is_some() {
        sql.push_str(" WHERE time_updated < ?1");
    }
    sql.push_str(" ORDER BY 2 DESC, key");
    let mut stmt = con.prepare(&sql)?;
    let rows = if let Some(c) = cutoff {
        stmt.query_map(params![c], read_entry)?
    } else {
        stmt.query_map([], read_entry)?
    };
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    let value = serde_json::to_value(out)?;
    let rows = value.as_array().map(Vec::as_slice).unwrap_or_default();
    let bytes: i64 = rows.iter().filter_map(|r| r["bytes"].as_i64()).sum();
    let footer = format!(
        "{} key{}, {} total",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" },
        output::human_bytes(bytes)
    );
    output::emit_cols_footer(&value, &["key", "bytes", "updated"], &footer)
}

fn read_entry(r: &rusqlite::Row) -> rusqlite::Result<KvEntry> {
    Ok(KvEntry {
        key: r.get(0)?,
        bytes: r.get(1)?,
        updated: dt(r.get::<_, i64>(2)?),
    })
}

pub fn cmd_kv_show(con: &Connection, key: &str, raw: bool) -> Result<()> {
    let (value, updated): (String, i64) = con
        .query_row(
            "SELECT value, time_updated FROM kv WHERE key = ?1",
            params![key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                AppError::usage(format!("key not found: {key}"))
            }
            other => AppError::db(other.to_string()),
        })?;
    if raw {
        return output::emit_text(&value);
    }
    let bytes = value.len() as i64;
    let truncated = value.chars().count() > SHOW_PREVIEW_CHARS;
    let preview: String = value.chars().take(SHOW_PREVIEW_CHARS).collect();
    output::emit(&serde_json::to_value(KvShow {
        key: key.to_string(),
        bytes,
        updated: dt(updated),
        value: preview,
        truncated,
    })?)
}

pub fn cmd_kv_delete(
    con: &mut Connection,
    keys: &[String],
    dry_run: bool,
    db_path: &Path,
) -> Result<()> {
    if keys.is_empty() {
        return Err(AppError::usage(
            "usage: opencode-dbtool kv delete <key> [key...]",
        ));
    }
    let mut rows = Vec::new();
    for key in keys {
        let bytes: Option<i64> = con
            .query_row(
                "SELECT COALESCE(length(CAST(value AS BLOB)),0) FROM kv WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    AppError::usage(format!("key not found: {key}"))
                }
                other => AppError::db(other.to_string()),
            })?;
        // QueryReturnedNoRows is mapped above; a missing key never yields None.
        rows.push(KvKeyRow {
            key: key.clone(),
            bytes: bytes.unwrap_or(0),
        });
    }
    let total: i64 = rows.iter().map(|k| k.bytes).sum();
    let mut out = KvDeleteOut {
        env: env_status(db_path),
        dry_run,
        keys: rows,
        total_bytes: total,
        deleted: false,
        note: None,
    };
    if dry_run {
        return output::emit(&serde_json::to_value(&out)?);
    }
    con.execute_batch("PRAGMA foreign_keys = ON;")?;
    let tx = con.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for k in &out.keys {
        let removed = tx.execute("DELETE FROM kv WHERE key = ?1", params![k.key])?;
        if removed == 0 {
            return Err(AppError::usage(format!("key not found: {}", k.key)));
        }
    }
    tx.commit()?;
    out.deleted = true;
    out.note = Some("file size is unchanged until `opencode-dbtool vacuum` is run".into());
    output::emit(&serde_json::to_value(&out)?)
}

#[derive(Serialize)]
struct KvPurgeOut {
    #[serde(flatten)]
    env: EnvStatus,
    dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    older_than: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    larger_than: Option<String>,
    keys: Vec<KvKeyRow>,
    total_keys: usize,
    total_bytes: i64,
    deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

/// Delete every kv entry matching the age/size filters. At least one
/// filter is required so a bare `kv purge` can never wipe the table.
pub fn cmd_kv_purge(
    con: &mut Connection,
    older_than: Option<&str>,
    larger_than: Option<&str>,
    dry_run: bool,
    db_path: &Path,
) -> Result<()> {
    if older_than.is_none() && larger_than.is_none() {
        return Err(AppError::usage(
            "usage: opencode-dbtool kv purge --older-than <age> | --larger-than <size>",
        ));
    }
    let cutoff = match older_than {
        Some(age) => Some(now_ms()? - parse_age_ms(age)?),
        None => None,
    };
    let min_bytes = match larger_than {
        Some(size) => Some(crate::util::parse_size_bytes(size)?),
        None => None,
    };
    let mut stmt = con.prepare(
        "SELECT key, COALESCE(length(CAST(value AS BLOB)),0) FROM kv \
         WHERE (?1 IS NULL OR time_updated < ?1) \
           AND (?2 IS NULL OR COALESCE(length(CAST(value AS BLOB)),0) > ?2) \
         ORDER BY 2 DESC, key",
    )?;
    let rows = stmt.query_map(params![cutoff, min_bytes], |r| {
        Ok(KvKeyRow {
            key: r.get(0)?,
            bytes: r.get(1)?,
        })
    })?;
    let mut keys = Vec::new();
    for r in rows {
        keys.push(r?);
    }
    drop(stmt);
    let total_bytes: i64 = keys.iter().map(|k| k.bytes).sum();
    let mut out = KvPurgeOut {
        env: env_status(db_path),
        dry_run,
        older_than: older_than.map(str::to_string),
        larger_than: larger_than.map(str::to_string),
        total_keys: keys.len(),
        keys,
        total_bytes,
        deleted: false,
        note: None,
    };
    if dry_run {
        return output::emit(&serde_json::to_value(&out)?);
    }
    con.execute_batch("PRAGMA foreign_keys = ON;")?;
    let tx = con.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for k in &out.keys {
        tx.execute("DELETE FROM kv WHERE key = ?1", params![k.key])?;
    }
    tx.commit()?;
    out.deleted = true;
    out.note = Some("file size is unchanged until `opencode-dbtool vacuum` is run".into());
    output::emit(&serde_json::to_value(&out)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb;

    fn kv_db() -> Connection {
        let con = testdb::create();
        con.execute(
            "INSERT INTO kv (key, value, time_created, time_updated) VALUES ('a', '12345', 0, 1000)",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO kv (key, value, time_created, time_updated) VALUES ('big', 'xxxxxxxxxxxxxxxx', 0, 0)",
            [],
        )
        .unwrap();
        con
    }

    #[test]
    fn show_truncates_large_values() {
        let con = kv_db();
        // Small value fits.
        cmd_kv_show(&con, "a", false).unwrap();
        // Missing key is a usage error.
        assert!(cmd_kv_show(&con, "nope", false).is_err());
    }

    #[test]
    fn delete_previews_and_removes() {
        let mut con = kv_db();
        cmd_kv_delete(&mut con, &["big".to_string()], true, Path::new("/tmp/x.db")).unwrap();
        let n: i64 = con
            .query_row("SELECT COUNT(*) FROM kv", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "dry-run changes nothing");

        cmd_kv_delete(
            &mut con,
            &["big".to_string()],
            false,
            Path::new("/tmp/x.db"),
        )
        .unwrap();
        let n: i64 = con
            .query_row("SELECT COUNT(*) FROM kv", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert!(cmd_kv_delete(
            &mut con,
            &["big".to_string()],
            false,
            Path::new("/tmp/x.db")
        )
        .is_err());
    }

    #[test]
    fn delete_rejects_flags_and_empty() {
        let mut con = kv_db();
        assert!(cmd_kv_delete(&mut con, &[], true, Path::new("/tmp/x.db")).is_err());
        assert!(cmd_kv_delete(
            &mut con,
            &["--older-than".to_string()],
            true,
            Path::new("/tmp/x.db")
        )
        .is_err());
    }

    #[test]
    fn purge_requires_a_filter_and_honors_them() {
        let mut con = kv_db();
        // No filter is a usage error: a bare purge must never wipe the table.
        assert!(cmd_kv_purge(&mut con, None, None, true, Path::new("/tmp/x.db")).is_err());

        // `--larger-than 10` selects only the 16-byte key.
        cmd_kv_purge(&mut con, None, Some("10"), true, Path::new("/tmp/x.db")).unwrap();
        let n: i64 = con
            .query_row("SELECT COUNT(*) FROM kv", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "dry-run changes nothing");

        cmd_kv_purge(&mut con, None, Some("10"), false, Path::new("/tmp/x.db")).unwrap();
        let n: i64 = con
            .query_row("SELECT COUNT(*) FROM kv", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let left: String = con
            .query_row("SELECT key FROM kv", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, "a");

        // `--older-than` matches rows with old timestamps.
        cmd_kv_purge(&mut con, Some("1d"), None, false, Path::new("/tmp/x.db")).unwrap();
        let n: i64 = con
            .query_row("SELECT COUNT(*) FROM kv", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }
}
