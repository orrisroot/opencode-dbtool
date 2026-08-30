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

```
stats                   table sizes + totals (DB overview)
doctor                  integrity + consistency checks
project list            project overview (counts, sizes)
project list --path <d> list only projects at a directory (repeatable)
project show <id>       project detail (sessions, breakdown)
project delete <id>...  delete project(s) + all related data
project purge [--older-than <age>] [--path <dir>...]  delete projects matching all filters
session list            per-session breakdown (full ids)
session show <id>       session detail
session delete <id>...  delete session(s) + cascade
session purge [--older-than <age>] [--subagents] [--path <dir>...]  delete sessions matching all filters
session strip-reasoning [--older-than <age>] [--subagents] [--path <dir>...]  delete only the reasoning parts of matching sessions
vacuum                  run VACUUM
```

All ids are matched exactly (no prefix/substring resolution). In `purge` /
`strip-reasoning`, `--path <dir>` is an exact match against the session
`directory`; in `project list`/`project purge` it is an exact match against
the project `worktree`. It is never a prefix match.

### Common fields

Commands that print a single result object (`stats`, `doctor`, `project
delete`, `project purge`, `session delete`, `session purge`, `session
strip-reasoning`, `vacuum`) start with an environment block;
`project/session list` print a bare array and `project/session show` a bare
object:

```json
{ "opencode_running": false, "pids": [], "db": "/path/to/opencode.db" }
```

If the running process cannot be determined, the block instead carries
`"opencode_running": null, "pids": null, "pid_error": "<reason>"` (read-only
commands still proceed; guarded commands fail with exit 3).

### `stats`

```json
{
  "db_bytes": 8388608,
  "wal_bytes": 0,
  "free_pages": 120,
  "tables": { "session": 6, "message": 640, "part": 5210, "event": 90210, "...": 0 },
  "total_data_bytes": 12345678
}
```

`tables` maps every table name to its row count. `total_data_bytes` sums the
`data` column of every table that has one (message, part, event,
session_message, ...). `db_bytes` is the file size on disk.

### `doctor`

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
    "mismatched_parts": []
  },
  "ok": true
}
```

`ok: false` (exit code 3) when integrity/fk/orphan checks fail.

### `project list` / `project show`

`project list` prints an array; `project show <id>` prints one object plus
`session_list`.

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

Deletes the projects selected by filters (same output shape as `project
delete`, plus `action`/`filters`). All set filters combine with **AND**; at
least one filter is required:

| filter | selects |
| --- | --- |
| `--older-than <age>` | projects whose latest session activity (`MAX(session.time_updated)`) is older than the age |
| `--path <dir>` | projects at a directory (repeatable, OR, exact worktree match) |

Deleting a project also deletes every session and all related data of that
project (same semantics as `project delete`). A filter match of zero
projects is normal (exit 0).

### `session list` / `session show`

`session list` prints an array; `session show <id>` prints one object.

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
  "cost": 0.42
}
```

`parent_id` is the parent session id for subagent sessions, `null` for
top-level sessions. It also appears in `project show`'s `session_list`.

### `session delete`

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

Deletes the sessions selected by filters. All set filters combine with
**AND**; at least one filter is required:

| filter | selects |
| --- | --- |
| `--older-than <age>` | sessions whose `time_updated` is older than the age |
| `--subagents` | subagent sessions (`parent_id` set) |
| `--path <dir>` | sessions in a directory (repeatable, OR, exact match) |

`<age>` is `<N><unit>` with units `h`/`d`/`w`; a bare number means days
(e.g. `30d`, `12h`, `2w`). The cutoff is strict: a session updated exactly
at the cutoff is not selected.

Selection applies per session, then children of selected sessions are
expanded recursively (same semantics as `session delete`). A child session
that does not match the filters itself is only deleted when an ancestor was
selected.

`purge` adds `action` and `filters` to the delete output shape; a filter
match of zero sessions is normal (exit 0), not an error:

```json
{
  "dry_run": true,
  "action": "delete",
  "filters": { "older_than": "30d", "subagents": true, "paths": [] },
  "total_rows": 23000,
  "sessions": [ { "id": "ses_...", "rows": { "message": 106 }, "total": 23000 } ],
  "deleted": false
}
```

### `session strip-reasoning`

Deletes only the `reasoning` parts (`part` rows whose `data` has
`type: "reasoning"`) of the sessions selected by the same filters as
`purge`. Filters are optional: without any, every session is stripped.
Conversation text, tool results, messages, and sessions themselves are
untouched; the `event` table is also left alone (it is an append-only log).

No child expansion is performed: only sessions that match the filters
themselves are stripped.

```json
{
  "dry_run": true,
  "action": "strip-reasoning",
  "filters": { "older_than": null, "subagents": false, "paths": [] },
  "sessions": [ { "id": "ses_...", "reasoning_parts": 33, "reasoning_bytes": 106929 } ],
  "total_sessions": 1,
  "total_reasoning_parts": 33,
  "total_reasoning_bytes": 106929,
  "stripped": false
}
```

After a real run the tool verifies no reasoning parts remain for the
selected sessions and reports an error (exit 2) otherwise.

### `vacuum`

```json
{
  "dry_run": false,
  "db_bytes_before": 8388608,
  "free_pages_before": 120,
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

## Exit codes

| code | meaning |
| --- | --- |
| 0 | success |
| 1 | opencode is running and the command was refused (close opencode and retry) |
| 2 | not found / bad arguments (usage printed) |
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

- `stats`, `doctor`, `project list/show`, `session list/show` are safe while
  opencode runs.
- Running instances are detected by process name (`opencode`,
  `opencode-server`) via the [sysinfo](https://crates.io/crates/sysinfo)
  crate, which works on Linux, macOS, and Windows.
- `delete`, `purge`, `strip-reasoning`, and `vacuum` are **refused while
  opencode runs** (exit 1); close opencode and retry. Deleting a session a
  running instance is using is not safe: the row is not resurrected, later
  writes fail FK checks (new prompts 404, in-flight streams 500), and the TUI
  keeps showing the cached session until refreshed. If detection itself
  fails, guarded commands fail with exit 3 instead of guessing.
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

## License

[MIT](LICENSE)
