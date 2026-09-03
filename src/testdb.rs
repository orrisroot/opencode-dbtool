//! Shared in-memory SQLite fixture for command tests.
//!
//! Mirrors the opencode schema subset the tool reads and writes.

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
         CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, parent_id TEXT, project_id TEXT REFERENCES project(id) ON DELETE CASCADE, workspace_id TEXT, time_updated INTEGER, cost REAL);
         CREATE TABLE message (id TEXT, session_id TEXT, data BLOB, time_created INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE part (session_id TEXT, message_id TEXT, data TEXT, time_created INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE todo (session_id TEXT);
         CREATE TABLE event (aggregate_id TEXT, type TEXT, data BLOB);
         CREATE TABLE event_sequence (aggregate_id TEXT);
         CREATE TABLE session_share (session_id TEXT);
         CREATE TABLE session_input (session_id TEXT);
         CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER NOT NULL DEFAULT 0, data TEXT);
         CREATE TABLE session_context_epoch (session_id TEXT);
         CREATE TABLE permission (project_id TEXT);
         CREATE TABLE project_directory (project_id TEXT);
         CREATE TABLE workspace (id TEXT PRIMARY KEY, project_id TEXT);",
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
        "INSERT INTO session (id, directory, title, parent_id, time_updated, cost) VALUES (?1, ?2, ?3, ?4, ?5, 0)",
        rusqlite::params![id, dir, id, parent, updated],
    )
    .unwrap();
}

/// Insert a part with a raw JSON `data` payload (e.g. `{"type":"reasoning"}`).
pub fn insert_part(con: &Connection, session_id: &str, data: &str) {
    insert_part_at(con, session_id, data, 0);
}

/// Insert a part with an explicit creation timestamp.
pub fn insert_part_at(con: &Connection, session_id: &str, data: &str, time_created: i64) {
    con.execute(
        "INSERT INTO part (session_id, data, time_created) VALUES (?1, ?2, ?3)",
        rusqlite::params![session_id, data, time_created],
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

pub fn part_count(con: &Connection) -> i64 {
    con.query_row("SELECT COUNT(*) FROM part", [], |r| r.get(0))
        .unwrap()
}

pub fn reasoning_part_count(con: &Connection) -> i64 {
    con.query_row(
        "SELECT COUNT(*) FROM part WHERE json_extract(data, '$.type') = 'reasoning'",
        [],
        |r| r.get(0),
    )
    .unwrap()
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
        "INSERT INTO session (id, directory, title, parent_id, project_id, time_updated, cost) VALUES (?1, ?2, ?3, NULL, ?4, ?5, 0)",
        rusqlite::params![id, dir, id, project_id, updated],
    )
    .unwrap();
}

pub fn session_count(con: &Connection) -> i64 {
    con.query_row("SELECT COUNT(*) FROM session", [], |r| r.get(0))
        .unwrap()
}

pub fn project_count(con: &Connection) -> i64 {
    con.query_row("SELECT COUNT(*) FROM project", [], |r| r.get(0))
        .unwrap()
}
