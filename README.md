# opencode-dbtool

Maintenance tool for the [opencode](https://opencode.ai) 2.x SQLite database
(`~/.local/share/opencode/opencode.db`): inspect sizes and integrity, and delete
projects/sessions to reclaim space.

Written in Rust. Output is a human-readable table on a terminal and JSON when
piped (`--format table|json` to force either). Destructive commands preview
their impact and ask `Proceed? [y/N]` on a terminal; `--dry-run` and `--yes`
keep them scriptable. Supports Linux, macOS, and Windows.

## Compatibility

Requires the opencode 2.x database schema (`session_v2`,
`session_message`, `session_inbox`/`session_pending`, `instruction_*`,
`worktree`, `event`, ...). V1 databases are not supported and fail with
exit code 3; open the DB once with opencode 2.x so its migrations run,
then retry.

## Install

Prebuilt binaries (Linux musl, macOS, Windows; arm64 + x86_64) are attached to
each [GitHub Release](https://github.com/orrisroot/opencode-dbtool/releases),
pushed as a `v<version>` tag matching `Cargo.toml`.

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
| `project purge [--older-than <age>] [--path <dir>...] [--empty]` | delete projects matching all filters |
| `session list [--sort size] [--limit <n>] [--search <text>]` | per-session breakdown (full ids) |
| `session show <id> [--messages [--limit <n>]]` | session detail, optionally with message previews |
| `session delete <id>...` | delete session(s) + cascade |
| `session purge [--older-than <age>] [--subagents] [--archived] [--empty] [--path <dir>...] [--path-prefix <dir>...] [--larger-than <size>] [--keep-latest <n>] [--keep-latest-per-project <n>]` | delete sessions matching all filters |
| `session strip-reasoning [--older-than <age>] [--subagents] [--archived] [--empty] [--path <dir>...] [--path-prefix <dir>...] [--larger-than <size>] [--keep-latest <n>] [--keep-latest-per-project <n>]` | delete only the reasoning content of matching sessions |
| `kv list [--older-than <age>]` | list global kv entries with sizes (large caches first) |
| `kv show <key>` | show a kv value (truncated) |
| `kv delete <key>...` | delete kv entries (caches regenerate on demand) |
| `backup [--keep-backups <n>]` | online verified backup (safe while opencode runs) |
| `fs clean-snapshots [--project <id>...] [--orphans-only]` | delete snapshot storage, optionally scoped |
| `fs clean-shell [--older-than <age>]` | delete shell output files, optionally only old ones |
| `fs clean-blob-orphans` | delete instruction blobs referenced by no state |
| `fs clean-log [--older-than <age>]` | truncate log/opencode.log, or prune only old lines |
| `vacuum [--no-backup] [--keep-backups <n>] [--online]` | run VACUUM (backup + verify by default; `--online` attempts it while opencode runs) |
| `db checkpoint [--truncate]` | checkpoint the WAL; `--truncate` shrinks the file (online) |
| `cleanup [filters] [--fs-older-than <age>] [--no-backup] [--no-vacuum] [--keep-backups <n>]` | backup → optional purge → orphan/file cleanup → VACUUM, in one run |
| `completions <shell>` | print a shell completion script (bash, zsh, fish, ...) |
| `self-update [--dry-run\|--yes]` | check for / install the latest GitHub release binary |

All commands accept `--format <table\|json>` (default: table on a terminal,
JSON when piped) and `--quiet` (suppress progress on stderr). `--dry-run` and
`--yes` may be passed anywhere on the command line. Every subcommand has its
own `--help`.

Session and project references resolve in this order: exact id, a unique id
prefix (for sessions the leading `ses_` may be omitted), then — for projects —
an exact worktree path. An ambiguous prefix, or a worktree shared by several
projects, is refused with the candidate list; use the full id to disambiguate.
`--path` filters stay exact matches: in `purge` / `strip-reasoning` it is an
exact match against the session `directory`; in `project list`/`project purge`
it is an exact match against the project `worktree`. It is never a prefix
match; a trailing slash is ignored on both sides of the comparison.
`--path-prefix` matches a directory and everything below it.

### Common fields

Commands that print a single result object (`stats`, `doctor`, `project
delete`, `project purge`, `session delete`, `session purge`, `session
strip-reasoning`, `kv delete`, `backup`, `fs clean-snapshots`,
`fs clean-shell`, `fs clean-blob-orphans`, `fs clean-log`, `vacuum`,
`cleanup`, `db checkpoint`)
start with an environment block;
`project/session list` print a bare array and `project/session show` a bare
object:

```json
{ "opencode_running": false, "pids": [], "db": "/path/to/opencode.db" }
```

If the running process cannot be determined, the block instead carries
`"opencode_running": null, "pids": null, "pid_error": "<reason>"` (read-only
commands still proceed; guarded commands fail with exit 3).

### Output formats

The JSON shapes below are the stable contract: they are always printed when
stdout is not a terminal (scripts, pipes) and with `--format json`. On a
terminal the same values render as a table — list commands show one row per
item, single results show aligned `key: value` lines, and byte fields
(`bytes`, `*_bytes`) are formatted for humans (`8.4 MB`). `--format table`
forces the table even when piped. The interactive prompt and errors go to
stderr, so stdout stays machine-readable.

### Interactive confirmation

Destructive commands (`session`/`project delete` and `purge`,
`strip-reasoning`, `kv delete`, `backup`, `fs clean-*`, `vacuum`, `cleanup`,
`self-update`) work in three modes:

- `--dry-run`: print the preview and change nothing (safe while opencode
  runs).
- `--yes`: execute for real (the running-instance guard applies).
- neither, on a terminal: print the preview, ask `Proceed? [y/N]`, then
  execute or abort.
- neither, not a terminal: exit 2 with a message pointing at both flags.

### Online maintenance (opencode running)

opencode 2.x keeps a background server (`opencode serve --service`) running
after the TUI exits, so maintenance cannot assume an idle database. This
tool adapts:

- **Session deletes and purges are routed through the running server's API**
  (`DELETE /api/session/<id>`) when the server operates on the same database
  file. The server owns its caches and event log, so no stale-state or
  foreign-key failures arise; children are deleted before their parents.
- **`backup` works online**: SQLite's online backup API takes a consistent
  snapshot of a live WAL database.
- **`db checkpoint [--truncate]`** moves committed WAL frames into the
  database file and can shrink the WAL while opencode runs (best effort:
  `busy: 1` means active readers held it back).
- **`fs clean-shell --older-than`**, **`fs clean-snapshots --orphans-only`**,
  and **`fs clean-blob-orphans`** are safe online.
- **`vacuum --online`** attempts a VACUUM while opencode runs with a long
  busy timeout; the server may block writes for the duration, and the
  command fails cleanly if it cannot win the lock.
- **`--restart-service`** (global flag) stops the registered service
  through its own `opencode service stop`, runs the command, and starts it
  again afterwards — even if the command fails. Use it for the commands
  that still need exclusive access (`project delete/purge`, `session
  strip-reasoning`, `kv delete`, `fs clean-log`, full `fs
  clean-snapshots`/`clean-shell`, `vacuum`, and `cleanup` including its
  VACUUM). A running TUI may need to be restarted if the service comes
  back on a different port.
- `project delete/purge`, `session strip-reasoning`, `kv delete`,
  `fs clean-log`, `fs clean-snapshots` without `--orphans-only`, and
  `fs clean-shell` without `--older-than` still require stopping opencode
  (exit 1) unless `--restart-service` is passed.

To stop and restart the service manually:

```sh
opencode service status   # prints the server URL, or "stopped"
opencode service stop
opencode-dbtool vacuum --yes
opencode service start
```

`--restart-service` automates exactly this sequence:

```sh
opencode-dbtool cleanup --older-than 30d --subagents --restart-service
```

The tool discovers the service through `service.json` (or `server.json`)
under `$XDG_STATE_HOME/opencode` (`~/.local/state/opencode`). On Linux it
only routes through the server after verifying, via `/proc/<pid>/fd`, that
the server has the target database open; elsewhere it trusts the service
only for the default database location (no `OPENCODE_DB` /
`OPENCODE_DATA_DIR` override).

### `stats`

Database overview: file and WAL sizes, per-table row counts, total data
volume, the sizes of opencode's filesystem storage, and the breakdown of
`session_message` rows by type. Start here to see where space is going.

```json
{
  "db_bytes": 8388608,
  "wal_bytes": 0,
  "free_pages": 120,
  "tables": { "session_v2": 6, "session_message": 640, "event": 90210, "...": 0 },
  "total_data_bytes": 12345678,
  "storage": { "snapshot_bytes": 736000, "shell_bytes": 39681, "repos_bytes": 0, "log_bytes": 2441760 },
  "message_types": { "assistant": { "count": 400, "bytes": 900000 }, "user": { "count": 240, "bytes": 10240 } }
}
```

| field | meaning |
| --- | --- |
| `db_bytes` | size of `opencode.db` on disk |
| `wal_bytes` | size of the WAL file |
| `free_pages` | SQLite freelist page count (space that `vacuum` can reclaim) |
| `tables` | row count per table |
| `total_data_bytes` | sum of content columns across all tables (`data`, `payload`, `value`, `initial_values`/`current_values`, ...) |
| `storage` | sizes of the filesystem storage beside the database: `snapshot_bytes` (`snapshot/`), `shell_bytes` (`shell/`), `repos_bytes` (`repos/`), `log_bytes` (`log/`); missing directories count as 0 |
| `message_types` | `session_message` rows grouped by `type` (largest first) |

`stats --detail` adds:

```json
{
  "activity": {
    "days": 30,
    "created": [ { "day": "2026-08-01", "messages": 12, "message_bytes": 1000 } ]
  },
  "subagent": { "sessions": 5, "total_sessions": 20, "size_bytes": 500000, "total_size_bytes": 2000000 }
}
```

`activity` reports the `session_message` creation history over the last 30
days (by `time_created`), grouped by the user's local calendar day.
`subagent` reports the session count and data size of subagent sessions
relative to all sessions.

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
    "sessions_dangling_fork": [],
    "sessions_missing_workspace": [],
    "orphaned_event_sequences": [],
    "orphan_instruction_blobs": []
  },
  "ok": true
}
```

| field | meaning |
| --- | --- |
| `quick_check` / `integrity_check` | result of SQLite's `PRAGMA quick_check` / `PRAGMA integrity_check` (both `"ok"` on a healthy database) |
| `foreign_key_violations` | every row rejected by `PRAGMA foreign_key_check` |
| `orphans.sessions_missing_parent` | a session whose `parent_id` points at a nonexistent session |
| `orphans.sessions_dangling_fork` | a session whose `fork_session_id` points at a nonexistent session (no FK covers forks) |
| `orphans.sessions_missing_workspace` | a session whose `workspace_id` has no `workspace` row (no FK covers it) |
| `orphans.orphaned_event_sequences` | `event_sequence` rows whose aggregate is in neither sessions nor projects |
| `orphans.orphan_instruction_blobs` | `instruction_blob` rows referenced by no state (leaked by session/project deletes; reclaim with `fs clean-blob-orphans`) |

`orphans` collects references that no FK constraint covers, plus
`session_message`/`inbox`/`pending`/`instruction_*` rows whose session is
gone (folded into `ok: false`). `ok: false` (exit code 3) when
integrity/fk/orphan checks fail.

### `project list` / `project show`

Browse projects (worktrees opencode tracks) with their session counts and
data sizes. `project list` prints an array; `project show <ref>` prints one
object plus `session_list`. `<ref>` may be the full project id, a unique id
prefix, or an exact worktree path (when it maps to exactly one project).

`project list --path <dir>` filters the array to projects whose worktree
matches the directory. `--path` is repeatable (OR) and matches **every**
project registered at that directory. `project purge --path` deletes all of
them likewise.

```json
{
  "id": "e39d872ab709d3fe972b7de3ce41c025915cf4ed",
  "worktree": "/home/user/work/...",
  "name": "...",
  "sessions": 6,
  "events": 90210,
  "session_messages": 640,
  "size_bytes": 12345678,
  "cost": 1.2345,
  "updated": "2026-01-01T00:00:00Z"
}
```

### `project delete`

Delete one or more projects by reference (id, unique id prefix, or an
exact worktree path that maps to exactly one project), together with every
session and all related data. Preview the impact with `--dry-run` first.

```json
{
  "dry_run": true,
  "total_rows": 23000,
  "projects": [ { "id": "e39d...", "worktree": "/home/user/work/..." } ],
  "rows": { "session_v2": 6, "session_message": 640, "event": 90210, "...": 0 },
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
| `--older-than <age>` | projects whose latest session activity (`MAX(session_v2.time_updated)`) is older than the age |
| `--path <dir>` | projects at a directory (repeatable, OR, exact worktree match) |
| `--empty` | projects with no sessions |

A filter match of zero projects is normal (exit 0).

### `session list` / `session show`

Per-session breakdown: event/`session_message`/`inbox`/`pending` counts,
data size, archived flag, and cost. Use it with `--sort size` and `--limit`
to find the biggest sessions before purging. `--search <text>` keeps only
sessions whose title or directory contains the text (case-insensitive).
`session list` prints an array; `session show <ref>` prints one object (add
`--messages [--limit <n>]` for oldest-first message previews with
per-message bytes, plus `total_messages`). Without `--sort`/`--limit`,
`session list` is ordered by `time_updated` descending. `size_bytes` sums
session_message+inbox+pending+instructions+event bytes.

`<ref>` may be the full id, a unique id prefix, or the id without its `ses_`
prefix (`session show ses_effb2a71` and `session show effb2a71` both work
when unambiguous).

```json
{
  "id": "ses_...",
  "title": "...",
  "directory": "/home/user/work/...",
  "parent_id": null,
  "updated": "2026-01-01T00:00:00Z",
  "archived": false,
  "events": 15035,
  "session_messages": 210,
  "inbox": 0,
  "pending": 1,
  "size_bytes": 2100000,
  "cost": 0.42
}
```

`parent_id` is the parent session id for subagent sessions, `null` for
top-level sessions. It also appears in `project show`'s `session_list`.

### `session delete`

Delete sessions by reference (id, unique id prefix, or id without `ses_`),
cascading to child sessions (subagents) and all related data. `session
purge` and `project delete` behave the same.

```json
{
  "dry_run": true,
  "total_rows": 23000,
  "sessions": [
    { "id": "ses_...", "rows": { "session_message": 106, "...": 0 }, "total": 23000 }
  ],
  "deleted": false
}
```

On success, `deleted: true` plus a note that the file size only shrinks
after `vacuum`. When the opencode server is running against the same
database, the deletes go through its API and the note says so.

### `session purge`

Delete the sessions selected by filters (same effect as `session delete`,
applied in bulk; output shape plus `action`/`filters`). All set filters
combine with **AND**; at least one filter is required:

| filter | selects |
| --- | --- |
| `--older-than <age>` | sessions whose `time_updated` is older than the age |
| `--subagents` | subagent sessions (`parent_id` set) |
| `--archived` | archived sessions (`time_archived` set) |
| `--empty` | sessions with no content rows (zero `size_bytes`) |
| `--path <dir>` | sessions in a directory (repeatable, OR, exact match) |
| `--path-prefix <dir>` | sessions in a directory or anything below it (repeatable, OR) |
| `--larger-than <size>` | sessions whose own `size_bytes` is larger than the size |
| `--keep-latest <n>` | keep the newest `<n>` matching sessions, purge the rest (`0` keeps nothing) |
| `--keep-latest-per-project <n>` | keep the newest `<n>` matching sessions per project instead (mutually exclusive with `--keep-latest`) |

`<age>` is `<N><unit>` with units `h`/`d`/`w`; a bare number means days
(e.g. `30d`, `12h`, `2w`). `<size>` is `<N><unit>` with units `K`/`M`/`G`
(1024-based); a bare number means bytes (e.g. `50M`, `500K`, `2G`). The
cutoffs are strict: a session exactly at the age cutoff is not selected,
and a session whose size equals the threshold is not selected.

`--keep-latest` applies after the other filters: the `<n>` most recent
matches by `time_updated` (id as tiebreaker) are kept.
`--keep-latest-per-project` does the same per project (retention policies).
For `purge`, the ancestors of kept sessions are also kept: deleting a
parent would orphan its kept child, so protection can exceed `<n>`.

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
  "sessions": [ { "id": "ses_...", "rows": { "session_message": 106 }, "total": 23000 } ],
  "deleted": false
}
```

### `session strip-reasoning`

Delete only the reasoning content of the sessions selected by the same
filters as `purge` (same output shape plus `action: "strip-reasoning"`).
Filters are optional: without any, every session is stripped. Conversation
text, tool results, messages, and sessions themselves are untouched.

Reasoning lives in two places, both of which are stripped:

| location | handled how |
| --- | --- |
| durable `event` rows (`session.next.reasoning.started` / `.ended`) | rows deleted (`.ended` holds the full text; `.delta` is live-only and never persisted) |
| `session_message` assistant `content[]` | `type: "reasoning"` elements removed from the JSON, rows rewritten only when changed |

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
    { "id": "ses_...",
      "reasoning_events": 4, "reasoning_event_bytes": 80000,
      "messages_rewritten": 2, "rewritten_bytes": 5000 }
  ],
  "total_sessions": 1,
  "total_reasoning_events": 4,
  "total_reasoning_event_bytes": 80000,
  "total_messages_rewritten": 2,
  "total_rewritten_bytes": 5000,
  "stripped": false
}
```

