//! Read models shared by list/show/delete commands, plus JSON conversion
//! and in-memory filtering. V2-only.

use crate::util::{dt, round4};
use serde::Serialize;

#[derive(Clone)]
pub struct SessionRow {
    pub id: String,
    pub title: String,
    pub directory: String,
    pub parent_id: Option<String>,
    pub updated: i64,
    pub archived: bool,
    pub events: i64,
    pub event_bytes: i64,
    pub sm_msgs: i64,
    pub sm_bytes: i64,
    pub inbox_msgs: i64,
    pub inbox_bytes: i64,
    pub pending_msgs: i64,
    pub pending_bytes: i64,
    pub instr_bytes: i64,
    pub cost: f64,
}

impl SessionRow {
    pub fn size_bytes(&self) -> i64 {
        self.event_bytes + self.sm_bytes + self.inbox_bytes + self.pending_bytes + self.instr_bytes
    }
}

/// JSON shape of a session in `session list` / `session show`.
#[derive(Serialize)]
pub struct SessionOut {
    pub id: String,
    pub title: String,
    pub directory: String,
    pub parent_id: Option<String>,
    pub updated: String,
    pub archived: bool,
    pub events: i64,
    pub session_messages: i64,
    pub inbox: i64,
    pub pending: i64,
    pub size_bytes: i64,
    pub cost: f64,
}

pub fn session_json(s: &SessionRow) -> SessionOut {
    SessionOut {
        id: s.id.clone(),
        title: s.title.clone(),
        directory: s.directory.clone(),
        parent_id: s.parent_id.clone(),
        updated: dt(s.updated),
        archived: s.archived,
        events: s.events,
        session_messages: s.sm_msgs,
        inbox: s.inbox_msgs,
        pending: s.pending_msgs,
        size_bytes: s.size_bytes(),
        cost: round4(s.cost),
    }
}

/// Lightweight session fields used by filter selection (no per-table
/// aggregates). Ordering matches `load_session_meta`.
#[derive(Clone)]
pub struct SessionMeta {
    pub id: String,
    pub directory: String,
    pub parent_id: Option<String>,
    pub updated: i64,
    pub project_id: Option<String>,
    pub archived: bool,
}

/// Filters for `session purge` / `session strip-reasoning`; set filters
/// combine with AND. `older_than` compares against `time_updated`,
/// `larger_than` against the session's own `size_bytes`.
/// `keep_latest` is applied after the other filters (see the purge
/// commands for the ancestor-protection rules).
#[derive(Default)]
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
    /// Raw `--larger-than` argument as given (for JSON output).
    pub larger_than_raw: Option<String>,
    /// Minimum own size in bytes; sessions with `size_bytes <=` this
    /// value are excluded.
    pub larger_than_bytes: Option<i64>,
    /// Raw `--keep-latest` argument as given (for JSON output).
    pub keep_latest_raw: Option<String>,
    /// Keep the N most recent matching sessions (and, for purge, their
    /// ancestors); `0` keeps nothing.
    pub keep_latest: Option<i64>,
    /// Raw `--keep-latest-per-project` argument as given (for JSON output).
    pub keep_latest_per_project_raw: Option<String>,
    /// Keep the N most recent matching sessions per project (and, for
    /// purge, their ancestors); `0` keeps nothing. Mutually exclusive
    /// with `keep_latest`.
    pub keep_latest_per_project: Option<i64>,
    /// Only archived sessions (`time_archived` set).
    pub archived: bool,
    /// Exact session `directory` prefixes (repeatable, OR): matches the
    /// directory itself and anything below it.
    pub path_prefixes: Vec<String>,
    /// Only sessions with no content rows (zero size).
    pub empty: bool,
}

impl PurgeFilter {
    pub fn is_empty(&self) -> bool {
        self.older_than_raw.is_none()
            && !self.subagents
            && self.paths.is_empty()
            && self.larger_than_raw.is_none()
            && self.keep_latest_raw.is_none()
            && self.keep_latest_per_project_raw.is_none()
            && !self.archived
            && self.path_prefixes.is_empty()
            && !self.empty
    }

