//! End-to-end tests: drive the real binary against a database built
//! from the same schema subset the unit tests use. These verify the CLI
//! contract (exit codes, JSON shape) through the actual process boundary.

use rusqlite::Connection;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_opencode-dbtool"))
}

fn temp_dir(stem: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "opencode-dbtool-cli-{}-{}",
        std::process::id(),
        stem
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build a database with the schema subset the tool reads.
fn create_db(db_path: &Path) {
    let con = Connection::open(db_path).unwrap();
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
    con.execute(
        "INSERT INTO project (id, worktree, name) VALUES ('p1', '/work/a', 'a')",
        [],
    )
    .unwrap();
    con.execute(
        "INSERT INTO session (id, directory, title, parent_id, project_id, time_updated, cost) \
         VALUES ('ses_1', '/work/a', 'hello', NULL, 'p1', 1700000000000, 0.42)",
        [],
    )
    .unwrap();
    con.execute(
        "INSERT INTO message (session_id, data, time_created) \
         VALUES ('ses_1', '{\"type\":\"text\",\"text\":\"hi\"}', 1700000000000)",
        [],
    )
    .unwrap();
    con.execute(
        "INSERT INTO part (session_id, data, time_created) \
         VALUES ('ses_1', '{\"type\":\"text\",\"text\":\"hello\"}', 1700000000000)",
        [],
    )
    .unwrap();
    con.execute(
        "INSERT INTO part (session_id, data, time_created) \
         VALUES ('ses_1', '{\"type\":\"reasoning\",\"text\":\"think\"}', 1700000000000)",
        [],
    )
    .unwrap();
    con.execute(
        "INSERT INTO event (aggregate_id, type, data) \
         VALUES ('ses_1', 'session.next.reasoning.ended', '{\"text\":\"think\"}')",
        [],
    )
    .unwrap();
    con.execute(
        "INSERT INTO event (aggregate_id, type, data) \
         VALUES ('ses_1', 'session.next.text.ended', '{\"text\":\"hi\"}')",
        [],
    )
    .unwrap();
    con.execute(
        "INSERT INTO session_message (id, session_id, type, data) \
         VALUES ('m1', 'ses_1', 'assistant', \
         '{\"type\":\"assistant\",\"content\":[{\"type\":\"reasoning\",\"text\":\"think\"},{\"type\":\"text\",\"text\":\"hi\"}]}')",
        [],
    )
    .unwrap();
}

fn run(args: &[&str], data_dir: &Path) -> Output {
    Command::new(bin())
        .args(args)
        .env("OPENCODE_DATA_DIR", data_dir)
        .output()
        .expect("failed to run binary")
}

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON: {e}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[test]
fn stats_reports_tables_and_part_types() {
    let dir = temp_dir("stats");
    let db = dir.join("opencode.db");
    create_db(&db);

    let out = run(&["stats"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert!(v["opencode_running"].is_boolean() || v["opencode_running"].is_null());
    assert_eq!(v["tables"]["session"], 1);
    assert_eq!(v["tables"]["part"], 2);
    assert_eq!(v["part_types"]["reasoning"]["count"], 1);
    assert_eq!(v["part_types"]["text"]["count"], 1);
    assert!(v["storage"]["session_diff_bytes"].is_number());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn doctor_ok_and_list_commands() {
    let dir = temp_dir("doctor");
    let db = dir.join("opencode.db");
    create_db(&db);

    let doc = run(&["doctor"], &dir);
    assert!(
        doc.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&doc.stderr)
    );
    let v = stdout_json(&doc);
    assert_eq!(v["integrity_check"], "ok");
    assert_eq!(v["ok"], true);

    let list = run(&["session", "list"], &dir);
    assert!(list.status.success());
    let arr = stdout_json(&list);
    let arr = arr.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "ses_1");
    assert_eq!(arr[0]["parts"], 2);

    let show = run(&["session", "show", "ses_1"], &dir);
    assert!(show.status.success());
    let s = stdout_json(&show);
    assert_eq!(s["id"], "ses_1");

    let projects = run(&["project", "list"], &dir);
    assert!(projects.status.success());
    let pv = stdout_json(&projects);
    let p = pv.as_array().unwrap();
    assert_eq!(p.len(), 1);
    assert_eq!(p[0]["sessions"], 1);

    let pshow = run(&["project", "show", "p1"], &dir);
    assert!(pshow.status.success());
    let pv = stdout_json(&pshow);
    assert_eq!(pv["session_list"].as_array().unwrap().len(), 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn dry_run_delete_previews_without_changing() {
    let dir = temp_dir("dryrun");
    let db = dir.join("opencode.db");
    create_db(&db);

    let out = run(&["session", "delete", "ses_1", "--dry-run"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(v["deleted"], false);
    assert!(v["total_rows"].as_i64().unwrap() > 0);
    // Session still present afterwards.
    let list = run(&["session", "list"], &dir);
    assert_eq!(stdout_json(&list).as_array().unwrap().len(), 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn purge_without_filters_is_exit_2() {
    let dir = temp_dir("purge-no-filter");
    let db = dir.join("opencode.db");
    create_db(&db);

    let out = run(&["session", "purge"], &dir);
    // Without opencode running this is a usage error (2); while opencode
    // runs the guard refuses with exit 1 first. Both are failures.
    let code = out.status.code();
    assert!(
        code == Some(1) || code == Some(2),
        "unexpected exit code: {code:?} stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn strip_reasoning_works_with_filters() {
    let dir = temp_dir("strip");
    let db = dir.join("opencode.db");
    create_db(&db);

    let out = run(
        &[
            "session",
            "strip-reasoning",
            "--path",
            "/work/a",
            "--dry-run",
        ],
        &dir,
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(v["action"], "strip-reasoning");
    assert_eq!(v["total_reasoning_parts"], 1);
    assert_eq!(v["total_reasoning_events"], 1);
    assert_eq!(v["total_messages_rewritten"], 1);
    assert_eq!(v["stripped"], false);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn destructive_commands_require_yes() {
    let dir = temp_dir("confirm");
    let db = dir.join("opencode.db");
    create_db(&db);

    let out = run(&["session", "delete", "ses_1"], &dir);
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--dry-run"), "stderr: {stderr}");
    assert!(stderr.contains("--yes"), "stderr: {stderr}");

    // --yes lets the command proceed past the confirmation (the running
    // guard may then refuse with exit 1, which is also a valid outcome).
    let out = run(&["session", "delete", "ses_1", "--yes"], &dir);
    let code = out.status.code();
    assert!(
        code == Some(0) || code == Some(1),
        "unexpected exit code: {code:?} stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn unknown_command_is_exit_2() {
    let dir = temp_dir("unknown");
    let db = dir.join("opencode.db");
    create_db(&db);

    let out = run(&["frobnicate"], &dir);
    assert_eq!(out.status.code(), Some(2));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn missing_db_is_exit_2() {
    let dir = temp_dir("missing-db");

    let out = run(&["stats"], &dir);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("DB not found"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn old_schema_is_clear_exit_3() {
    let dir = temp_dir("old-schema");
    let con = Connection::open(dir.join("opencode.db")).unwrap();
    con.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, time_updated INTEGER);
         CREATE TABLE project (id TEXT PRIMARY KEY);",
    )
    .unwrap();
    drop(con);

    let out = run(&["stats"], &dir);
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("schema is not supported"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("1.18.0"), "stderr: {stderr}");

    std::fs::remove_dir_all(&dir).unwrap();
}