After a real run the tool verifies no reasoning events or message content
remain for the selected sessions and reports an error (exit 2) otherwise.

### `fs clean-snapshots`

Delete snapshot storage (git object packs used for undo/redo), optionally
scoped: `--project <id>` (repeatable) limits deletion to those projects,
`--orphans-only` deletes only directories whose project no longer exists
(e.g. left behind by `project delete`, which removes rows but not files).
Without filters everything is deleted. It destroys revert history, so it is
**refused while opencode runs** (exit 1).

```json
{
  "dry_run": true,
  "dir": "/path/snapshot",
  "projects": [],
  "orphans_only": false,
  "entries": [ { "name": "<project-id>", "bytes": 736000 } ],
  "total_bytes": 736000,
  "deleted": false
}
```

### `fs clean-shell`

Delete shell output files (`shell/<project>/sh_*.out`). With
`--older-than <age>` only outputs not modified since the cutoff are
removed; without it everything goes. Empty project directories are pruned
afterwards. Guarded while opencode runs (live runs append to these files).

```json
{
  "dry_run": true,
  "dir": "/path/shell",
  "files": [ { "file": "<project-id>/sh_....out", "bytes": 1258 } ],
  "total_files": 26,
  "total_bytes": 39681,
  "deleted": false
}
```

