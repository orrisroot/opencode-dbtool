//! `report`: read-only suggestions for reclaiming space.
//!
//! Never deletes anything: it aggregates the usual candidates (WAL,
//! freelist, old sessions, large kv caches, backups) and pairs each with
//! the command that would act on it.

use crate::cli::ReportArgs;
use crate::db::{env_status, file_size, EnvStatus};
use crate::error::Result;
use crate::output;
use crate::output::human_bytes;
use crate::repo;
use crate::util::{dt, now_ms, parse_age_ms, shell_quote};
use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

/// kv entries at least this large are reported (1 MiB).
const KV_MIN_BYTES: i64 = 1024 * 1024;
/// Sessions older than this are reported.
const OLD_DAYS: i64 = 90;
/// Report the largest N sessions.
const TOP_SESSIONS: usize = 5;

#[derive(Serialize)]
struct KvCandidate {
    key: String,
    bytes: i64,
    updated: String,
    command: String,
}

#[derive(Serialize)]
struct SessionCandidate {
    id: String,
    title: String,
    bytes: i64,
    updated: String,
}

#[derive(Serialize)]
struct OldSessions {
    days: i64,
    count: usize,
    bytes: i64,
    command: String,
}

#[derive(Serialize)]
struct BackupsInfo {
    count: usize,
    newest: Option<String>,
    total_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
}

#[derive(Serialize)]
struct Suggestion {
    action: String,
    command: String,
}

#[derive(Serialize)]
struct ProjectCost {
    id: String,
    name: String,
    worktree: String,
    cost: f64,
}

#[derive(Serialize)]
struct DayCost {
    day: String,
    cost: f64,
    sessions: i64,
}

#[derive(Serialize)]
struct CostsOut {
    total: f64,
    by_project: Vec<ProjectCost>,
    by_day: Vec<DayCost>,
}

/// Active `--since`/`--project` scope (present only when filtering).
#[derive(Serialize)]
struct ScopeOut {
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
}

/// Session-derived sections are limited to this scope.
pub struct ReportOptions<'a> {
    pub costs: bool,
    pub since: Option<&'a str>,
    pub project: Option<&'a str>,
}

#[derive(Serialize)]
struct ReportOut {
    #[serde(flatten)]
    env: EnvStatus,
    db_bytes: u64,
    wal_bytes: u64,
    reclaimable_bytes: i64,
    old_sessions: OldSessions,
    largest_sessions: Vec<SessionCandidate>,
    kv_candidates: Vec<KvCandidate>,
    backups: BackupsInfo,
    suggestions: Vec<Suggestion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<ScopeOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    costs: Option<CostsOut>,
}

pub fn cmd_report(con: &Connection, db_path: &Path, args: &ReportArgs) -> Result<()> {
    let opts = ReportOptions {
        costs: args.costs,
        since: args.since.as_deref(),
        project: args.project.as_deref(),
    };
    let value = report_value(con, db_path, &opts)?;
    if output::table_mode() {
        output::emit_text(&report_table(&value))
    } else {
        output::emit(&value)
    }
}

