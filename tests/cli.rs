//! End-to-end tests: drive the real binary against a V2 database.
//! These verify the CLI contract (exit codes, JSON shape) through the
//! actual process boundary.

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

/// Build a V2 database with the schema subset the tool reads.
fn create_db(db_path: &Path) {
    let con = Connection::open(db_path).unwrap();
    con.execute_batch(
        "CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT, name TEXT);
         CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT, title TEXT, parent_id TEXT, project_id TEXT, workspace_id TEXT, fork_session_id TEXT, time_updated INTEGER, cost REAL, time_archived INTEGER, version TEXT NOT NULL DEFAULT '2.0.0');
         CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER NOT NULL DEFAULT 0, data TEXT, time_created INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE session_inbox (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, payload TEXT NOT NULL, delivery TEXT NOT NULL, enqueued_seq INTEGER NOT NULL DEFAULT 0, time_created INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE session_pending (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, data TEXT NOT NULL, delivery TEXT, admitted_seq INTEGER NOT NULL DEFAULT 0, time_created INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE instruction_entry (session_id TEXT, key TEXT, value TEXT, PRIMARY KEY (session_id, key));
         CREATE TABLE instruction_state (session_id TEXT PRIMARY KEY, epoch_start INTEGER NOT NULL DEFAULT 0, through_seq INTEGER NOT NULL DEFAULT 0, initial_values TEXT NOT NULL DEFAULT '{}', current_values TEXT NOT NULL DEFAULT '{}');
         CREATE TABLE instruction_blob (hash TEXT PRIMARY KEY, value TEXT);
         CREATE TABLE event (aggregate_id TEXT, type TEXT, data BLOB);
         CREATE TABLE event_sequence (aggregate_id TEXT PRIMARY KEY, seq INTEGER NOT NULL DEFAULT 0, owner_id TEXT);
         CREATE TABLE permission (project_id TEXT);
         CREATE TABLE worktree (project_id TEXT, directory TEXT);
         CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL, time_created INTEGER NOT NULL DEFAULT 0, time_updated INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE workspace (id TEXT PRIMARY KEY, provider TEXT NOT NULL);",
    )
    .unwrap();
    con.execute(
        "INSERT INTO project (id, worktree, name) VALUES ('p1', '/work/a', 'a')",
        [],
    )
    .unwrap();
    con.execute(
        "INSERT INTO session_v2 (id, directory, title, parent_id, project_id, time_updated, cost) \
         VALUES ('ses_1', '/work/a', 'hello', NULL, 'p1', 1700000000000, 0.42)",
        [],
    )
    .unwrap();
    con.execute(
        "INSERT INTO session_message (id, session_id, type, data, time_created) \
         VALUES ('m1', 'ses_1', 'assistant', \
         '{\"type\":\"assistant\",\"content\":[{\"type\":\"reasoning\",\"text\":\"think\"},{\"type\":\"text\",\"text\":\"hi\"}]}', 1700000000000)",
        [],
    )
    .unwrap();
    con.execute(
        "INSERT INTO session_inbox (id, session_id, type, payload, delivery, enqueued_seq) \
         VALUES ('i1', 'ses_1', 'prompt', 'payload-bytes', 'steer', 0)",
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
}

/// Run with `OPENCODE_DB` set explicitly; `run` below clears it so tests
/// stay hermetic against a leaked parent-environment value.
fn run_db_env(args: &[&str], data_dir: &Path, db: Option<&str>) -> Output {
    let mut cmd = Command::new(bin());
    cmd.args(args).env("OPENCODE_DATA_DIR", data_dir);
    match db {
        Some(value) => {
            cmd.env("OPENCODE_DB", value);
        }
        None => {
            cmd.env_remove("OPENCODE_DB");
        }
    }
    cmd.output().expect("failed to run binary")
}

fn run(args: &[&str], data_dir: &Path) -> Output {
    run_db_env(args, data_dir, None)
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
fn stats_reports_tables_and_message_types() {
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
    assert_eq!(v["tables"]["session_v2"], 1);
    assert_eq!(v["tables"]["session_message"], 1);
    assert_eq!(v["message_types"]["assistant"]["count"], 1);
    assert!(v["storage"]["snapshot_bytes"].is_number());

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
    assert_eq!(arr[0]["session_messages"], 1);

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
fn self_update_requires_confirmation() {
    let dir = temp_dir("self-update");

    // Bare `self-update` must refuse before touching the network (exit 2)
    // and point at the confirmation flags.
    let out = run(&["self-update"], &dir);
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--dry-run"), "stderr: {stderr}");
    assert!(stderr.contains("--yes"), "stderr: {stderr}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn self_update_rejects_unknown_options() {
    let dir = temp_dir("self-update-opts");

    let out = run(&["self-update", "--tag", "v0.0.1", "--dry-run"], &dir);
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unknown option"),
        "stderr: {}",
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
fn channel_database_is_used_when_default_is_missing() {
    let dir = temp_dir("channel-db");
    create_db(&dir.join("opencode-prod.db"));

    let out = run(&["stats"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(
        v["db"],
        dir.join("opencode-prod.db").to_string_lossy().as_ref()
    );
    assert_eq!(v["tables"]["session_v2"], 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn open_code_db_env_overrides_the_database_path() {
    let dir = temp_dir("db-env");
    create_db(&dir.join("opencode.db"));
    let custom = dir.join("custom.db");
    create_db(&custom);

    // An absolute path wins over the default and channel fallbacks.
    let out = run_db_env(&["stats"], &dir, Some(custom.to_string_lossy().as_ref()));
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(v["db"], custom.to_string_lossy().as_ref());
    assert_eq!(v["tables"]["session_v2"], 1);

    // A relative path resolves against the data dir.
    let out = run_db_env(&["stats"], &dir, Some("custom.db"));
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(v["db"], dir.join("custom.db").to_string_lossy().as_ref());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn open_code_db_env_missing_path_is_exit_2() {
    let dir = temp_dir("db-env-missing");
    create_db(&dir.join("opencode.db"));

    let out = run_db_env(&["stats"], &dir, Some("does-not-exist.db"));
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("DB not found"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn multiple_database_candidates_are_ambiguous_exit_2() {
    let dir = temp_dir("multi-db");
    create_db(&dir.join("opencode-prod.db"));
    create_db(&dir.join("opencode-nightly.db"));

    let out = run(&["stats"], &dir);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("OPENCODE_DB"), "stderr: {stderr}");
    assert!(stderr.contains("opencode-prod.db"), "stderr: {stderr}");
    assert!(stderr.contains("opencode-nightly.db"), "stderr: {stderr}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn v1_schema_is_rejected_exit_3() {
    let dir = temp_dir("v1-schema");
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
    assert!(stderr.contains("2.x"), "stderr: {stderr}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn removed_fs_commands_are_unknown() {
    // V1 `storage/session_diff` and `tool-output` are gone with V1
    // support: the commands no longer exist.
    let dir = temp_dir("removed-fs");
    create_db(&dir.join("opencode.db"));

    for args in [vec!["fs", "clean-orphans"], vec!["fs", "clean-tool-output"]] {
        let out = run(&args, &dir);
        assert_eq!(out.status.code(), Some(2), "args: {args:?}");
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn kv_commands_round_trip() {
    let dir = temp_dir("kv");
    let db = dir.join("opencode.db");
    create_db(&db);
    {
        let con = Connection::open(&db).unwrap();
        con.execute(
            "INSERT INTO kv (key, value, time_created, time_updated) VALUES ('cache:a', '12345', 0, 1700000000000)",
            [],
        )
        .unwrap();
    }

    let out = run(&["kv", "list"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let arr = stdout_json(&out);
    assert_eq!(arr.as_array().unwrap().len(), 1);
    assert_eq!(arr[0]["key"], "cache:a");
    assert_eq!(arr[0]["bytes"], 5);

    let out = run(&["kv", "show", "cache:a"], &dir);
    assert!(out.status.success());
    assert_eq!(stdout_json(&out)["value"], "12345");

    let out = run(&["kv", "show", "missing"], &dir);
    assert_eq!(out.status.code(), Some(2));

    let out = run(&["kv", "delete", "cache:a", "--dry-run"], &dir);
    assert!(out.status.success());
    assert_eq!(stdout_json(&out)["deleted"], false);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn backup_dry_run_reports_planned_path() {
    let dir = temp_dir("backup");
    create_db(&dir.join("opencode.db"));

    let out = run(&["backup", "--dry-run"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(v["dry_run"], true);
    assert!(v["backup"]["path"]
        .as_str()
        .unwrap()
        .contains("opencode.db.backup-"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn clean_shell_dry_run_lists_files() {
    let dir = temp_dir("shell");
    create_db(&dir.join("opencode.db"));
    std::fs::create_dir_all(dir.join("shell/p1")).unwrap();
    std::fs::write(dir.join("shell/p1/sh_x.out"), vec![0u8; 6]).unwrap();

    let out = run(&["fs", "clean-shell", "--dry-run"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(v["total_files"], 1);
    assert_eq!(v["total_bytes"], 6);
    assert!(
        dir.join("shell/p1/sh_x.out").exists(),
        "dry-run changes nothing"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn show_messages_flag_attaches_previews() {
    let dir = temp_dir("show-msg");
    create_db(&dir.join("opencode.db"));

    let plain = run(&["session", "show", "ses_1"], &dir);
    assert!(plain.status.success());
    assert!(stdout_json(&plain).get("messages").is_none());

    let out = run(&["session", "show", "ses_1", "--messages"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(v["total_messages"], 1);
    assert_eq!(v["messages"].as_array().unwrap().len(), 1);
    assert_eq!(v["messages"][0]["type"], "assistant");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn new_purge_filters_work_end_to_end() {
    let dir = temp_dir("new-filters");
    create_db(&dir.join("opencode.db"));

    // --path-prefix matches the session directory subtree.
    let out = run(
        &["session", "purge", "--path-prefix", "/work", "--dry-run"],
        &dir,
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout_json(&out)["sessions"].as_array().unwrap().len(), 1);

    // --empty selects nothing here (the session has content).
    let out = run(&["session", "purge", "--empty", "--dry-run"], &dir);
    assert!(out.status.success());
    assert_eq!(stdout_json(&out)["sessions"].as_array().unwrap().len(), 0);

    // --keep-latest and --keep-latest-per-project conflict.
    let out = run(
        &[
            "session",
            "purge",
            "--keep-latest",
            "1",
            "--keep-latest-per-project",
            "1",
        ],
        &dir,
    );
    assert_eq!(out.status.code(), Some(2));

    std::fs::remove_dir_all(&dir).unwrap();
}