### `fs clean-blob-orphans`

Delete `instruction_blob` rows referenced by no `instruction_state`
(content-addressed blobs leak when sessions/projects are deleted, since the
states cascade but the blobs have no FK). Run it after `purge`/`delete`
runs. Guarded while opencode runs. `doctor` reports the same orphans.

```json
{
  "dry_run": true,
  "orphans": [ { "hash": "d43ac7...", "bytes": 1234 } ],
  "total_blobs": 1,
  "total_bytes": 1234,
  "deleted": false
}
```

### `fs clean-log`

Truncate `log/opencode.log` to zero bytes, or with `--older-than <age>`
drop only lines older than the cutoff (lines without a parseable
`timestamp=` prefix are kept, and the surviving byte count is reported as
`remaining_bytes`). opencode appends to this single file with no rotation,
so it grows unboundedly. Rotation (renaming) would leave the running opencode
writing to the old file, so truncation/pruning in place is the only option;
both modes are **refused while opencode runs** (exit 1), like
`clean-snapshots`.

```json
{
  "dry_run": true,
  "file": "/path/log/opencode.log",
  "bytes": 2441760,
  "remaining_bytes": 1200,
  "deleted": false
}
```

### `backup`

Create a verified timestamped backup (`opencode.db.backup-<UTC>`) without
touching table data: integrity check, **online copy** (SQLite's backup API
takes a consistent snapshot of a live WAL database), verify the copy, and
optional `--keep-backups <n>` pruning. Safe while opencode runs.

