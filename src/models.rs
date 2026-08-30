//! Read models shared by list/show/delete commands, plus JSON conversion
//! and in-memory filtering.

use crate::util::{dt, round4};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct SessionRow {
    pub id: String,
    pub title: String,
    pub directory: String,
    pub parent_id: Option<String>,
    pub updated: i64,
    pub msgs: i64,
    pub msg_bytes: i64,
    pub parts: i64,
    pub part_bytes: i64,
    pub events: i64,
    pub event_bytes: i64,
    pub cost: f64,
}

impl SessionRow {
    pub fn size_bytes(&self) -> i64 {
        self.msg_bytes + self.part_bytes + self.event_bytes
    }
}

pub fn session_json(s: &SessionRow) -> Value {
    json!({
        "id": s.id,
        "title": s.title,
        "directory": s.directory,
        "parent_id": s.parent_id,
        "updated": dt(s.updated),
        "msgs": s.msgs,
        "parts": s.parts,
        "events": s.events,
        "size_bytes": s.size_bytes(),
        "cost": round4(s.cost),
    })
}

/// Filters for `session purge` / `session strip-reasoning`; set filters
/// combine with AND. `older_than` compares against `time_updated`.
pub struct PurgeFilter {
    /// Raw `--older-than` argument as given (for JSON output).
    pub older_than_raw: Option<String>,
    /// Parsed cutoff (epoch ms); sessions with `time_updated >=` this
    /// value are excluded. `None` when `--older-than` was not given.
    pub cutoff_ms: Option<i64>,
    /// Only subagent sessions (`parent_id` set).
    pub subagents: bool,
    /// Exact session `directory` matches (repeatable, OR).
    pub paths: Vec<String>,
}

impl PurgeFilter {
    pub fn is_empty(&self) -> bool {
        self.older_than_raw.is_none() && !self.subagents && self.paths.is_empty()
    }

    pub fn matches(&self, s: &SessionRow) -> bool {
        if let Some(cutoff) = self.cutoff_ms {
            if s.updated >= cutoff {
                return false;
            }
        }
        if self.subagents && s.parent_id.is_none() {
            return false;
        }
        if !self.paths.is_empty() {
            let dir = s.directory.trim_end_matches('/');
            if !self.paths.iter().any(|p| p.trim_end_matches('/') == dir) {
                return false;
            }
        }
        true
    }

    /// Stable JSON contract for the `filters` field in purge output.
    pub fn json(&self) -> Value {
        json!({
            "older_than": self.older_than_raw,
            "subagents": self.subagents,
            "paths": self.paths,
        })
    }
}

#[derive(Clone)]
pub struct ProjectRow {
    pub id: String,
    pub worktree: String,
    pub name: String,
    pub sessions: i64,
    pub msgs: i64,
    pub msg_bytes: i64,
    pub parts: i64,
    pub part_bytes: i64,
    pub events: i64,
    pub event_bytes: i64,
    pub cost: f64,
    pub updated: i64,
}

pub fn project_total_bytes(p: &ProjectRow) -> i64 {
    p.msg_bytes + p.part_bytes + p.event_bytes
}

pub fn project_json(p: &ProjectRow) -> Value {
    json!({
        "id": p.id,
        "worktree": p.worktree,
        "name": p.name,
        "sessions": p.sessions,
        "msgs": p.msgs,
        "parts": p.parts,
        "events": p.events,
        "size_bytes": project_total_bytes(p),
        "cost": round4(p.cost),
        "updated": dt(p.updated),
    })
}

/// Filter projects by exact worktree match (trailing slash tolerant, OR).
pub fn filter_projects<'a>(projects: &'a [ProjectRow], paths: &[&str]) -> Vec<&'a ProjectRow> {
    if paths.is_empty() {
        return projects.iter().collect();
    }
    let trimmed: Vec<&str> = paths.iter().map(|d| d.trim_end_matches('/')).collect();
    projects
        .iter()
        .filter(|p| trimmed.iter().any(|d| p.worktree == *d))
        .collect()
}

/// Filters for `project purge`; set filters combine with AND.
/// `older_than` compares against the project's latest session activity
/// (the max `time_updated` of its sessions).
pub struct ProjectFilter {
    /// Raw `--older-than` argument as given (for JSON output).
    pub older_than_raw: Option<String>,
    /// Parsed cutoff (epoch ms); projects whose latest session activity
    /// is `>=` this value are excluded.
    pub cutoff_ms: Option<i64>,
    /// Exact project `worktree` matches (repeatable, OR).
    pub paths: Vec<String>,
}

impl ProjectFilter {
    pub fn is_empty(&self) -> bool {
        self.older_than_raw.is_none() && self.paths.is_empty()
    }

    pub fn matches(&self, p: &ProjectRow) -> bool {
        if let Some(cutoff) = self.cutoff_ms {
            if p.updated >= cutoff {
                return false;
            }
        }
        if !self.paths.is_empty() {
            let wt = p.worktree.trim_end_matches('/');
            if !self.paths.iter().any(|d| d.trim_end_matches('/') == wt) {
                return false;
            }
        }
        true
    }

