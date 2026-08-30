//! Shared in-memory SQLite fixture for command tests.
//!
//! Mirrors the opencode schema subset the tool reads and writes.

use rusqlite::Connection;
use std::path::Path;

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
         CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, parent_id TEXT, project_id TEXT REFERENCES project(id) ON DELETE CASCADE, time_updated INTEGER, cost REAL);
         CREATE TABLE message (session_id TEXT, data BLOB);
         CREATE TABLE part (session_id TEXT, data TEXT);
         CREATE TABLE todo (session_id TEXT);
         CREATE TABLE event (aggregate_id TEXT, data BLOB);
         CREATE TABLE event_sequence (aggregate_id TEXT);
         CREATE TABLE session_share (session_id TEXT);
         CREATE TABLE session_input (session_id TEXT);
         CREATE TABLE session_message (session_id TEXT);
         CREATE TABLE session_context_epoch (session_id TEXT);
         CREATE TABLE permission (project_id TEXT);
         CREATE TABLE project_directory (project_id TEXT);
         CREATE TABLE workspace (project_id TEXT);",
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
    con.execute(
        "INSERT INTO part (session_id, data) VALUES (?1, ?2)",
        rusqlite::params![session_id, data],
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