```json
{
  "dry_run": true,
  "backup": { "path": "/path/opencode.db.backup-20260830T120000Z", "bytes": 8388608 },
  "deleted": false
}
```

### `kv list` / `kv show` / `kv delete`

Inspect the global `kv` table, which can dominate database size (e.g. the
multi-megabyte `models-dev:catalog` cache). `kv list [--older-than <age>]`
prints `{key, bytes, updated}` largest first; `kv show <key>` prints the
value truncated to 2000 chars plus `truncated`; `kv delete <key>...`
removes keys (caches regenerate on demand) and is guarded while opencode
runs. Preview with `--dry-run` first.

### `vacuum`

Compact the database file, reclaiming the space freed by deletes. The safe
VACUUM sequence: checkpoint the WAL, run an integrity check (abort on
failure), create a **timestamped backup** of the database file
(`opencode.db.backup-<UTC>`), verify the backup with an integrity check
(abort on failure), VACUUM, restore `journal_mode = WAL`, checkpoint, and
run a final integrity check. The backup is created by default and is kept
until you verify opencode works correctly; it needs free disk space equal
to the database size. `--no-backup` skips the backup. `--keep-backups <n>`
keeps only the newest `<n>` backups found after a successful run and
deletes the older `opencode.db.backup-*` files (it cannot be combined
with `--no-backup`). In dry-run mode the
planned backup path and size are reported and nothing is written.

