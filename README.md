# opencode-dbtool

Maintenance tool for the [opencode](https://opencode.ai) SQLite database
(`~/.local/share/opencode/opencode.db`): inspect sizes and integrity, and delete
projects/sessions to reclaim space.

Written in Rust. All commands print JSON to stdout; errors go to stderr.
Supports Linux, macOS, and Windows.

## Compatibility

Requires the opencode database schema as migrated by **opencode >= 1.18.0**
(the June 2026 schema: `session_context_epoch`/`session_input` in their
current shapes, plus `project_directory`, `workspace`, `event`, ...). Older
databases fail with exit code 3; open the DB once with opencode >= 1.18.0 so
its migrations run, then retry.

## Build

```sh
cargo build --release   # -> target/release/opencode-dbtool
```

## Commands

| command | description |
| --- | --- |
| `stats [--detail]` | DB overview: table sizes, totals, storage usage (`--detail` adds analysis) |
| `doctor` | integrity + consistency checks |
| `project list [--path <dir>]` | project overview (counts, sizes); `--path` filters to a directory (repeatable) |
| `project show <id>` | project detail (sessions, breakdown) |
| `project delete <id>...` | delete project(s) + all related data |
| `project purge [--older-than <age>] [--path <dir>...]` | delete projects matching all filters |
| `session list [--sort size] [--limit <n>]` | per-session breakdown (full ids) |
| `session show <id>` | session detail |
| `session delete <id>...` | delete session(s) + cascade |
| `session purge [--older-than <age>] [--subagents] [--path <dir>...] [--larger-than <size>] [--keep-latest <n>]` | delete sessions matching all filters |
| `session strip-reasoning [--older-than <age>] [--subagents] [--path <dir>...] [--larger-than <size>] [--keep-latest <n>]` | delete only the reasoning parts of matching sessions |
| `fs clean-orphans` | delete session_diff files with no matching session |
| `fs clean-snapshots` | delete all snapshot (undo/redo) storage |
| `fs clean-tool-output` | delete all truncated tool output |
| `fs clean-log` | truncate log/opencode.log to zero bytes |
| `vacuum [--no-backup]` | run VACUUM (backup + verify by default) |

All ids are matched exactly (no prefix/substring resolution). In `purge` /
`strip-reasoning`, `--path <dir>` is an exact match against the session
`directory`; in `project list`/`project purge` it is an exact match against
the project `worktree`. It is never a prefix match; a trailing slash is
ignored on both sides of the comparison.

### Common fields

Commands that print a single result object (`stats`, `doctor`, `project
delete`, `project purge`, `session delete`, `session purge`, `session
strip-reasoning`, `fs clean-orphans`, `fs clean-snapshots`,
`fs clean-tool-output`, `fs clean-log`, `vacuum`)
start with an environment block;
`project/session list` print a bare array and `project/session show` a bare
object:

```json
{ "opencode_running": false, "pids": [], "db": "/path/to/opencode.db" }
```

If the running process cannot be determined, the block instead carries
`"opencode_running": null, "pids": null, "pid_error": "<reason>"` (read-only
commands still proceed; guarded commands fail with exit 3).

### `stats`

Database overview: file and WAL sizes, per-table row counts, total data
volume, the sizes of opencode's filesystem storage, and the breakdown of
`part` rows by type. Start here to see where space is going.

```json
{
  "db_bytes": 8388608,
  "wal_bytes": 0,
  "free_pages": 120,
  "tables": { "session": 6, "message": 640, "part": 5210, "event": 90210, "...": 0 },
  "total_data_bytes": 12345678,
  "storage": { "session_diff_bytes": 0, "snapshot_bytes": 736000, "tool_output_bytes": 0, "log_bytes": 2441760 },
  "part_types": { "reasoning": { "count": 139, "bytes": 400619 }, "text": { "count": 100, "bytes": 10240 } }
}
```

| field | meaning |
| --- | --- |
| `db_bytes` | size of `opencode.db` on disk |
| `wal_bytes` | size of the WAL file |
| `free_pages` | SQLite freelist page count (space that `vacuum` can reclaim) |
| `tables` | row count per table |
| `total_data_bytes` | sum of the `data` column across all tables that have one (message, part, event, session_message, ...) |
| `storage` | sizes of the filesystem storage beside the database: `session_diff_bytes` (`storage/session_diff/`), `snapshot_bytes` (`snapshot/`), `tool_output_bytes` (`tool-output/`), `log_bytes` (`log/`); missing directories count as 0 |
| `part_types` | `part` rows grouped by `data.type` (largest first); rows whose `data` is not JSON or has no `type` are grouped under `"unknown"` |

`stats --detail` adds:

```json
{
  "activity": {
    "days": 30,
    "created": [ { "day": "2026-08-01", "parts": 12, "part_bytes": 1000, "msgs": 3, "msg_bytes": 100 } ]
  },
  "subagent": { "sessions": 5, "total_sessions": 20, "size_bytes": 500000, "total_size_bytes": 2000000 }
}
```

`activity` reports the creation history of `part` and `message` rows over
the last 30 days (by `time_created`), grouped by the user's local
calendar day. `subagent` reports the session count and data size of
subagent sessions relative to all sessions.

### `doctor`

Integrity and consistency checks. Run it when something looks wrong, or
before `vacuum`, to confirm the database is healthy.

```json
{
  "db_bytes": 8388608,
  "wal_bytes": 0,
  "quick_check": "ok",
  "integrity_check": "ok",
  "foreign_key_violations": [],
  "orphans": {
    "sessions_missing_parent": [],
    "sessions_missing_workspace": [],
    "orphaned_event_sequences": [],
    "mismatched_parts": 0
  },
  "ok": true
}
```

| field | meaning |
| --- | --- |
| `quick_check` / `integrity_check` | result of SQLite's `PRAGMA quick_check` / `PRAGMA integrity_check` (both `"ok"` on a healthy database) |
| `foreign_key_violations` | every row rejected by `PRAGMA foreign_key_check` |
| `orphans.sessions_missing_parent` | a session whose `parent_id` points at a nonexistent session |
| `orphans.sessions_missing_workspace` | a session whose `workspace_id` has no `workspace` row |
| `orphans.orphaned_event_sequences` | `event_sequence` rows whose session is gone |
| `orphans.mismatched_parts` | `part` rows whose `session_id` disagrees with their `message`'s |

`orphans` collects references that no FK constraint covers. `ok: false`
(exit code 3) when integrity/fk/orphan checks fail.

### `project list` / `project show`

Browse projects (worktrees opencode tracks) with their session counts and
data sizes. `project list` prints an array; `project show <id>` prints one
object plus `session_list`.

`project list --path <dir>` filters the array to projects whose worktree
matches the directory. `--path` is repeatable (OR) and matches **every**
project registered at that directory, so duplicate worktrees (e.g. a
git-derived id and the literal `global`) all show up. `project purge --path`
deletes all of them likewise.

```json
{
  "id": "e39d872ab709d3fe972b7de3ce41c025915cf4ed",
  "worktree": "/home/user/work/...",
  "name": "...",
  "sessions": 6,
  "msgs": 640,
  "parts": 5210,
  "events": 90210,
  "size_bytes": 12345678,
  "cost": 1.2345,
  "updated": "2026-01-01T00:00:00Z"
}
```

`id` is normally a 40-char SHA-1 derived from the git remote
(`git-remote:<host>/<path>`), falling back to the cached `.git/opencode` value
or the root commit hash. For a directory that is **not under git management**,
the id is the literal `global`.

### `project delete`

Delete one or more projects by id, together with every session and all
related data. Preview the impact with `--dry-run` first.

```json
{
  "dry_run": true,
  "total_rows": 23000,
  "projects": [ { "id": "e39d...", "worktree": "/home/user/work/..." } ],
  "rows": { "session": 6, "message": 640, "part": 5210, "event": 90210, "...": 0 },
  "deleted": false
}
```

`rows` holds per-table row counts across all targeted projects. On success,
`deleted: true` plus a note that the file size only shrinks after `vacuum`.

### `project purge`

Delete the projects selected by filters (same effect as `project delete`,
applied in bulk; output shape plus `action`/`filters`). All set filters
combine with **AND**; at least one filter is required:

| filter | selects |
| --- | --- |
| `--older-than <age>` | projects whose latest session activity (`MAX(session.time_updated)`) is older than the age |
| `--path <dir>` | projects at a directory (repeatable, OR, exact worktree match) |

A filter match of zero projects is normal (exit 0).

### `session list` / `session show`

Per-session breakdown: message/part/event counts, data size, session_diff
size, and cost. Use it with `--sort size` and `--limit` to find the biggest
sessions before purging. `session list` prints an array; `session show
<id>` prints one object. Without `--sort`/`--limit`, `session list` is
ordered by `time_updated` descending.

```json
{
  "id": "ses_...",
  "title": "...",
  "directory": "/home/user/work/...",
  "parent_id": null,
  "updated": "2026-01-01T00:00:00Z",
  "msgs": 106,
  "parts": 868,
  "events": 15035,
  "size_bytes": 2100000,
  "diff_bytes": 1234,
  "cost": 0.42
}
```

`parent_id` is the parent session id for subagent sessions, `null` for
top-level sessions. It also appears in `project show`'s `session_list`.
`diff_bytes` is the size of the session's `storage/session_diff/<id>.json`
file (0 when it does not exist).

### `session delete`

Delete sessions by id, cascading to child sessions (subagents) and all
related data. The session's `storage/session_diff/<id>.json` file (if any)
is removed as well, reported as `diff_files_removed` / `diff_bytes_removed`.
`session purge` and `project delete` behave the same.

```json
{
  "dry_run": true,
  "total_rows": 23000,
  "sessions": [
    { "id": "ses_...", "rows": { "message": 106, "...": 0 }, "total": 23000 }
  ],
  "deleted": false
}
```

On success, `deleted: true` plus a note that the file size only shrinks
after `vacuum`.

### `session purge`

Delete the sessions selected by filters (same effect as `session delete`,
applied in bulk; output shape plus `action`/`filters`). All set filters
combine with **AND**; at least one filter is required:

| filter | selects |
| --- | --- |
| `--older-than <age>` | sessions whose `time_updated` is older than the age |
| `--subagents` | subagent sessions (`parent_id` set) |
| `--path <dir>` | sessions in a directory (repeatable, OR, exact match) |
| `--larger-than <size>` | sessions whose own `size_bytes` (msg+part+event) is larger than the size |
| `--keep-latest <n>` | keep the newest `<n>` matching sessions, purge the rest (`0` keeps nothing) |

`<age>` is `<N><unit>` with units `h`/`d`/`w`; a bare number means days
(e.g. `30d`, `12h`, `2w`). `<size>` is `<N><unit>` with units `K`/`M`/`G`
(1024-based); a bare number means bytes (e.g. `50M`, `500K`, `2G`). The
cutoffs are strict: a session exactly at the age cutoff is not selected,
and a session whose size equals the threshold is not selected.

`--keep-latest` applies after the other filters: the `<n>` most recent
matches by `time_updated` (id as tiebreaker) are kept. For `purge`, the
ancestors of kept sessions are also kept: deleting a parent would orphan
its kept child, so protection can exceed `<n>`.

Selection applies per session, then children of selected sessions are
expanded recursively (same semantics as `session delete`). A child session
that does not match the filters itself is only deleted when an ancestor was
selected.

A filter match of zero sessions is normal (exit 0), not an error:

```json
{
  "dry_run": true,
  "action": "delete",
  "filters": { "older_than": "30d", "subagents": true, "paths": [], "larger_than": null, "keep_latest": null },
  "total_rows": 23000,
  "sessions": [ { "id": "ses_...", "rows": { "message": 106 }, "total": 23000 } ],
  "deleted": false
}
```

### `session strip-reasoning`

Delete only the reasoning content of the sessions selected by the same
filters as `purge` (same output shape plus `action: "strip-reasoning"`).
Filters are optional: without any, every session is stripped. Conversation
text, tool results, messages, and sessions themselves are untouched.

Reasoning lives in three places, all of which are stripped:

| location | handled how |
| --- | --- |
| V1 `part` rows (`data` has `type: "reasoning"`) | rows deleted |
| durable `event` rows (`session.next.reasoning.started` / `.ended`) | rows deleted (`.ended` holds the full text; `.delta` is live-only and never persisted) |
| V2 `session_message` assistant `content[]` | `type: "reasoning"` elements removed from the JSON, rows rewritten only when changed |

Deleting the events also prevents reasoning from being re-projected from
the event log. Token and cost aggregates (`tokens.reasoning`, `cost`) are
kept: they record what was actually billed and are not content.

No child expansion is performed: only sessions that match the filters
themselves are stripped. `--keep-latest` keeps the newest matches'
reasoning (no ancestor protection is needed because sessions are never
deleted).

```json
{
  "dry_run": true,
  "action": "strip-reasoning",
  "filters": { "older_than": null, "subagents": false, "paths": [], "larger_than": null, "keep_latest": null },
  "sessions": [
    { "id": "ses_...", "reasoning_parts": 33, "reasoning_bytes": 106929,
      "reasoning_events": 4, "reasoning_event_bytes": 80000,
      "messages_rewritten": 2, "rewritten_bytes": 5000 }
  ],
  "total_sessions": 1,
  "total_reasoning_parts": 33,
  "total_reasoning_bytes": 106929,
  "total_reasoning_events": 4,
  "total_reasoning_event_bytes": 80000,
  "total_messages_rewritten": 2,
  "total_rewritten_bytes": 5000,
  "stripped": false
}
```

After a real run the tool verifies no reasoning parts, events, or
message content remain for the selected sessions and reports an error
(exit 2) otherwise.

### `fs clean-orphans`

Reclaim space from `storage/session_diff/` files whose session no longer
exists in the database (opencode or other tools can leave them behind when
sessions are removed). Safe to run while opencode runs: opencode only
writes diff files for live sessions, so the files removed here are never
referenced again.

```json
{
  "dry_run": true,
  "dir": "/path/storage/session_diff",
  "orphans": [ { "file": "ses_...", "bytes": 1234 } ],
  "total_files": 3,
  "total_bytes": 5000,
  "deleted": false
}
```

### `fs clean-snapshots`

Delete the entire `snapshot/` directory (git object packs used for
undo/redo). It has no database relationship, but it destroys revert
history, so it is **refused while opencode runs** (exit 1).

```json
{
  "dry_run": true,
  "dir": "/path/snapshot",
  "entries": [ { "name": "<project-id>", "bytes": 736000 } ],
  "total_bytes": 736000,
  "deleted": false
}
```

### `fs clean-tool-output`

Delete `tool-output/` files (the truncated model tool output opencode
spills there, named `tool_<id>`). Like every other `clean-*` command it
takes no arguments and deletes **all** `tool_*` files; files not named
`tool_*` are never touched. (opencode's own hourly cleanup is more
conservative — it keeps 7 days of files — but it only runs while opencode
is running.)

These files are never read back by opencode — sessions only keep marker
text pointing at them — so this command is **safe while opencode runs**.

```json
{
  "dry_run": true,
  "dir": "/path/tool-output",
  "files": [ { "file": "tool_...", "bytes": 500000 } ],
  "total_files": 3,
  "total_bytes": 1200000,
  "deleted": false
}
```

### `fs clean-log`

Truncate `log/opencode.log` to zero bytes. opencode appends to this
single file with no rotation, so it grows unboundedly. Rotation (renaming)
would leave the running opencode writing to the old file, so truncation is
the only option; it is **refused while opencode runs** (exit 1), like
`clean-snapshots`.

```json
{
  "dry_run": true,
  "file": "/path/log/opencode.log",
  "bytes": 2441760,
  "deleted": false
}
```

### `vacuum`

Compact the database file, reclaiming the space freed by deletes. The safe
VACUUM sequence: checkpoint the WAL, run an integrity check (abort on
failure), create a **timestamped backup** of the database file
(`opencode.db.backup-<UTC>`), verify the backup with an integrity check
(abort on failure), VACUUM, restore `journal_mode = WAL`, checkpoint, and
run a final integrity check. The backup is created by default and is kept
until you verify opencode works correctly; it needs free disk space equal
to the database size. `--no-backup` skips the backup. In dry-run mode the
planned backup path and size are reported and nothing is written.

```json
{
  "dry_run": false,
  "db_bytes_before": 8388608,
  "free_pages_before": 120,
  "backup": { "path": "/path/opencode.db.backup-20260830T120000Z", "bytes": 8388608, "integrity": "ok" },
  "db_bytes_after": 2097152,
  "wal_bytes_after": 0,
  "free_pages_after": 0,
  "integrity": "ok"
}
```

## Flags

| flag | meaning |
| --- | --- |
| `--dry-run`, `-n` | print actions without changing anything (safe while opencode runs) |
| `--no-backup` | `vacuum` only: skip the timestamped backup (dangerous) |

## Exit codes

| code | meaning |
| --- | --- |
| 0 | success |
| 1 | opencode is running and the command was refused (close opencode and retry) |
| 2 | not found / bad arguments |
| 3 | database error, or running-process detection failed |

## Environment

| variable | meaning |
| --- | --- |
| `OPENCODE_DATA_DIR` | override data dir |
| `XDG_DATA_HOME` | data dir defaults to `$XDG_DATA_HOME/opencode` |

Default data dir: `~/.local/share/opencode`.

## Concurrency

opencode runs SQLite in WAL mode, and this tool opens the database with a
matching busy timeout, so concurrent reads never wedge.

- `stats`, `doctor`, `project list/show`, `session list/show`,
  `fs clean-orphans`, and `fs clean-tool-output` are safe while opencode
  runs.
- Running instances are detected by process name (`opencode`,
  `opencode-server`) via the [sysinfo](https://crates.io/crates/sysinfo)
  crate, which works on Linux, macOS, and Windows.
- `delete`, `purge`, `strip-reasoning`, `fs clean-snapshots`,
  `fs clean-log`, and `vacuum` are **refused while opencode runs** (exit
  1); close opencode and retry.
  Deleting a session a running instance is using is not safe: the row is
  not resurrected, later writes fail FK checks (new prompts 404, in-flight
  streams 500), and the TUI keeps showing the cached session until
  refreshed. If detection itself fails, guarded commands fail with exit 3
  instead of guessing.
- `--dry-run` is exempt from the running-instance guard: it only reads the DB
  and previews the impact, so it works while opencode runs.
- Deleted rows free space only after `vacuum`.

## Delete semantics

Deletes run in a single immediate transaction with `PRAGMA foreign_keys=ON`:

- `event` / `event_sequence` rows for the target are deleted explicitly (they
  have no FK to session/project).
- The `session` / `project` row is deleted; `message`, `part`, `todo`,
  `session_message`, `session_input`, `session_context_epoch` etc. follow via
  `ON DELETE CASCADE`.
- After commit the tool verifies the rows are gone and reports an error
  (exit 2) if not.

Child sessions (subagent sessions, created via the Task tool) carry a
`parent_id` pointing at their parent. Deleting a parent session also deletes
its children recursively (nested subagents included); they appear in the
output's `sessions` array like any other target. `doctor`'s
`sessions_missing_parent` still detects children whose parent is missing for
other reasons.

`strip-reasoning` runs in its own immediate transaction (foreign keys on)
that only deletes matching `part` rows, then verifies the result.

Every deleted session also removes its `storage/session_diff/<id>.json`
file (best-effort, reported in the output); files of other sessions are
never touched. Orphans created by other tools can be reclaimed with
`fs clean-orphans`.

## License

[MIT](LICENSE)