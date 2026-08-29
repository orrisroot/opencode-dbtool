//! Shared in-memory SQLite fixture for command tests.
//!
//! Mirrors the opencode schema subset the tool reads and writes.

use rusqlite::Connection;

pub fn create() -> Connection {
    let con = Connection::open_in_memory().unwrap();
    con.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, parent_id TEXT, project_id TEXT, time_updated INTEGER, cost REAL);
         CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT, name TEXT);
         CREATE TABLE message (session_id TEXT, data BLOB);
         CREATE TABLE part (session_id TEXT, data BLOB);
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
    con
}

pub fn insert_session(con: &Connection, id: &str, dir: &str, parent: Option<&str>) {
    con.execute(
        "INSERT INTO session (id, directory, title, parent_id, time_updated, cost) VALUES (?1, ?2, ?3, ?4, 0, 0)",
        rusqlite::params![id, dir, id, parent.unwrap_or("")],
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

pub fn session_count(con: &Connection) -> i64 {
    con.query_row("SELECT COUNT(*) FROM session", [], |r| r.get(0))
        .unwrap()
}

pub fn project_count(con: &Connection) -> i64 {
    con.query_row("SELECT COUNT(*) FROM project", [], |r| r.get(0))
        .unwrap()
}
