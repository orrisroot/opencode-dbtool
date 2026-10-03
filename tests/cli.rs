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
/// stay hermetic against a leaked parent-environment value. The state dir
/// is isolated too, so a real running opencode service is never contacted.
fn run_db_env(args: &[&str], data_dir: &Path, db: Option<&str>) -> Output {
    let mut cmd = Command::new(bin());
    cmd.args(args)
        .env("OPENCODE_DATA_DIR", data_dir)
        .env("XDG_STATE_HOME", data_dir.join("state"))
        .env("XDG_CONFIG_HOME", data_dir.join("config"));
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
    assert!(v["reclaimable_bytes"].is_number());
    assert!(v["table_bytes"]["session_message"].is_number());

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
        String::from_utf8_lossy(&out.stderr).contains("unexpected argument"),
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

#[test]
fn session_show_resolves_unique_prefix_and_rejects_ambiguous() {
    let dir = temp_dir("prefix");
    create_db(&dir.join("opencode.db"));
    {
        let con = Connection::open(dir.join("opencode.db")).unwrap();
        con.execute(
            "INSERT INTO session_v2 (id, directory, title, time_updated, cost) \
             VALUES ('ses_1x', '/work/a', 'other', 0, 0)",
            [],
        )
        .unwrap();
    }

    // Exact id still wins over the prefix match.
    let out = run(&["session", "show", "ses_1"], &dir);
    assert!(out.status.success());
    assert_eq!(stdout_json(&out)["id"], "ses_1");

    // An ambiguous prefix is a usage error listing the candidates.
    let out = run(&["session", "show", "ses_"], &dir);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("ambiguous"), "stderr: {stderr}");
    assert!(stderr.contains("ses_1"), "stderr: {stderr}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn project_show_accepts_a_worktree_path() {
    let dir = temp_dir("project-path");
    create_db(&dir.join("opencode.db"));

    let out = run(&["project", "show", "/work/a"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout_json(&out)["id"], "p1");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn session_list_search_filters_by_title_and_directory() {
    let dir = temp_dir("search");
    create_db(&dir.join("opencode.db"));

    let out = run(&["session", "list", "--search", "HELLO"], &dir);
    assert!(out.status.success());
    assert_eq!(stdout_json(&out).as_array().unwrap().len(), 1);

    let out = run(&["session", "list", "--search", "/work"], &dir);
    assert!(out.status.success());
    assert_eq!(stdout_json(&out).as_array().unwrap().len(), 1);

    let out = run(&["session", "list", "--search", "nomatch"], &dir);
    assert!(out.status.success());
    assert_eq!(stdout_json(&out).as_array().unwrap().len(), 0);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn format_table_prints_a_human_table() {
    let dir = temp_dir("table");
    create_db(&dir.join("opencode.db"));

    let out = run(&["session", "list", "--format", "table"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("ses_1"), "stdout: {text}");
    assert!(text.contains("size_bytes"), "stdout: {text}");
    assert!(text.contains("title"), "stdout: {text}");
    assert!(
        serde_json::from_slice::<Value>(&out.stdout).is_err(),
        "not JSON"
    );

    // Explicit json wins over the terminal default.
    let out = run(&["session", "list", "--format", "json"], &dir);
    assert!(out.status.success());
    assert_eq!(stdout_json(&out).as_array().unwrap().len(), 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cleanup_dry_run_reports_the_plan() {
    let dir = temp_dir("cleanup");
    create_db(&dir.join("opencode.db"));

    // Bare cleanup deletes no sessions: only orphans and old files.
    let out = run(&["cleanup", "--dry-run"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(v["action"], "cleanup");
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["cleaned"], false);
    assert!(v["purge"].is_null(), "no session filters -> no purge");
    assert!(v["backup"].is_null(), "no purge -> no backup");
    assert_eq!(v["fs_older_than"], "7d");
    assert_eq!(v["vacuum"]["dry_run"], true);
    assert!(v["blob_orphans"].is_object());
    assert!(v["snapshots"].is_object());

    // With session filters the plan includes the purge and a backup.
    let out = run(&["cleanup", "--older-than", "30d", "--dry-run"], &dir);
    assert!(out.status.success());
    let v = stdout_json(&out);
    assert_eq!(v["purge"]["sessions"].as_array().unwrap().len(), 1);
    assert!(v["backup"]["backup"]["path"].is_string());

    // Real runs still require --yes outside a terminal.
    let out = run(&["cleanup"], &dir);
    assert_eq!(out.status.code(), Some(2));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cleanup_with_filters_applies_changes() {
    let dir = temp_dir("cleanup-apply");
    create_db(&dir.join("opencode.db"));
    {
        let con = Connection::open(dir.join("opencode.db")).unwrap();
        con.execute(
            "INSERT INTO instruction_blob (hash, value) VALUES ('orphan', 'x')",
            [],
        )
        .unwrap();
    }

    let out = run(
        &["cleanup", "--older-than", "30d", "--no-backup", "--yes"],
        &dir,
    );
    // While this test harness itself runs under opencode, the guard may
    // refuse with exit 1; both outcomes are valid (like other real-run
    // tests in this file).
    let code = out.status.code();
    assert!(
        code == Some(0) || code == Some(1),
        "unexpected exit code: {code:?} stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    if code == Some(1) {
        std::fs::remove_dir_all(&dir).unwrap();
        return;
    }
    let v = stdout_json(&out);
    assert_eq!(v["cleaned"], true);
    assert_eq!(v["purge"]["deleted"], true);

    let con = Connection::open(dir.join("opencode.db")).unwrap();
    let sessions: i64 = con
        .query_row("SELECT COUNT(*) FROM session_v2", [], |r| r.get(0))
        .unwrap();
    let blobs: i64 = con
        .query_row("SELECT COUNT(*) FROM instruction_blob", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sessions, 0);
    assert_eq!(blobs, 0);
    drop(con);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn subcommand_help_lists_flags() {
    let dir = temp_dir("help");
    create_db(&dir.join("opencode.db"));

    let out = run(&["session", "purge", "--help"], &dir);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("--older-than"), "stdout: {text}");
    assert!(text.contains("--keep-latest-per-project"), "stdout: {text}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn completions_emit_a_script() {
    let dir = temp_dir("completions");

    let out = run(&["completions", "bash"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("opencode-dbtool"), "stdout: {text}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn backup_runs_while_opencode_is_running() {
    let dir = temp_dir("backup-online");
    create_db(&dir.join("opencode.db"));

    // The online backup API does not need exclusive access, so the
    // running-instance guard no longer blocks `backup`.
    let out = run(&["backup", "--yes"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(v["backup"]["integrity"], "ok");
    let path = v["backup"]["path"].as_str().unwrap();
    assert!(Path::new(path).exists(), "backup file missing: {path}");

    let con = Connection::open(path).unwrap();
    let n: i64 = con
        .query_row("SELECT COUNT(*) FROM session_v2", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "backup contains the session");
    drop(con);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn db_checkpoint_reports_wal_state() {
    let dir = temp_dir("checkpoint");
    create_db(&dir.join("opencode.db"));

    // --dry-run reports the plan without running the pragma.
    let out = run(&["db", "checkpoint", "--truncate", "--dry-run"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["wal_bytes_after"], v["wal_bytes_before"]);

    let out = run(&["db", "checkpoint", "--truncate"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(v["dry_run"], false);
    assert_eq!(v["mode"], "truncate");
    assert!(v["busy"].is_number());
    assert!(v["wal_bytes_after"].is_number());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn fs_clean_shell_older_than_runs_while_opencode_runs() {
    let dir = temp_dir("shell-online");
    create_db(&dir.join("opencode.db"));
    std::fs::create_dir_all(dir.join("shell/p1")).unwrap();
    let old = dir.join("shell/p1/sh_old.out");
    std::fs::write(&old, vec![0u8; 4]).unwrap();
    // Backdate by two days so --older-than 1d selects it.
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86_400);
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(past)
        .unwrap();

    let out = run(&["fs", "clean-shell", "--older-than", "1d", "--yes"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !old.exists(),
        "old shell output removed while opencode runs"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

/// A running service whose DB is the target gets session deletes routed
/// through its HTTP API (Linux: the fd check proves it owns the DB).
#[test]
#[cfg(target_os = "linux")]
fn session_delete_routes_through_the_running_service() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    let dir = temp_dir("api-delete");
    let db = dir.join("opencode.db");
    create_db(&db);
    {
        let con = Connection::open(&db).unwrap();
        con.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        con.execute(
            "INSERT INTO session_v2 (id, directory, title, parent_id, time_updated, cost) \
             VALUES ('ses_api_parent', '/work/a', 'parent', NULL, 0, 0)",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO session_v2 (id, directory, title, parent_id, time_updated, cost) \
             VALUES ('ses_api_child', '/work/a', 'child', 'ses_api_parent', 0, 0)",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO event (aggregate_id, type, data) \
             VALUES ('ses_api_child', 'session.next.text.ended', '{}')",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO event_sequence (aggregate_id, seq) VALUES ('ses_api_child', 1)",
            [],
        )
        .unwrap();
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let order_thread = Arc::clone(&order);
    let db_thread = db.clone();
    let server = std::thread::spawn(move || {
        // Hold the database open for the whole test: the tool only trusts
        // a service that has the target DB among its open files.
        let con = Connection::open(&db_thread).unwrap();
        con.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
        con.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        // Serve up to two requests, but never block forever: a regression
        // that sends fewer requests must fail the test rather than hang it.
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut served = 0;
        while served < 2 && std::time::Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                Err(e) => panic!("mock server accept failed: {e}"),
            };
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                let n = stream.read(&mut tmp).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&buf).to_string();
            let first = request.lines().next().unwrap_or("");
            let id = first
                .split_whitespace()
                .nth(1)
                .unwrap()
                .rsplit('/')
                .next()
                .unwrap()
                .to_string();
            order_thread.lock().unwrap().push(id.clone());
            con.execute(
                "DELETE FROM session_v2 WHERE id = ?1",
                rusqlite::params![id],
            )
            .unwrap();
            let body = "{}";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            served += 1;
        }
    });

    let state = dir.join("state/opencode");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("service.json"),
        serde_json::json!({
            "pid": std::process::id(),
            "url": format!("http://{addr}"),
            "version": "2.0.22",
            "password": "pw",
        })
        .to_string(),
    )
    .unwrap();

    let out = run(&["session", "delete", "ses_api_parent", "--yes"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(v["deleted"], true);
    assert!(
        v["note"]
            .as_str()
            .unwrap()
            .contains("running opencode server"),
        "note: {}",
        v["note"]
    );

    server.join().unwrap();
    let deleted = order.lock().unwrap().clone();
    assert_eq!(
        deleted,
        vec!["ses_api_child", "ses_api_parent"],
        "children must be deleted before their parent"
    );

    let con = Connection::open(&db).unwrap();
    let left: i64 = con
        .query_row(
            "SELECT COUNT(*) FROM session_v2 WHERE id LIKE 'ses_api_%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 0, "API deletes removed the rows");
    // The mock server only removes session rows; the tool cleans up the
    // event/event_sequence leftovers of the API path.
    let events: i64 = con
        .query_row(
            "SELECT COUNT(*) FROM event WHERE aggregate_id LIKE 'ses_api_%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let sequences: i64 = con
        .query_row(
            "SELECT COUNT(*) FROM event_sequence WHERE aggregate_id LIKE 'ses_api_%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(events, 0, "event rows cleaned up");
    assert_eq!(sequences, 0, "event_sequence rows cleaned up");
    drop(con);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn restart_service_without_service_falls_back_to_the_guard() {
    let dir = temp_dir("restart-missing");
    create_db(&dir.join("opencode.db"));

    let out = run(
        &[
            "session",
            "purge",
            "--older-than",
            "30d",
            "--restart-service",
            "--yes",
        ],
        &dir,
    );
    // With no registered service the flag falls back to the normal
    // running-instance guard: exit 0 when nothing is running, exit 1 when
    // opencode is (the test harness itself may run under opencode).
    let code = out.status.code();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        code == Some(0) || code == Some(1),
        "unexpected exit code: {code:?} stderr: {stderr}"
    );
    assert!(
        !stderr.contains("no running opencode service"),
        "the flag must not require a service: {stderr}"
    );
    if code == Some(1) {
        assert!(stderr.contains("opencode is running"), "stderr: {stderr}");
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn restart_service_dry_run_previews_without_stopping() {
    let dir = temp_dir("restart-dry");
    create_db(&dir.join("opencode.db"));

    let out = run(
        &[
            "session",
            "purge",
            "--older-than",
            "30d",
            "--restart-service",
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
    assert_eq!(v["deleted"], false);
    assert_eq!(v["sessions"].as_array().unwrap().len(), 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn restart_service_is_ignored_by_read_only_commands() {
    let dir = temp_dir("restart-stats");
    create_db(&dir.join("opencode.db"));

    let out = run(&["stats", "--restart-service"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn backup_list_and_restore_round_trip() {
    let dir = temp_dir("restore");
    let db = dir.join("opencode.db");
    create_db(&db);

    // Take a backup, then add a session so the restore has something to undo.
    let out = run(&["backup", "--yes"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let backup = stdout_json(&out)["backup"]["path"]
        .as_str()
        .unwrap()
        .to_string();
    {
        let con = Connection::open(&db).unwrap();
        con.execute(
            "INSERT INTO session_v2 (id, directory, title, time_updated, cost) \
             VALUES ('ses_extra', '/work/a', 'extra', 0, 0)",
            [],
        )
        .unwrap();
    }

    let out = run(&["backup", "list"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let arr = stdout_json(&out);
    let arr = arr.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let expected = Path::new(&backup)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert_eq!(arr[0]["file"], expected);

    // --verify reports the integrity of each backup.
    let out = run(&["backup", "list", "--verify"], &dir);
    assert!(out.status.success());
    let arr = stdout_json(&out);
    assert_eq!(arr[0]["integrity"], "ok");

    // Dry-run restore verifies the backup but changes nothing.
    let out = run(&["backup", "restore", &backup, "--dry-run"], &dir);
    assert!(out.status.success());
    assert_eq!(stdout_json(&out)["dry_run"], true);
    {
        let con = Connection::open(&db).unwrap();
        let n: i64 = con
            .query_row(
                "SELECT COUNT(*) FROM session_v2 WHERE id = 'ses_extra'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "dry-run restore keeps the current data");
    }

    // Real restore replaces the database (the running-instance guard may
    // refuse with exit 1 while this test harness runs under opencode).
    let out = run(&["backup", "restore", &backup, "--yes"], &dir);
    let code = out.status.code();
    assert!(
        code == Some(0) || code == Some(1),
        "unexpected exit code: {code:?} stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    if code == Some(1) {
        std::fs::remove_dir_all(&dir).unwrap();
        return;
    }
    let v = stdout_json(&out);
    assert_eq!(v["integrity"], "ok");
    assert!(v["safety_backup"]["path"].is_string());

    let con = Connection::open(&db).unwrap();
    let extra: i64 = con
        .query_row(
            "SELECT COUNT(*) FROM session_v2 WHERE id = 'ses_extra'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let original: i64 = con
        .query_row(
            "SELECT COUNT(*) FROM session_v2 WHERE id = 'ses_1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(extra, 0, "restore removed the later session");
    assert_eq!(original, 1, "restore kept the backed-up session");
    drop(con);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn no_input_requires_yes() {
    let dir = temp_dir("no-input");
    create_db(&dir.join("opencode.db"));

    let out = run(
        &["session", "purge", "--older-than", "30d", "--no-input"],
        &dir,
    );
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--yes"), "stderr: {stderr}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn service_status_without_service() {
    let dir = temp_dir("service-status");
    create_db(&dir.join("opencode.db"));

    let out = run(&["service", "status"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(v["registered"], false);
    assert!(
        v["note"]
            .as_str()
            .unwrap()
            .contains("no registered opencode service"),
        "note: {}",
        v["note"]
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn dry_run_prints_the_apply_hint() {
    let dir = temp_dir("apply-hint");
    create_db(&dir.join("opencode.db"));

    let out = run(
        &["session", "purge", "--older-than", "30d", "--dry-run"],
        &dir,
    );
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("to apply:") && stderr.contains("--yes"),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("--dry-run"), "stderr: {stderr}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn config_defaults_apply_to_cleanup() {
    let dir = temp_dir("config");
    create_db(&dir.join("opencode.db"));
    let cfg_dir = dir.join("config/opencode-dbtool");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.toml"),
        "fs_older_than = \"14d\"\n\n[purge]\nolder_than = \"30d\"\n",
    )
    .unwrap();

    let out = run(&["cleanup", "--dry-run"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(
        v["purge"]["sessions"].as_array().unwrap().len(),
        1,
        "config older_than selected the session"
    );
    assert_eq!(v["fs_older_than"], "14d");

    // Command-line flags still win over the config.
    let out = run(&["cleanup", "--path", "/nope", "--dry-run"], &dir);
    assert!(out.status.success());
    let v = stdout_json(&out);
    assert_eq!(
        v["purge"]["sessions"].as_array().unwrap().len(),
        0,
        "the CLI filter wins"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[cfg(target_os = "linux")]
fn service_status_with_mock_service() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let dir = temp_dir("service-status-live");
    let db = dir.join("opencode.db");
    create_db(&db);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let db_thread = db.clone();
    let server = std::thread::spawn(move || {
        // Hold the DB open so the fd check reports a match.
        let _held = Connection::open(&db_thread).unwrap();
        // Never block forever: a regression that stops sending the request
        // must fail the test rather than hang it.
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        panic!("mock server received no request");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("mock server accept failed: {e}"),
            }
        };
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf).unwrap();
        let body = r#"{"version":"2.0.22","pid":1,"urls":[]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    });

    let state = dir.join("state/opencode");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("service.json"),
        serde_json::json!({
            "pid": std::process::id(),
            "url": format!("http://{addr}"),
            "password": "pw",
        })
        .to_string(),
    )
    .unwrap();

    let out = run(&["service", "status"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    server.join().unwrap();
    let v = stdout_json(&out);
    assert_eq!(v["registered"], true);
    assert_eq!(v["db_matches"], true);
    assert_eq!(v["api_ok"], true);
    assert_eq!(v["version"], "2.0.22");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn session_list_sort_and_min_size() {
    let dir = temp_dir("list-sort");
    let db = dir.join("opencode.db");
    create_db(&db);
    {
        let con = Connection::open(&db).unwrap();
        con.execute("UPDATE session_v2 SET cost = 0.5 WHERE id = 'ses_1'", [])
            .unwrap();
        con.execute(
            "INSERT INTO session_v2 (id, directory, title, time_updated, cost) \
             VALUES ('ses_big', '/work/a', 'big', 0, 0.1)",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO session_message (id, session_id, type, data) VALUES ('mbig', 'ses_big', 'assistant', ?1)",
            [&"x".repeat(2000)],
        )
        .unwrap();
        con.execute(
            "INSERT INTO session_message (id, session_id, type, data) VALUES ('mbig2', 'ses_big', 'assistant', 'y')",
            [],
        )
        .unwrap();
    }

    let out = run(&["session", "list", "--sort", "size"], &dir);
    assert!(out.status.success());
    assert_eq!(stdout_json(&out)[0]["id"], "ses_big");

    let out = run(&["session", "list", "--sort", "cost"], &dir);
    assert!(out.status.success());
    assert_eq!(stdout_json(&out)[0]["id"], "ses_1");

    let out = run(&["session", "list", "--sort", "messages"], &dir);
    assert!(out.status.success());
    assert_eq!(stdout_json(&out)[0]["id"], "ses_big");

    let out = run(&["session", "list", "--min-size", "1000"], &dir);
    assert!(out.status.success());
    let v = stdout_json(&out);
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["id"], "ses_big");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn project_list_sort_size() {
    let dir = temp_dir("project-sort");
    let db = dir.join("opencode.db");
    create_db(&db);
    {
        let con = Connection::open(&db).unwrap();
        con.execute(
            "INSERT INTO project (id, worktree, name) VALUES ('p2', '/work/b', 'b')",
            [],
        )
        .unwrap();
    }

    let out = run(&["project", "list", "--sort", "size"], &dir);
    assert!(out.status.success());
    assert_eq!(stdout_json(&out)[0]["id"], "p1");

    let out = run(&["project", "list", "--sort", "sessions"], &dir);
    assert!(out.status.success());
    assert_eq!(stdout_json(&out)[0]["id"], "p1");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn kv_show_raw_prints_value_verbatim() {
    let dir = temp_dir("kv-raw");
    let db = dir.join("opencode.db");
    create_db(&db);
    {
        let con = Connection::open(&db).unwrap();
        con.execute(
            "INSERT INTO kv (key, value, time_created, time_updated) VALUES ('k', 'line1' || char(10) || 'line2', 0, 0)",
            [],
        )
        .unwrap();
    }

    let out = run(&["kv", "show", "k", "--raw"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"line1\nline2\n");

    let out = run(&["kv", "show", "k"], &dir);
    assert_eq!(stdout_json(&out)["value"], "line1\nline2");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn session_show_full_does_not_truncate() {
    let dir = temp_dir("show-full");
    let db = dir.join("opencode.db");
    create_db(&db);
    {
        let con = Connection::open(&db).unwrap();
        con.execute(
            "UPDATE session_message SET data = ?1 WHERE id = 'm1'",
            [&"x".repeat(500)],
        )
        .unwrap();
    }

    let out = run(&["session", "show", "ses_1", "--messages"], &dir);
    assert!(out.status.success());
    let v = stdout_json(&out);
    assert_eq!(v["messages"][0]["truncated"], true);

    let out = run(&["session", "show", "ses_1", "--messages", "--full"], &dir);
    assert!(out.status.success());
    let v = stdout_json(&out);
    assert_eq!(v["messages"][0]["truncated"], false);
    assert_eq!(
        v["messages"][0]["preview"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        500
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn table_relative_time_and_absolute_flag() {
    let dir = temp_dir("relative");
    create_db(&dir.join("opencode.db"));

    // ses_1 has time_updated = 1700000000000 (2023-11-14T22:13:20Z).
    let out = run(&["session", "list", "--format", "table"], &dir);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains("2023-11-14T22:13:20Z"),
        "relative by default: {text}"
    );
    assert!(text.contains("2023-11-14"), "date fallback: {text}");

    let out = run(
        &["session", "list", "--format", "table", "--absolute"],
        &dir,
    );
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("2023-11-14T22:13:20Z"),
        "absolute flag: {text}"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[cfg(unix)]
fn restore_rejects_a_symlink_to_the_database() {
    let dir = temp_dir("restore-self");
    let db = dir.join("opencode.db");
    create_db(&db);
    let link = dir.join("link.db");
    std::os::unix::fs::symlink(&db, &link).unwrap();

    let out = run(
        &["backup", "restore", link.to_str().unwrap(), "--dry-run"],
        &dir,
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("over itself"), "stderr: {stderr}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn backup_list_rejects_keep_backups() {
    let dir = temp_dir("backup-list-flag");
    create_db(&dir.join("opencode.db"));

    // The flag is accepted only before the subcommand (clap rejects it
    // after `list`); in that position it must be refused explicitly.
    let out = run(&["backup", "--keep-backups", "1", "list"], &dir);
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("only applies when creating"),
        "stderr: {stderr}"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn doctor_fix_repairs_orphans() {
    let dir = temp_dir("doctor-fix");
    let db = dir.join("opencode.db");
    create_db(&db);
    {
        let con = Connection::open(&db).unwrap();
        con.execute(
            "INSERT INTO instruction_blob (hash, value) VALUES ('zzz', 'orphan')",
            [],
        )
        .unwrap();
        con.execute(
            "INSERT INTO session_v2 (id, directory, title, parent_id, time_updated, cost) \
             VALUES ('ses_dangling', '/work/a', 'x', 'missing-parent', 0, 0)",
            [],
        )
        .unwrap();
    }

    // Dry-run reports the plan and changes nothing.
    let out = run(&["doctor", "--fix", "--dry-run"], &dir);
    assert_eq!(out.status.code(), Some(3), "problems still exist");
    let v = stdout_json(&out);
    assert_eq!(v["fix"]["dry_run"], true);
    assert_eq!(v["fix"]["blobs_deleted"], 1);
    assert_eq!(v["fix"]["parents_cleared"], 1);
    {
        let con = Connection::open(&db).unwrap();
        let n: i64 = con
            .query_row("SELECT COUNT(*) FROM instruction_blob", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "dry-run keeps the blob");
    }

    // Real run (may be refused while this harness runs under opencode).
    let out = run(&["doctor", "--fix", "--yes"], &dir);
    let code = out.status.code();
    assert!(
        code == Some(0) || code == Some(1),
        "unexpected exit code: {code:?} stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    if code == Some(1) {
        std::fs::remove_dir_all(&dir).unwrap();
        return;
    }
    let v = stdout_json(&out);
    assert_eq!(v["fix"]["blobs_deleted"], 1);
    assert_eq!(v["fix"]["parents_cleared"], 1);
    assert_eq!(v["ok"], true);

    let out = run(&["doctor"], &dir);
    assert!(
        out.status.success(),
        "doctor is healthy after --fix: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn report_lists_suggestions() {
    let dir = temp_dir("report");
    let db = dir.join("opencode.db");
    create_db(&db);
    {
        let con = Connection::open(&db).unwrap();
        con.execute(
            "INSERT INTO kv (key, value, time_created, time_updated) VALUES ('big', ?1, 0, 0)",
            ["x".repeat(2 * 1024 * 1024)],
        )
        .unwrap();
    }

    let out = run(&["report"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(v["old_sessions"]["count"], 1);
    assert_eq!(v["kv_candidates"][0]["key"], "big");
    let commands: Vec<&str> = v["suggestions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["command"].as_str().unwrap())
        .collect();
    assert!(
        commands.iter().any(|c| c.contains("session purge")),
        "suggestions: {commands:?}"
    );
    assert!(
        commands.iter().any(|c| c.contains("kv delete")),
        "suggestions: {commands:?}"
    );

    // Table mode renders the curated summary.
    let out = run(&["report", "--format", "table"], &dir);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("suggestions"), "stdout: {text}");
    assert!(text.contains("session purge"), "stdout: {text}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn csv_format_and_fields() {
    let dir = temp_dir("csv");
    create_db(&dir.join("opencode.db"));

    let out = run(&["session", "list", "--format", "csv"], &dir);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("id,title,directory,updated,session_messages,size_bytes,cost\n"),
        "stdout: {text}"
    );
    assert!(text.contains("ses_1"), "stdout: {text}");

    // --fields selects (and orders) the columns.
    let out = run(
        &[
            "session",
            "list",
            "--format",
            "csv",
            "--fields",
            "size_bytes,id",
        ],
        &dir,
    );
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(text.lines().next().unwrap(), "size_bytes,id");
    assert!(
        text.lines().nth(1).unwrap().ends_with(",ses_1"),
        "stdout: {text}"
    );

    // CSV of a single object is key/value rows.
    let out = run(&["stats", "--format", "csv"], &dir);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("db,"), "stdout: {text}");

    std::fs::remove_dir_all(&dir).unwrap();
}
