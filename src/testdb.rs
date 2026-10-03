//! Shared in-memory SQLite fixture for command tests (V2-only).

use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Fresh temporary directory for tests that need real files.
pub fn temp_data_dir(stem: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "opencode-dbtool-test-{}-{}",
        std::process::id(),
        stem
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn create() -> Connection {
    let con = Connection::open_in_memory().unwrap();
    schema(&con);
    con
}

/// Same schema on a real file (for tests that need file operations,
/// such as VACUUM and backups).
pub fn create_at(path: &Path) -> Connection {
    let con = Connection::open(path).unwrap();
    schema(&con);
    con
}

fn schema(con: &Connection) {
    con.execute_batch(
        "CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT, name TEXT);
         CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT, title TEXT, parent_id TEXT, project_id TEXT REFERENCES project(id) ON DELETE CASCADE, workspace_id TEXT, fork_session_id TEXT, fork_boundary TEXT, time_updated INTEGER, cost REAL, time_archived INTEGER, version TEXT NOT NULL DEFAULT '2.0.0');
         CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT REFERENCES session_v2(id) ON DELETE CASCADE, type TEXT, seq INTEGER NOT NULL DEFAULT 0, data TEXT, time_created INTEGER NOT NULL DEFAULT 0, time_updated INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE session_inbox (id TEXT PRIMARY KEY, session_id TEXT REFERENCES session_v2(id) ON DELETE CASCADE, type TEXT, payload TEXT NOT NULL, delivery TEXT NOT NULL, enqueued_seq INTEGER NOT NULL DEFAULT 0, time_created INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE session_pending (id TEXT PRIMARY KEY, session_id TEXT REFERENCES session_v2(id) ON DELETE CASCADE, type TEXT, data TEXT NOT NULL, delivery TEXT, admitted_seq INTEGER NOT NULL DEFAULT 0, time_created INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE instruction_entry (session_id TEXT REFERENCES session_v2(id) ON DELETE CASCADE, key TEXT NOT NULL, value TEXT, PRIMARY KEY (session_id, key));
         CREATE TABLE instruction_state (session_id TEXT PRIMARY KEY REFERENCES session_v2(id) ON DELETE CASCADE, epoch_start INTEGER NOT NULL DEFAULT 0, through_seq INTEGER NOT NULL DEFAULT 0, initial_values TEXT NOT NULL DEFAULT '{}', current_values TEXT NOT NULL DEFAULT '{}');
         CREATE TABLE instruction_blob (hash TEXT PRIMARY KEY, value TEXT);
         CREATE TABLE event (aggregate_id TEXT, type TEXT, data BLOB);
         CREATE TABLE event_sequence (aggregate_id TEXT PRIMARY KEY, seq INTEGER NOT NULL DEFAULT 0, owner_id TEXT);
         CREATE TABLE permission (project_id TEXT);
         CREATE TABLE worktree (project_id TEXT, directory TEXT);
         CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL, time_created INTEGER NOT NULL DEFAULT 0, time_updated INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE workspace (id TEXT PRIMARY KEY, provider TEXT NOT NULL, binding TEXT, created_at INTEGER NOT NULL DEFAULT 0, last_used_at INTEGER NOT NULL DEFAULT 0);",
    )
    .unwrap();
}

pub fn insert_session(con: &Connection, id: &str, dir: &str, parent: Option<&str>) {
    insert_session_at(con, id, dir, parent, 0);
}

pub fn insert_session_at(
    con: &Connection,
    id: &str,
    dir: &str,
    parent: Option<&str>,
    updated: i64,
) {
    con.execute(
        "INSERT INTO session_v2 (id, directory, title, parent_id, time_updated, cost) VALUES (?1, ?2, ?3, ?4, ?5, 0)",
        rusqlite::params![id, dir, id, parent, updated],
    )
    .unwrap();
}

/// Insert an event row (durable event log).
pub fn insert_event(con: &Connection, aggregate_id: &str, event_type: &str, data: &str) {
    con.execute(
        "INSERT INTO event (aggregate_id, type, data) VALUES (?1, ?2, ?3)",
        rusqlite::params![aggregate_id, event_type, data],
    )
    .unwrap();
}

/// Insert a session_message row with raw JSON `data`.
pub fn insert_session_message(
    con: &Connection,
    id: &str,
    session_id: &str,
    msg_type: &str,
    data: &str,
) {
    con.execute(
        "INSERT INTO session_message (id, session_id, type, data) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![id, session_id, msg_type, data],
    )
    .unwrap();
}

pub fn insert_inbox(con: &Connection, id: &str, session_id: &str, payload: &str) {
    con.execute(
        "INSERT INTO session_inbox (id, session_id, type, payload, delivery, enqueued_seq) VALUES (?1, ?2, 'prompt', ?3, 'steer', 0)",
        rusqlite::params![id, session_id, payload],
    )
    .unwrap();
}

pub fn insert_project(con: &Connection, id: &str, worktree: &str) {
    con.execute(
        "INSERT INTO project (id, worktree, name) VALUES (?1, ?2, ?3)",
        rusqlite::params![id, worktree, id],
    )
    .unwrap();
}

/// Insert a top-level session linked to a project (for project-level
/// queries such as purge `--older-than`).
pub fn insert_project_session(
    con: &Connection,
    id: &str,
    dir: &str,
    project_id: &str,
    updated: i64,
) {
    con.execute(
        "INSERT INTO session_v2 (id, directory, title, parent_id, project_id, time_updated, cost) VALUES (?1, ?2, ?3, NULL, ?4, ?5, 0)",
        rusqlite::params![id, dir, id, project_id, updated],
    )
    .unwrap();
}

pub fn session_count(con: &Connection) -> i64 {
    con.query_row("SELECT COUNT(*) FROM session_v2", [], |r| r.get(0))
        .unwrap()
}

pub fn project_count(con: &Connection) -> i64 {
    con.query_row("SELECT COUNT(*) FROM project", [], |r| r.get(0))
        .unwrap()
}