    /// Stable JSON contract for the `filters` field in purge output.
    pub fn json(&self) -> Value {
        json!({
            "older_than": self.older_than_raw,
            "paths": self.paths,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::load_projects;
    use crate::testdb;

    fn row(dir: &str) -> SessionRow {
        SessionRow {
            id: String::new(),
            title: String::new(),
            directory: dir.to_string(),
            parent_id: None,
            updated: 0,
            msgs: 0,
            msg_bytes: 0,
            parts: 0,
            part_bytes: 0,
            events: 0,
            event_bytes: 0,
            cost: 0.0,
        }
    }

    fn filter(f: &PurgeFilter, s: &SessionRow) -> bool {
        f.matches(s)
    }

    #[test]
    fn path_is_exact_match_only() {
        let f = PurgeFilter {
            older_than_raw: None,
            cutoff_ms: None,
            subagents: false,
            paths: vec!["/home/okumura/work/misc".to_string()],
        };
        let sessions = [
            row("/home/okumura/work/misc"),
            row("/home/okumura/work/misc/opencode-dbtool"),
            row("/home/okumura/work/misc/opencode-dbtool/sub"),
        ];
        let hit: Vec<_> = sessions
            .iter()
            .filter(|s| filter(&f, s))
            .map(|s| s.directory.clone())
            .collect();
        assert_eq!(hit, vec!["/home/okumura/work/misc"]);
    }

    #[test]
    fn trailing_slash_stripped() {
        let f = PurgeFilter {
            older_than_raw: None,
            cutoff_ms: None,
            subagents: false,
            paths: vec!["/a/b/".to_string()],
        };
        assert!(filter(&f, &row("/a/b")));
        assert!(!filter(&f, &row("/a")));
    }

    #[test]
    fn multiple_paths_are_or() {
        let f = PurgeFilter {
            older_than_raw: None,
            cutoff_ms: None,
            subagents: false,
            paths: vec!["/a".to_string(), "/b".to_string()],
        };
        assert!(filter(&f, &row("/a")));
        assert!(filter(&f, &row("/b")));
        assert!(!filter(&f, &row("/c")));
    }

    #[test]
    fn empty_paths_match_all() {
        let f = PurgeFilter {
            older_than_raw: None,
            cutoff_ms: None,
            subagents: false,
            paths: Vec::new(),
        };
        assert!(filter(&f, &row("/a")));
        assert!(filter(&f, &row("/b")));
    }

    #[test]
    fn subagents_only() {
        let f = PurgeFilter {
            older_than_raw: None,
            cutoff_ms: None,
            subagents: true,
            paths: Vec::new(),
        };
        let mut root = row("/a");
        root.parent_id = None;
        let mut child = row("/a");
        child.parent_id = Some("p".to_string());
        assert!(!filter(&f, &root));
        assert!(filter(&f, &child));
    }

    #[test]
    fn older_than_cutoff_is_strict() {
        let f = PurgeFilter {
            older_than_raw: Some("30d".to_string()),
            cutoff_ms: Some(1000),
            subagents: false,
            paths: Vec::new(),
        };
        let mut old = row("/a");
        old.updated = 999;
        let mut boundary = row("/a");
        boundary.updated = 1000;
        let mut recent = row("/a");
        recent.updated = 1001;
        assert!(filter(&f, &old));
        assert!(!filter(&f, &boundary));
        assert!(!filter(&f, &recent));
    }

    #[test]
    fn filters_combine_with_and() {
        let f = PurgeFilter {
            older_than_raw: Some("30d".to_string()),
            cutoff_ms: Some(1000),
            subagents: true,
            paths: vec!["/a".to_string()],
        };
        let mut old_child_in_a = row("/a");
        old_child_in_a.parent_id = Some("p".to_string());
        old_child_in_a.updated = 0;
        let mut recent_child_in_a = old_child_in_a.clone();
        recent_child_in_a.updated = 2000;
        let mut old_root_in_a = old_child_in_a.clone();
        old_root_in_a.parent_id = None;
        let mut old_child_in_b = old_child_in_a.clone();
        old_child_in_b.directory = "/b".to_string();
        assert!(filter(&f, &old_child_in_a));
        assert!(!filter(&f, &recent_child_in_a));
        assert!(!filter(&f, &old_root_in_a));
        assert!(!filter(&f, &old_child_in_b));
    }

    #[test]
    fn empty_filter_matches_all() {
        let f = PurgeFilter {
            older_than_raw: None,
            cutoff_ms: None,
            subagents: false,
            paths: Vec::new(),
        };
        assert!(f.is_empty());
        assert!(filter(&f, &row("/a")));
    }

    #[test]
    fn list_filters_by_path() {
        let con = testdb::create();
        for (id, worktree) in [("p1", "/a"), ("p2", "/a"), ("p3", "/b")] {
            testdb::insert_project(&con, id, worktree);
        }
        let projects = load_projects(&con).unwrap();

        let hit: Vec<String> = filter_projects(&projects, &["/a"])
            .iter()
            .map(|p| p.id.clone())
            .collect();
        assert_eq!(hit, vec!["p1", "p2"]);

        let hit: Vec<String> = filter_projects(&projects, &["/a", "/b"])
            .iter()
            .map(|p| p.id.clone())
            .collect();
        assert_eq!(hit, vec!["p1", "p2", "p3"]);

        let hit: Vec<String> = filter_projects(&projects, &["/nope"])
            .iter()
            .map(|p| p.id.clone())
            .collect();
        assert_eq!(hit, Vec::<String>::new());

        assert_eq!(filter_projects(&projects, &[]).len(), 3);
    }
}