Add `--online` to attempt the VACUUM while opencode runs: the tool waits up
to 60 seconds for the write lock. The server may block writes for the
duration; if the lock cannot be won, the command fails with a database
error (nothing is corrupted). Without `--online`, VACUUM is refused while
opencode runs (exit 1).

```json
{
  "dry_run": false,
  "online": false,
  "db_bytes_before": 8388608,
  "free_pages_before": 120,
  "backup": { "path": "/path/opencode.db.backup-20260830T120000Z", "bytes": 8388608, "integrity": "ok" },
  "db_bytes_after": 2097152,
  "wal_bytes_after": 0,
  "free_pages_after": 0,
  "integrity": "ok"
}
```

With `--keep-backups <n>` (at least 1), a `backup_cleanup` block is
added on a real run (dry-run reports nothing since no backup is touched):

```json
"backup_cleanup": { "kept": 3, "removed_files": 7, "removed_bytes": 52428800 }
```

### `db checkpoint`

Checkpoint the WAL into the database file. PASSIVE (the default) never
blocks; `--truncate` additionally shrinks `opencode.db-wal` when no
connection holds a read snapshot. Both are safe while opencode runs and
report the lock state instead of failing:

```json
{
  "mode": "truncate",
  "busy": 0,
  "log_frames": 0,
  "checkpointed_frames": 0,
  "wal_bytes_before": 10098152,
  "wal_bytes_after": 0
}
```