/// Build the report (exposed for tests).
pub fn report_value(
    con: &Connection,
    db_path: &Path,
    opts: &ReportOptions,
) -> Result<serde_json::Value> {
    let cutoff = match opts.since {
        Some(age) => Some(now_ms()? - parse_age_ms(age)?),
        None => None,
    };
    let project_id = match opts.project {
        Some(reference) => Some(repo::resolve_project(con, reference)?.id),
        None => None,
    };
    let scoped = cutoff.is_some() || project_id.is_some();

    let freelist: i64 = con.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    let page_size: i64 = con.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let wal_bytes = file_size(&db_path.with_extension("db-wal"));
    let reclaimable = freelist * page_size;

    // Old sessions (default 90 days), limited to the scope.
    let cutoff_old = now_ms()? - OLD_DAYS * 86_400_000;
    let mut meta = repo::load_session_meta(con)?;
    if let Some(cutoff) = cutoff {
        meta.retain(|m| m.updated >= cutoff);
    }
    if let Some(project) = project_id.as_deref() {
        meta.retain(|m| m.project_id.as_deref() == Some(project));
    }
    let old_ids: Vec<String> = meta
        .iter()
        .filter(|m| m.updated < cutoff_old)
        .map(|m| m.id.clone())
        .collect();
    let old_sizes = repo::session_sizes(con, &old_ids)?;
    let old_bytes: i64 = old_sizes.values().sum();

    // Largest sessions, limited to the scope.
    let mut sessions = repo::load_sessions(con)?;
    if scoped {
        let scoped_ids: std::collections::HashSet<&str> =
            meta.iter().map(|m| m.id.as_str()).collect();
        sessions.retain(|s| scoped_ids.contains(s.id.as_str()));
    }
    sessions.sort_by(|a, b| {
        b.size_bytes()
            .cmp(&a.size_bytes())
            .then_with(|| a.id.cmp(&b.id))
    });
    let largest: Vec<SessionCandidate> = sessions
        .iter()
        .take(TOP_SESSIONS)
        .map(|s| SessionCandidate {
            id: s.id.clone(),
            title: s.title.clone(),
            bytes: s.size_bytes(),
            updated: dt(s.updated),
        })
        .collect();

    // Large kv caches.
    let kv_candidates: Vec<KvCandidate> = {
        let mut stmt = con.prepare(
            "SELECT key, COALESCE(length(CAST(value AS BLOB)),0), time_updated FROM kv \
             WHERE length(CAST(value AS BLOB)) >= ?1 ORDER BY 2 DESC LIMIT 10",
        )?;
        let rows = stmt.query_map([KV_MIN_BYTES], |r| {
            let key: String = r.get(0)?;
            Ok(KvCandidate {
                bytes: r.get(1)?,
                updated: dt(r.get(2)?),
                command: format!("opencode-dbtool kv delete {} --yes", shell_quote(&key)),
                key,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };

    // Backups.
    let backups = crate::commands::vacuum::list_backup_files(db_path, false)?;
    let backups_info = BackupsInfo {
        count: backups.len(),
        newest: backups.first().map(|b| b.file.clone()),
        total_bytes: backups.iter().map(|b| b.bytes).sum(),
        command: (backups.len() > 5)
            .then(|| "opencode-dbtool backup --keep-backups 3 --yes".to_string()),
    };

    // Suggestions, most impactful first.
    let mut suggestions = Vec::new();
    if wal_bytes >= KV_MIN_BYTES as u64 {
        suggestions.push(Suggestion {
            action: format!(
                "WAL file is {}; checkpoint it",
                human_bytes(wal_bytes as i64)
            ),
            command: "opencode-dbtool db checkpoint --truncate".to_string(),
        });
    }
    if reclaimable >= KV_MIN_BYTES {
        suggestions.push(Suggestion {
            action: format!("VACUUM can reclaim {}", human_bytes(reclaimable)),
            command: "opencode-dbtool vacuum --online --yes".to_string(),
        });
    }
    if !old_ids.is_empty() {
        suggestions.push(Suggestion {
            action: format!(
                "{} session(s) older than {OLD_DAYS}d use {}",
                old_ids.len(),
                human_bytes(old_bytes)
            ),
            command: format!("opencode-dbtool session purge --older-than {OLD_DAYS}d --dry-run"),
        });
    }
    if let Some(kv) = kv_candidates.first() {
        suggestions.push(Suggestion {
            action: format!("kv cache {} is {}", kv.key, human_bytes(kv.bytes)),
            command: kv.command.clone(),
        });
    }
    if let Some(command) = &backups_info.command {
        suggestions.push(Suggestion {
            action: format!(
                "{} backups use {}",
                backups.len(),
                human_bytes(backups_info.total_bytes as i64)
            ),
            command: command.clone(),
        });
    }

    let out = ReportOut {
        env: env_status(db_path),
        db_bytes: file_size(db_path),
        wal_bytes,
        reclaimable_bytes: reclaimable,
        old_sessions: OldSessions {
            days: OLD_DAYS,
            count: old_ids.len(),
            bytes: old_bytes,
            command: format!("opencode-dbtool session purge --older-than {OLD_DAYS}d --dry-run"),
        },
        largest_sessions: largest,
        kv_candidates,
        backups: backups_info,
        suggestions,
        scope: scoped.then(|| ScopeOut {
            since: opts.since.map(str::to_string),
            project: project_id.clone(),
        }),
        costs: if opts.costs {
            Some(costs_out(
                con,
                cutoff,
                project_id.as_deref(),
                &meta,
                scoped,
            )?)
        } else {
            None
        },
    };
    Ok(serde_json::to_value(&out)?)
}

/// Cost aggregates by project and by update day, limited to the scope.
fn costs_out(
    con: &Connection,
    cutoff: Option<i64>,
    project: Option<&str>,
    meta: &[crate::models::SessionMeta],
    scoped: bool,
) -> Result<CostsOut> {
    let total: f64 = con.query_row(
        "SELECT COALESCE(SUM(cost),0) FROM \"session_v2\" \
         WHERE (?1 IS NULL OR time_updated >= ?1) \
           AND (?2 IS NULL OR project_id = ?2)",
        params![cutoff, project],
        |r| r.get(0),
    )?;
    let total = crate::util::round4(total);

    // Unscoped reports keep listing every project (including empty ones);
    // a scoped report recomputes from the filtered session set.
    let mut by_project: Vec<ProjectCost> = if scoped {
        let mut names: HashMap<String, (String, String)> = repo::load_projects(con)?
            .into_iter()
            .map(|p| (p.id, (p.name, p.worktree)))
            .collect();
        let mut costs: HashMap<String, f64> = HashMap::new();
        for m in meta {
            if let Some(id) = &m.project_id {
                *costs.entry(id.clone()).or_default() += m.cost;
            }
        }
        costs
            .into_iter()
            .map(|(id, cost)| {
                let (name, worktree) = names.remove(&id).unwrap_or_default();
                ProjectCost {
                    id,
                    name,
                    worktree,
                    cost: crate::util::round4(cost),
                }
            })
            .collect()
    } else {
        repo::load_projects(con)?
            .into_iter()
            .map(|p| ProjectCost {
                id: p.id,
                name: p.name,
                worktree: p.worktree,
                cost: crate::util::round4(p.cost),
            })
            .collect()
    };
    by_project.sort_by(|a, b| {
        b.cost
            .total_cmp(&a.cost)
            .then_with(|| a.worktree.cmp(&b.worktree))
    });
    by_project.truncate(10);

    let by_day: Vec<DayCost> = {
        let mut stmt = con.prepare(
            "SELECT date(time_updated/1000, 'unixepoch', 'localtime'), \
             COALESCE(SUM(cost),0), COUNT(*) \
             FROM \"session_v2\" \
             WHERE (?1 IS NULL OR time_updated >= ?1) \
               AND (?2 IS NULL OR project_id = ?2) \
             GROUP BY 1 ORDER BY 1 DESC LIMIT 30",
        )?;
        let rows = stmt.query_map(params![cutoff, project], |r| {
            Ok(DayCost {
                day: r.get(0)?,
                cost: crate::util::round4(r.get(1)?),
                sessions: r.get(2)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    Ok(CostsOut {
        total,
        by_project,
        by_day,
    })
}
/// Curated table-mode rendering.
fn report_table(value: &serde_json::Value) -> String {
    let num = |v: &serde_json::Value, key: &str| v.get(key).and_then(|x| x.as_i64()).unwrap_or(0);
    let mut lines: Vec<(String, String)> = Vec::new();
    lines.push(("db".into(), human_bytes(num(value, "db_bytes"))));
    lines.push(("wal".into(), human_bytes(num(value, "wal_bytes"))));
    lines.push((
        "reclaimable".into(),
        human_bytes(num(value, "reclaimable_bytes")),
    ));
    if let Some(scope) = value.get("scope") {
        let mut parts = Vec::new();
        if let Some(since) = scope.get("since").and_then(|v| v.as_str()) {
            parts.push(format!("since {since}"));
        }
        if let Some(project) = scope.get("project").and_then(|v| v.as_str()) {
            parts.push(format!("project {project}"));
        }
        if !parts.is_empty() {
            lines.push(("scope".into(), parts.join(", ")));
        }
    }
    let old = value.get("old_sessions").cloned().unwrap_or_default();
    lines.push((
        "old sessions".into(),
        format!(
            "{} older than {}d ({})",
            old.get("count").and_then(|x| x.as_u64()).unwrap_or(0),
            old.get("days").and_then(|x| x.as_i64()).unwrap_or(0),
            human_bytes(old.get("bytes").and_then(|x| x.as_i64()).unwrap_or(0)),
        ),
    ));
    if let Some(top) = value
        .get("largest_sessions")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
    {
        lines.push((
            "largest session".into(),
            format!(
                "{} ({})",
                top.get("id").and_then(|x| x.as_str()).unwrap_or("-"),
                human_bytes(top.get("bytes").and_then(|x| x.as_i64()).unwrap_or(0)),
            ),
        ));
    }
    if let Some(kv) = value
        .get("kv_candidates")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
    {
        lines.push((
            "largest kv".into(),
            format!(
                "{} ({})",
                kv.get("key").and_then(|x| x.as_str()).unwrap_or("-"),
                human_bytes(kv.get("bytes").and_then(|x| x.as_i64()).unwrap_or(0)),
            ),
        ));
    }
    let backups = value.get("backups").cloned().unwrap_or_default();
    lines.push((
        "backups".into(),
        format!(
            "{} ({})",
            backups.get("count").and_then(|x| x.as_u64()).unwrap_or(0),
            human_bytes(
                backups
                    .get("total_bytes")
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0)
            ),
        ),
    ));
    lines.push(("".into(), String::new()));
    lines.push(("suggestions".into(), String::new()));
    if let Some(items) = value.get("suggestions").and_then(|v| v.as_array()) {
        for item in items {
            lines.push((
                "".into(),
                format!(
                    "- {}: {}",
                    item.get("action").and_then(|x| x.as_str()).unwrap_or(""),
                    item.get("command").and_then(|x| x.as_str()).unwrap_or(""),
                ),
            ));
        }
    }
    if let Some(costs) = value.get("costs") {
        lines.push(("".into(), String::new()));
        lines.push((
            "costs".into(),
            format!(
                "total {}",
                costs.get("total").and_then(|x| x.as_f64()).unwrap_or(0.0)
            ),
        ));
        if let Some(items) = costs.get("by_project").and_then(|v| v.as_array()) {
            for item in items.iter().take(5) {
                lines.push((
                    "".into(),
                    format!(
                        "- {}: {}",
                        item.get("worktree").and_then(|x| x.as_str()).unwrap_or(""),
                        item.get("cost").and_then(|x| x.as_f64()).unwrap_or(0.0),
                    ),
                ));
            }
        }
    }
    let width = lines
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    lines
        .iter()
        .map(|(k, v)| {
            if v.is_empty() {
                k.clone()
            } else {
                format!("{k:width$}  {v}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb;

    #[test]
    fn report_lists_candidates_and_suggestions() {
        let dir = testdb::temp_data_dir("report");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);
        testdb::insert_session_at(&con, "old", "/a", None, 0);
        testdb::insert_session_at(&con, "new", "/a", None, crate::util::now_ms().unwrap());
        con.execute(
            "INSERT INTO kv (key, value, time_created, time_updated) VALUES ('big', ?1, 0, 0)",
            ["x".repeat(2 * 1024 * 1024)],
        )
        .unwrap();
        // A session without a project still counts toward the total cost.
        con.execute("UPDATE session_v2 SET cost = 0.75 WHERE id = 'old'", [])
            .unwrap();

        let v = report_value(
            &con,
            &db_path,
            &ReportOptions {
                costs: true,
                since: None,
                project: None,
            },
        )
        .unwrap();
        assert_eq!(v["old_sessions"]["count"], 1);
        assert_eq!(v["kv_candidates"][0]["key"], "big");
        assert_eq!(v["costs"]["total"], 0.75);
        assert!(v["costs"]["by_project"].is_array());
        assert!(v["costs"]["by_day"].is_array());
        assert!(
            v["suggestions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["command"].as_str().unwrap().contains("kv delete")),
            "suggestions: {}",
            v["suggestions"]
        );
        assert!(
            v["suggestions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["command"].as_str().unwrap().contains("session purge")),
            "suggestions: {}",
            v["suggestions"]
        );

        drop(con);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn report_scope_filters_sessions_and_costs() {
        let dir = testdb::temp_data_dir("report-scope");
        let db_path = dir.join("opencode.db");
        let con = testdb::create_at(&db_path);
        con.execute(
            "INSERT INTO project (id, worktree, name) VALUES ('p1','/a','p1')",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO project (id, worktree, name) VALUES ('p2','/b','p2')",
            [],
        )
        .unwrap();
        let now = crate::util::now_ms().unwrap();
        for (id, project, updated, cost) in [
            ("old1", "p1", 0, 1.0),
            ("new1", "p1", now, 2.0),
            ("new2", "p2", now, 4.0),
        ] {
            con.execute(
                "INSERT INTO session_v2 (id, directory, title, project_id, time_updated, cost) \
                 VALUES (?1, '/a', 't', ?2, ?3, ?4)",
                rusqlite::params![id, project, updated, cost],
            )
            .unwrap();
        }

        // --since limits every session-derived section.
        let v = report_value(
            &con,
            &db_path,
            &ReportOptions {
                costs: true,
                since: Some("1d"),
                project: None,
            },
        )
        .unwrap();
        assert_eq!(v["scope"]["since"], "1d");
        assert_eq!(v["costs"]["total"], 6.0);
        assert_eq!(v["old_sessions"]["count"], 0);
        assert_eq!(v["largest_sessions"].as_array().unwrap().len(), 2);

        // --project limits to one project (id, prefix, or worktree).
        let v = report_value(
            &con,
            &db_path,
            &ReportOptions {
                costs: true,
                since: None,
                project: Some("p1"),
            },
        )
        .unwrap();
        assert_eq!(v["scope"]["project"], "p1");
        assert_eq!(v["costs"]["total"], 3.0);
        assert_eq!(v["old_sessions"]["count"], 1);
        assert_eq!(v["costs"]["by_project"][0]["id"], "p1");

        drop(con);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