    /// Same as `matches_meta` plus the size check (which needs the
    /// per-table aggregates); used by unit tests and `--larger-than`
    /// callers that already hold a `SessionRow`.
    #[cfg(test)]
    pub fn matches(&self, s: &SessionRow) -> bool {
        let meta = SessionMeta {
            id: s.id.clone(),
            directory: s.directory.clone(),
            parent_id: s.parent_id.clone(),
            updated: s.updated,
            project_id: None,
            archived: s.archived,
        };
        if !self.matches_meta(&meta) {
            return false;
        }
        if let Some(min) = self.larger_than_bytes {
            if s.size_bytes() <= min {
                return false;
            }
        }
        if self.empty && s.size_bytes() != 0 {
            return false;
        }
        true
    }

    /// Same as `matches` minus the size check (which needs the
    /// per-table aggregates); used with `SessionMeta`.
    pub fn matches_meta(&self, m: &SessionMeta) -> bool {
        if let Some(cutoff) = self.cutoff_ms {
            if m.updated >= cutoff {
                return false;
            }
        }
        if self.subagents && m.parent_id.is_none() {
            return false;
        }
        if self.archived && !m.archived {
            return false;
        }
        if !self.paths.is_empty() {
            let dir = m.directory.trim_end_matches('/');
            if !self.paths.iter().any(|p| p.trim_end_matches('/') == dir) {
                return false;
            }
        }
        if !self.path_prefixes.is_empty() {
            let dir = m.directory.trim_end_matches('/');
            if !self.path_prefixes.iter().any(|p| {
                let p = p.trim_end_matches('/');
                dir == p || dir.starts_with(&format!("{p}/"))
            }) {
                return false;
            }
        }
        true
    }

    /// Stable JSON contract for the `filters` field in purge output.
    pub fn json(&self) -> PurgeFilterJson {
        PurgeFilterJson {
            older_than: self.older_than_raw.clone(),
            subagents: self.subagents,
            paths: self.paths.clone(),
            path_prefixes: self.path_prefixes.clone(),
            archived: self.archived,
            empty: self.empty,
            larger_than: self.larger_than_raw.clone(),
            keep_latest: self.keep_latest_raw.clone(),
            keep_latest_per_project: self.keep_latest_per_project_raw.clone(),
        }
    }
}

/// JSON contract for `session purge` / `strip-reasoning` filters.
#[derive(Serialize)]
pub struct PurgeFilterJson {
    pub older_than: Option<String>,
    pub subagents: bool,
    pub paths: Vec<String>,
    pub path_prefixes: Vec<String>,
    pub archived: bool,
    pub empty: bool,
    pub larger_than: Option<String>,
    pub keep_latest: Option<String>,
    pub keep_latest_per_project: Option<String>,
}

#[derive(Clone)]
pub struct ProjectRow {
    pub id: String,
    pub worktree: String,
    pub name: String,
    pub sessions: i64,
    pub sm_msgs: i64,
    pub sm_bytes: i64,
    pub events: i64,
    pub event_bytes: i64,
    pub inbox_bytes: i64,
    pub pending_bytes: i64,
    pub instr_bytes: i64,
    pub cost: f64,
    pub updated: i64,
}

pub fn project_total_bytes(p: &ProjectRow) -> i64 {
    p.sm_bytes + p.event_bytes + p.inbox_bytes + p.pending_bytes + p.instr_bytes
}

/// JSON shape of a project in `project list` / `project show`.
#[derive(Serialize)]
pub struct ProjectOut {
    pub id: String,
    pub worktree: String,
    pub name: String,
    pub sessions: i64,
    pub events: i64,
    pub session_messages: i64,
    pub size_bytes: i64,
    pub cost: f64,
    pub updated: String,
}