### `cleanup`

One-shot maintenance: runs the common cleanup steps in a single guarded
command and reports each step's result. Steps, in order:

1. a verified pre-run backup (skipped with `--no-backup`, and when no
   session filters are given);
2. `session purge` with the given filters — **without session filters no
   sessions are deleted**, so a bare `cleanup` only removes orphans and old
   files;
3. `fs clean-blob-orphans`;
4. `fs clean-snapshots --orphans-only`;
5. `fs clean-shell --older-than <age>` (default `7d`, change with
   `--fs-older-than`);
6. `fs clean-log --older-than <age>` (same cutoff);
7. `vacuum` (skipped with `--no-vacuum`; no second backup is taken because
   step 1 already backed up the pre-change database). **While opencode
   runs, the final VACUUM is skipped** with a note, because it needs the
   write lock; run `opencode-dbtool vacuum --online` afterwards, or stop
   the service. The purge step is routed through the server API, so a
   `cleanup` with session filters is safe online.
   `cleanup --restart-service` stops the service first instead: the purge
   then uses the direct database path and the final VACUUM runs too.

All `session purge` filters are accepted directly. `--keep-backups <n>`
prunes older backups after the pre-run backup. The output embeds each step's
result (without nested environment blocks) plus `action`, `filters`,
`fs_older_than`, `cleaned`, and a `note`. `--dry-run` prints the complete
plan without changing anything.

```sh
opencode-dbtool cleanup --older-than 30d --subagents --keep-latest-per-project 5
```

### `completions`

Print a completion script for your shell (bash, elvish, fish, powershell,
zsh):

```sh
source <(opencode-dbtool completions bash)
opencode-dbtool completions zsh > ~/.zfunc/_opencode-dbtool
```

### `self-update`

