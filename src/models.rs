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

/// Filter sessions by exact directory match (trailing slash tolerant).
pub fn filter_sessions<'a>(sessions: &'a [SessionRow], dir: Option<&str>) -> Vec<&'a SessionRow> {
    match dir {
        None => sessions.iter().collect(),
        Some(dir) => {
            let dir = dir.trim_end_matches('/');
            sessions.iter().filter(|s| s.directory == dir).collect()
        }
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

    #[test]
    fn exact_match_only() {
        let sessions = vec![
            row("/home/okumura/work/misc"),
            row("/home/okumura/work/misc/opencode-dbtool"),
            row("/home/okumura/work/misc/opencode-dbtool/sub"),
        ];
        let hit: Vec<_> = filter_sessions(&sessions, Some("/home/okumura/work/misc"))
            .into_iter()
            .map(|s| s.directory.clone())
            .collect();
        assert_eq!(hit, vec!["/home/okumura/work/misc"]);
    }

    #[test]
    fn trailing_slash_stripped() {
        let sessions = vec![row("/a/b")];
        let hit = filter_sessions(&sessions, Some("/a/b/"));
        assert_eq!(hit.len(), 1);
    }

    #[test]
    fn empty_dir_matches_nothing() {
        let sessions = vec![row("/a/b")];
        let hit = filter_sessions(&sessions, Some(""));
        assert_eq!(hit.len(), 0);
    }

    #[test]
    fn none_matches_all() {
        let sessions = vec![row("/a"), row("/b")];
        let hit = filter_sessions(&sessions, None);
        assert_eq!(hit.len(), 2);
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