pub fn project_json(p: &ProjectRow) -> ProjectOut {
    ProjectOut {
        id: p.id.clone(),
        worktree: p.worktree.clone(),
        name: p.name.clone(),
        sessions: p.sessions,
        events: p.events,
        session_messages: p.sm_msgs,
        size_bytes: project_total_bytes(p),
        cost: round4(p.cost),
        updated: dt(p.updated),
    }
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
    /// Only projects with no sessions.
    pub empty: bool,
}

impl ProjectFilter {
    pub fn is_empty(&self) -> bool {
        self.older_than_raw.is_none() && self.paths.is_empty() && !self.empty
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
        if self.empty && p.sessions != 0 {
            return false;
        }
        true
    }

    /// Stable JSON contract for the `filters` field in purge output.
    pub fn json(&self) -> ProjectFilterJson {
        ProjectFilterJson {
            older_than: self.older_than_raw.clone(),
            paths: self.paths.clone(),
            empty: self.empty,
        }
    }
}

/// JSON contract for `project purge` filters.
#[derive(Serialize)]
pub struct ProjectFilterJson {
    pub older_than: Option<String>,
    pub paths: Vec<String>,
    pub empty: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::load_projects;
    use crate::testdb;

    #[test]
    fn session_output_contract() {
        let s = SessionRow {
            id: "ses_1".into(),
            title: "t".into(),
            directory: "/a".into(),
            parent_id: None,
            updated: 1136214245000,
            archived: true,
            events: 3,
            event_bytes: 30,
            sm_msgs: 4,
            sm_bytes: 40,
            inbox_msgs: 1,
            inbox_bytes: 10,
            pending_msgs: 2,
            pending_bytes: 20,
            instr_bytes: 5,
            cost: 0.004594,
        };
        let v = serde_json::to_value(session_json(&s)).unwrap();
        let expected = serde_json::json!({
            "id": "ses_1", "title": "t", "directory": "/a", "parent_id": null,
            "updated": "2006-01-02T15:04:05Z", "archived": true, "events": 3,
            "session_messages": 4, "inbox": 1, "pending": 2,
            "size_bytes": 105, "cost": 0.0046
        });
        assert_eq!(v, expected, "session JSON contract changed");
    }

    #[test]
    fn project_output_contract() {
        let p = ProjectRow {
            id: "p1".into(),
            worktree: "/w".into(),
            name: "n".into(),
            sessions: 1,
            sm_msgs: 4,
            sm_bytes: 40,
            events: 3,
            event_bytes: 30,
            inbox_bytes: 10,
            pending_bytes: 20,
            instr_bytes: 5,
            cost: 0.004594,
            updated: 1136214245000,
        };
        let v = serde_json::to_value(project_json(&p)).unwrap();
        let expected = serde_json::json!({
            "id": "p1", "worktree": "/w", "name": "n", "sessions": 1,
            "events": 3, "session_messages": 4, "size_bytes": 105,
            "cost": 0.0046, "updated": "2006-01-02T15:04:05Z"
        });
        assert_eq!(v, expected, "project JSON contract changed");
    }

    fn row(dir: &str) -> SessionRow {
        SessionRow {
            id: String::new(),
            title: String::new(),
            directory: dir.to_string(),
            parent_id: None,
            updated: 0,
            archived: false,
            events: 0,
            event_bytes: 0,
            sm_msgs: 0,
            sm_bytes: 0,
            inbox_msgs: 0,
            inbox_bytes: 0,
            pending_msgs: 0,
            pending_bytes: 0,
            instr_bytes: 0,
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
    fn larger_than_is_strict_on_own_size() {
        let f = PurgeFilter {
            larger_than_raw: Some("10".to_string()),
            larger_than_bytes: Some(10),
            ..Default::default()
        };
        let mut small = row("/a");
        small.sm_bytes = 10;
        let mut big = row("/a");
        big.sm_bytes = 11;
        assert!(!filter(&f, &small));
        assert!(filter(&f, &big));
    }

    #[test]
    fn empty_filter_matches_all() {
        let f = PurgeFilter {
            older_than_raw: None,
            cutoff_ms: None,
            subagents: false,
            paths: Vec::new(),
            ..Default::default()
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