Update the running binary from the [GitHub
releases](https://github.com/orrisroot/opencode-dbtool/releases) of this
repository. `--dry-run` only checks (reports what would happen, changes
nothing); `--yes` downloads the release asset for the current platform
(`x86_64`/`aarch64` for Linux musl, macOS, and Windows) and replaces the
running executable in place. `GH_TOKEN`/`GITHUB_TOKEN` is picked up from the
environment when set, avoiding the unauthenticated GitHub API rate limit.

```json
{
  "command": "self-update",
  "current_version": "0.3.0",
  "latest_version": "0.3.0",
  "target": "x86_64-unknown-linux-musl",
  "update_available": true,
  "updated": false,
  "dry_run": true,
  "status": "update-available",
  "path": "/home/user/.cargo/bin/opencode-dbtool",
  "release_url": "https://github.com/orrisroot/opencode-dbtool/releases/tag/v0.3.0"
}
```

`status` is `"up-to-date"`, `"update-available"` (dry-run, or nothing
applied), or `"updated"` (`updated: true`, the binary at `path` was
replaced). Only stable releases are considered: the reported version is
the one actually installed (the apply step is pinned to that release). A
source build whose libc differs from the published ones still updates to
the static musl build on Linux; an unsupported local platform fails with
exit code 2, while a release missing this platform's asset, network/API
failures, and download problems surface as exit code 3.

## Flags

| flag | meaning |
| --- | --- |
| `--dry-run`, `-n` | print actions without changing anything (safe while opencode runs) |
| `--yes`, `-y` | confirm a destructive command. On a terminal, omitting `--yes`/`--dry-run` shows the preview and asks `Proceed? [y/N]` instead; off a terminal `--yes` is required for every real run of `delete`, `purge`, `strip-reasoning`, `kv delete`, `backup`, `fs clean-*`, `vacuum`, `cleanup`, and `self-update` |
| `--format <table\|json>` | output format (default: table on a terminal, JSON when piped) |
| `--quiet` | suppress progress and confirmation messages on stderr |
| `--no-backup` | `vacuum`/`cleanup`: skip the timestamped backup (dangerous) |
| `--online` | `vacuum` only: attempt VACUUM while opencode runs (may block the server briefly) |
| `--restart-service` | stop the registered opencode service for the run and restart it afterwards (for commands that need exclusive access) |

Destructive commands refuse to run without `--yes`, `--dry-run`, or an
interactive terminal (exit code 2): use `--dry-run` to preview the impact
before confirming.

## Recipes

```sh
# Where is the space going?
opencode-dbtool stats --detail

# Biggest sessions first
opencode-dbtool session list --sort size --limit 10

# Preview a retention policy, then apply it
opencode-dbtool session purge --older-than 30d --keep-latest-per-project 5 --dry-run
opencode-dbtool session purge --older-than 30d --keep-latest-per-project 5

# One-shot cleanup (backup, purge, orphan/file cleanup, VACUUM)
opencode-dbtool cleanup --older-than 30d --subagents

# Maintenance while the opencode service keeps running
opencode-dbtool db checkpoint --truncate
opencode-dbtool backup
opencode-dbtool session purge --older-than 30d --subagents

# Reclaim the database file size (VACUUM needs the write lock)
opencode-dbtool vacuum --online

# Full cleanup including VACUUM, with an automatic service restart
opencode-dbtool cleanup --older-than 30d --subagents --restart-service

# Inspect a huge cache key
opencode-dbtool kv list
opencode-dbtool kv show models-dev:catalog

# Shell completions
source <(opencode-dbtool completions bash)
```

## Exit codes

| code | meaning |
| --- | --- |
| 0 | success |
| 1 | opencode is running and the command is not safe online (see "Online maintenance"; `--dry-run` still works) |
| 2 | not found / bad arguments |
| 3 | database or network error, or running-process detection failed |

## Environment

| variable | meaning |
| --- | --- |
| `OPENCODE_DATA_DIR` | override data dir |
| `OPENCODE_DB` | override database path (same rules as opencode: `:memory:` and absolute paths as-is, relative resolves against the data dir) |
| `OPENCODE_DISABLE_CHANNEL_DB` | skip the channel-database fallback and always target `opencode.db` |
| `XDG_DATA_HOME` | data dir defaults to `$XDG_DATA_HOME/opencode` |
| `XDG_STATE_HOME` | state dir for service discovery defaults to `$XDG_STATE_HOME/opencode` (`~/.local/state/opencode`) |
| `GH_TOKEN` / `GITHUB_TOKEN` | `self-update` only: GitHub API token (raises the release-check rate limit) |

Default data dir: `~/.local/share/opencode`.

### Database selection

Commands always print the database they operate on in the environment
block's `db` field. The path resolves in this order:

1. `$OPENCODE_DB`, exactly like opencode resolves it: `:memory:` and
   absolute paths are used as-is, a relative path is joined onto the data
   dir.
2. `opencode.db` in the data dir (what opencode uses on the
   `latest`/`beta`/`prod` channels, and with `OPENCODE_DISABLE_CHANNEL_DB`
   set — the tool honors that variable too).
3. A single channel database `opencode-<channel>.db` in the data dir
   (the layout of installs on other channels). Several such files with no
   `opencode.db` are refused with exit 2 naming them; set `OPENCODE_DB`
   to pick one. Backup files (`*.backup-*`) and WAL/SHM siblings never
   count as candidates.

The filesystem storage (`snapshot/`, `log/`) always follows the data dir,
never the database file's location — matching opencode, which keeps these
directories in the data dir even when `OPENCODE_DB` points elsewhere.

## Concurrency

opencode runs SQLite in WAL mode, and this tool opens the database with a
matching busy timeout, so concurrent reads never wedge.

- `stats`, `doctor`, `project list/show`, `session list/show`, `kv list/show`
  are safe while opencode runs.
- Running instances are detected by process name, executable path, and
  command line (`opencode`, `opencode-server`) via the
  [sysinfo](https://crates.io/crates/sysinfo) crate, which works on
  Linux, macOS, and Windows. Command-line matching only accepts
  invocations (e.g. `/usr/bin/opencode`), not references to opencode's
  data files.
- Safe online: `backup`, `db checkpoint`, `fs clean-shell --older-than`,
  `fs clean-snapshots --orphans-only`, `fs clean-blob-orphans`, and
  `session delete`/`purge` (also the purge step of `cleanup`) when the
  running server's API is available — see "Online maintenance" above.
- Still refused while opencode runs (exit 1): `project delete/purge`,
  `session strip-reasoning`, `kv delete`, `fs clean-log`, `fs
  clean-snapshots` without `--orphans-only`, `fs clean-shell` without
  `--older-than`, and `vacuum` without `--online`.
  Deleting a session a running instance is using directly is not safe: the
  row is not resurrected, later writes fail FK checks, and the TUI keeps
  showing the cached session until refreshed. If detection itself fails,
  guarded commands fail with exit 3 instead of guessing.
- `--dry-run` is exempt from the running-instance guard: it only reads the DB
  and previews the impact, so it works while opencode runs.
- Deleted rows free space only after `vacuum` (or, for the WAL itself,
  `db checkpoint --truncate`).

## Delete semantics

Deletes run in a single immediate transaction with `PRAGMA foreign_keys=ON`:

- `event` / `event_sequence` rows for the target are deleted explicitly (they
  reference aggregates, not sessions).
- The `session_v2` / `project` row is deleted; `session_message`,
  `session_inbox`, `session_pending`, `instruction_*` follow via
  `ON DELETE CASCADE`.
- After commit the tool verifies the rows are gone and reports an error
  (exit 2) if not.

When a matching opencode server is running (same database file), `session
delete`/`purge` use `DELETE /api/session/<id>` instead, deepest children
first. The server removes the same rows (including `event`/`event_sequence`)
and keeps its caches consistent; the tool verifies the result the same way.

Child sessions (subagent sessions) carry a `parent_id` pointing at their
parent. Deleting a parent session also deletes its children recursively
(nested subagents included); they appear in the output's `sessions` array
like any other target. `doctor`'s `sessions_missing_parent` still detects
children whose parent is missing for other reasons.

`strip-reasoning` runs in its own immediate transaction (foreign keys on)
that deletes matching reasoning `event` rows and rewrites `session_message`
rows, then verifies the result.

## License

[MIT](LICENSE)
