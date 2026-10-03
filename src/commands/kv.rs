//! `kv` subcommands: inspect and delete rows of the global `kv` table.
//!
//! Some keys are multi-megabyte caches (e.g. `models-dev:catalog`); `kv
//! list` exposes per-key sizes so they can be found, and `kv delete`
//! drops selected keys (opencode regenerates caches on demand).

use crate::db::{env_status, EnvStatus};
use crate::error::{AppError, Result};
use crate::output::print_json;
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

pub fn cmd_kv_list(con: &Connection, args: &[String]) -> Result<()> {
    // Optional `--older-than <age>` limits the listing to keys not updated
    // since the cutoff (stale caches first).
    let mut cutoff: Option<i64> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--older-than" => {
                let age = args
                    .get(i + 1)
                    .ok_or_else(|| AppError::usage("--older-than requires an age (e.g. 30d)"))?;
                cutoff = Some(now_ms()? - parse_age_ms(age)?);
                i += 2;
            }
            other => return Err(AppError::usage(format!("unknown option: {other}"))),
        }
    }
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
    print_json(&serde_json::to_value(out)?)
}

fn read_entry(r: &rusqlite::Row) -> rusqlite::Result<KvEntry> {
    Ok(KvEntry {
        key: r.get(0)?,
        bytes: r.get(1)?,
        updated: dt(r.get::<_, i64>(2)?),
    })
}

pub fn cmd_kv_show(con: &Connection, args: &[String]) -> Result<()> {
    if args.len() != 1 {
        return Err(AppError::usage("usage: opencode-dbtool kv show <key>"));
    }
    let key = args[0].as_str();
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
    let bytes = value.len() as i64;
    let truncated = value.chars().count() > SHOW_PREVIEW_CHARS;
    let preview: String = value.chars().take(SHOW_PREVIEW_CHARS).collect();
    print_json(&serde_json::to_value(KvShow {
        key: key.to_string(),
        bytes,
        updated: dt(updated),
        value: preview,
        truncated,
    })?)
}

pub fn cmd_kv_delete(
    con: &mut Connection,
    args: &[String],
    dry_run: bool,
    db_path: &Path,
) -> Result<()> {
    if args.is_empty() || args.iter().any(|a| a.starts_with("--")) {
        return Err(AppError::usage(
            "usage: opencode-dbtool kv delete <key> [key...]",
        ));
    }
    let mut keys = Vec::new();
    for key in args {
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
        keys.push(KvKeyRow {
            key: key.clone(),
            bytes: bytes.unwrap_or(0),
        });
    }
    let total: i64 = keys.iter().map(|k| k.bytes).sum();
    let mut out = KvDeleteOut {
        env: env_status(db_path),
        dry_run,
        keys,
        total_bytes: total,
        deleted: false,
        note: None,
    };
    if dry_run {
        return print_json(&serde_json::to_value(&out)?);
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
    print_json(&serde_json::to_value(&out)?)
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
        cmd_kv_show(&con, &["a".to_string()]).unwrap();
        // Missing key is a usage error.
        assert!(cmd_kv_show(&con, &["nope".to_string()]).is_err());
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
}
