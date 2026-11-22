# Command reference

Complete command and option reference for `opencode-dbtool`. See
[README.md](README.md) for installation, configuration, and environment
variables, and [CHANGELOG.md](CHANGELOG.md) for release history.

## Contents

- [Synopsis](#synopsis)
- [Commands](#commands)
- [Global flags](#global-flags)
- [Output and scripting](#output-and-scripting)
- [Interactive mode](#interactive-mode)
- [Inspection](#inspection)
- [Projects](#projects)
- [Sessions](#sessions)
- [KV cache](#kv-cache)
- [Storage cleanup](#storage-cleanup)
- [Backups](#backups)
- [Maintenance](#maintenance)
- [Configuration and utilities](#configuration-and-utilities)
- [Online maintenance (opencode running)](#online-maintenance-opencode-running)
- [Concurrency](#concurrency)
- [Delete semantics](#delete-semantics)
- [Recipes](#recipes)

## Synopsis

```text
opencode-dbtool [GLOBAL OPTIONS] <COMMAND> [OPTIONS]
```

Run `opencode-dbtool --help` for the command list and
`opencode-dbtool <command> --help` for command-specific options.

## Commands

| command | description |
| --- | --- |
| `stats [--detail]` | DB overview: table sizes, totals, storage usage, reclaimable space (`--detail` adds activity/subagent analysis) |
| `tui` | interactive full-screen menu for the common inspection, cleanup, backup, and maintenance actions, with list browsers for sessions, projects, kv, and backups (requires a TTY) |
| `doctor [--fix] [--max-backup-age <age>]` | integrity + consistency checks; `--fix` repairs supported orphans |
| `report [--costs] [--since <age>] [--project <ref>]` | read-only reclaim and cost suggestions |
| `project list [--path <dir>...] [--sort worktree\|size\|updated\|sessions] [--search <text>] [--older-than <age>] [--min-size <size>] [--limit <n>]` | project overview (counts, sizes); list filters match the session/kv/backup conventions |
| `project show <id>` | project detail (sessions, breakdown) |
| `project delete <id>...` | delete project(s) + all related data |
| `project purge [--older-than <age>] [--path <dir>...] [--empty]` | delete projects matching all filters |
| `session list [--sort size\|cost\|updated\|messages] [--limit <n>] [--search <text>] [--min-size <size>] [--older-than <age>] [--project <ref>] [--parent <ref>]` | per-session breakdown (full ids) |
| `session show <id> [--messages [--limit <n>\|--last <n>] [--full] [--markdown [--include-tools]]]` | session detail, optionally with message previews |
| `session search <text> [--limit <n>] [--snippets <n>] [--since <age>]` | search message/part content across sessions (`--limit 0` = no limit) |
| `session delete <id>...` | delete session(s) + cascade |
| `session purge [filters]` | delete sessions matching all filters |
| `session strip-reasoning [filters]` | delete only the reasoning content of matching sessions |
| `session export <id> [--out <file>] [--markdown [--include-tools]]` | export one session (JSON or Markdown) |
| `session export --out-dir <dir> [--older-than <age>] [--project <ref>] [--limit <n>] [--markdown [--include-tools]]` | batch-export sessions |
| `session import <file>` | import a session through the running server |
| `kv list [--sort size\|key\|updated] [--older-than <age>] [--search <text>] [--min-size <size>] [--limit <n>]` | list global kv entries with sizes (large caches first by default) |
| `kv show <key> [--raw]` | show a kv value (truncated; `--raw` prints it verbatim) |
| `kv delete <key>...` | delete kv entries (caches regenerate on demand) |
| `kv purge [--older-than <age>] [--larger-than <size>]` | delete kv entries matching filters |
| `db checkpoint [--truncate]` | checkpoint the WAL; `--truncate` shrinks the file (online) |
| `db path` | show the resolved database path and key pragmas |
| `db optimize` | refresh SQLite query-planner statistics (online) |
| `db query <sql> [--limit <n>]` | run a read-only SELECT/WITH/EXPLAIN query |
| `backup [--keep-backups <n>]` | online verified backup (safe while opencode runs) |
| `backup list [--verify] [--sort date\|size] [--search <text>] [--older-than <age>] [--min-size <size>] [--limit <n>]` | list timestamped backups |
| `backup restore <file>\|--latest` | restore a backup over the current database (bare names resolve in the DB directory; safety copy first) |
| `backup delete <file>...` | delete specific backup files (preview with `--dry-run` first) |
| `backup prune [--keep <n>\|--older-than <age>]` | delete old backups without creating one |
| `service status` | diagnose the running opencode service (API, DB match) |
| `fs clean-snapshots [--project <id>...] [--orphans-only]` | delete snapshot storage, optionally scoped |
| `fs clean-shell [--older-than <age>]` | delete shell output files, optionally only old ones |
| `fs clean-tool-output [--older-than <age>]` | delete truncated tool-output files, optionally only old ones |
| `fs clean-blob-orphans` | delete instruction/media blobs referenced by nothing |
| `fs clean-log [--older-than <age>]` | truncate log/opencode.log, or prune only old lines |
| `fs clean-repos [--older-than <age>]` | delete cached git repositories, optionally only old ones |
| `vacuum [--no-backup] [--keep-backups <n>] [--online] [--into <file>]` | run VACUUM (backup + verify by default; `--online` attempts it while opencode runs; `--into` writes a compacted copy) |
| `cleanup [filters] [--fs-older-than <age>] [--no-backup] [--no-vacuum] [--keep-backups <n>] [--vacuum-online] [--verify]` | backup → optional purge → orphan/file cleanup → VACUUM, in one run |
| `config show` | show effective settings and their source |
| `config path` | print resolved config, data, state, and database paths |
| `completions <shell>` | print a shell completion script (bash, zsh, fish, ...) |
| `self-update [--dry-run\|--yes]` | check for / install the latest GitHub release binary |

All commands accept the global flags described under [Global flags](#global-flags), including
`--format`, `--fields`, `--quiet`, `--absolute`, `--color`/`--no-color`,
`--no-pager`, `--wait-lock`, `--config`, `--dry-run`, `--yes`,
`--no-input`, and `--restart-service`. Every subcommand has its own
`--help`.

The list commands (`project list`, `session list`, `kv list`, `backup list`)
share the same filter conventions: `--sort`, `--limit` (0 = no limit),
`--search`, `--older-than`, and `--min-size` where the item has a size.
`session search` also treats `--limit 0` as no limit.

Filters for `session purge` / `session strip-reasoning` are
`--older-than <age>`, `--subagents`, `--archived`, `--empty`,
`--path <dir>...`, `--path-prefix <dir>...`, `--larger-than <size>`,
`--keep-latest <n>`, and `--keep-latest-per-project <n>` (see
[`session purge`](#session-purge) and
[`session strip-reasoning`](#session-strip-reasoning)).

Session and project references resolve in this order: exact id, a unique id
prefix (for sessions the leading `ses_` may be omitted), then — for projects —
an exact worktree path. An ambiguous prefix, or a worktree shared by several
projects, is refused with the candidate list; use the full id to disambiguate.
`--path` filters stay exact matches: in `purge` / `strip-reasoning` it is an
exact match against the session `directory`; in `project list`/`project purge`
it is an exact match against the project `worktree`. It is never a prefix
match; a trailing slash is ignored on both sides of the comparison.
`--path-prefix` matches a directory and everything below it.

## Global flags

| flag | meaning |
| --- | --- |
| `--dry-run`, `-n` | print actions without changing anything (safe while opencode runs) and show a `to apply:` hint |
| `--yes`, `-y` | confirm a destructive command. On a terminal, omitting `--yes`/`--dry-run` shows the preview and asks `Proceed? [y/N]` instead; off a terminal `--yes` is required for every real run of `delete`, `purge`, `strip-reasoning`, `session import`, `kv delete`/`purge`, `backup`/`restore`/`delete`/`prune`, `doctor --fix`, `fs clean-*`, `vacuum`, `cleanup`, and `self-update` |
| `--no-input` | never prompt: destructive commands then require `--yes`, even on a terminal (useful for CI with a pseudo-TTY) |
| `--format <table\|json\|csv>` | output format (default: table on a terminal, JSON when piped) |
| `--fields <fields>` | table/CSV columns to print, comma-separated (for example `id,title,size_bytes`) |
| `--quiet` | suppress progress and confirmation messages on stderr |
| `--absolute` | absolute ISO timestamps in tables instead of relative ages |
| `--no-color` | disable ANSI colors in tables (also honors `NO_COLOR`) |
| `--color <auto\|always\|never>` | force or disable ANSI colors; `always` also works when piping |
| `--no-pager` | disable the automatic pager on a terminal |
| `--config <file>` | load defaults from this config file |
| `--wait-lock <age>` | wait up to this long for the cross-process maintenance lock instead of failing immediately |
| `--restart-service` | stop the registered opencode service for the run and restart it afterwards (for commands that need exclusive access) |

Command-specific flags are documented with each command, including
`doctor --fix`/`--max-backup-age`, `vacuum --no-backup`/`--online`/`--into`/
`--keep-backups`, and `cleanup --fs-older-than`/`--no-backup`/`--no-vacuum`/
`--vacuum-online`/`--verify`.

Destructive commands refuse to run without `--yes`, `--dry-run`, or an
interactive terminal (exit code 2): use `--dry-run` to preview the impact
before confirming.

## Output and scripting

### Common fields

Commands that print a single result object (for example `stats`, `doctor`,
`report`, deletes/purges, `backup`, `fs clean-*`, `vacuum`, `cleanup`, and
`db checkpoint/path/optimize/query`) start with an environment block.
`project/session list`, `session search`, `backup list`, and `kv list` print
a bare array; `project/session show` and `kv show` print a bare object.
`session export` prints the transfer payload (or a summary when writing
files with `--out`/`--out-dir`), and `session import` prints the server
response:

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
forces the table even when piped; `--format csv` prints comma-separated rows.
`--fields id,title,size_bytes` selects and orders table/CSV columns, and
`--no-pager` disables the automatic pager. The interactive prompt and errors
go to stderr, so stdout stays machine-readable.

### Interactive confirmation

Destructive commands (`session`/`project delete` and `purge`,
`strip-reasoning`, `session import`, `kv delete`/`purge`,
`backup`/`restore`/`delete`/`prune`, `doctor --fix`, `fs clean-*`, `vacuum`,
`cleanup`, `self-update`) work in three modes:

- `--dry-run`: print the preview and change nothing (safe while opencode
  runs).
- `--yes`: execute for real (the running-instance guard applies).
- neither, on a terminal: print the preview, ask `Proceed? [y/N]`, then
  execute or abort.
- neither, not a terminal: exit 2 with a message pointing at both flags.

## Interactive mode

### `tui`

`opencode-dbtool tui` opens a full-screen menu for the same operations as
the CLI. Use `Up`/`Down` (or `j`/`k`) to select an item, `Left`/`Right`
(or `Tab`, or `1`-`9`) to switch sections, and `Enter` to open or run it.
`PgUp`/`PgDn` jump a page in long lists. In command menus `/` searches
every action by label or description; in the list browsers it filters the
list. `?` or `F1` shows a context-sensitive help screen for the current
view. `q` or `Esc` steps back out of a nested list or leaves the menu
(`Ctrl+C` always quits). The header shows the resolved database size,
whether opencode is running, and the registered service pid. Layout uses
terminal cell widths, so Japanese and other wide characters stay aligned
in titles and columns.

The sections cover overview/health, projects, sessions, kv cache, backups,
storage cleanup, database tools, one-shot cleanup, and system commands
(`service status`, `config show/path`, `self-update`).

`Projects`, `Sessions`, `KV cache`, and `Backups` open file-manager-style
lists instead of command menus. Move with `Up`/`Down` (`j`/`k`) or
`PgUp`/`PgDn`; the right pane previews the selected item. Common keys are
`/` (filter), `o` (only items older than an age), `s` (cycle the sort key),
`c` (clear filters), `r` (reload), and `d` (delete the selected item, preview
first).

- Projects (staged browsing): `Enter` opens the selected project's sessions,
  `v` opens the project summary (`project show`), `d` deletes the project,
  `E` purges empty projects, and `P` purges old projects. Inside a
  project's sessions, `u` or `>` drills into a session's subagents, `t`
  toggles top-level sessions only vs all sessions, `v` shows the project
  summary, and `Esc` or `Backspace` steps back one level (restoring your
  previous filters and cursor).
- Sessions: `Enter` opens session messages; `u` or `>` drills into direct
  subagent sessions (`Esc`/`Backspace` steps back); `t` toggles top-level
  sessions only; `F` searches message content; `P` purges old sessions;
  `S` strips reasoning. Subagent rows are tagged `[sub]` and parent rows
  with loaded children show `[+N sub]`.
- KV cache: `Enter` shows the value; `P` purges old entries; `L` purges
  entries larger than a size.
- Backups: `Enter` restores the selected backup; `b` creates a new
  verified backup; `V` toggles integrity verification of the list; `K`
  keeps only the newest N; `P` prunes old backups.

All of those destructive operations use the same preview-and-confirm flow as
the CLI. Delete previews and completion screens render structured summaries
with clear status badges (`[PREVIEW]`, `[DELETED]`, `[NO CHANGES]`,
`[FAILED]`), affected item lists, and table row counts without raw JSON
dumps. After a delete finishes, `Enter` or `Esc` returns to the refreshed
list with the cursor position preserved, and `r` is disabled so completed
deletes cannot be accidentally re-run against removed IDs. The rightmost
session column follows the active sort key (size, cost, or
messages/parts). Press `?` or `F1` for the full key list in any browser.

The other sections are command menus. Actions that need a key, text, age, or
size ask for it in a one-line prompt; leaving the prompt empty uses the shown
default where one exists (`30d` for purges and cleanup, `7d` for file
cleanup, `90d` for projects, `1M` for kv sizes). The prompt supports cursor
movement, `Ctrl+U` (clear), `Ctrl+W` (delete the previous word), and
`Ctrl+K` (delete to the end).

Every action runs the real CLI as a child process with `--no-pager --no-input`
(using structured JSON internally for list browsers and delete reports, and
`--format table` for command views), so the menu cannot bypass the service
guard, maintenance lock, or confirmation rules. Destructive actions are
always previewed with `--dry-run` first; `Enter`/`a` then re-runs the same
command with `--yes`, and `r` runs the preview again. In the result view,
`Up`/`Down`, `PgUp`/`PgDn`, and `Home`/`End` scroll the captured output;
`Enter` (when no preview confirmation is pending) or `Esc` returns to the
menu. `R` refreshes the header status.

`tui --dry-run` makes the whole session preview-only, so no action can be
applied. `--no-input` is rejected (the menu needs the terminal); all other
global flags are passed through to the child commands where applicable.
stdin and stdout must be a terminal, otherwise `tui` exits with code 2.

## Inspection

### `stats`

Database overview: file and WAL sizes, per-table row counts, total data
volume, reclaimable space, the sizes of opencode's filesystem storage,
and the breakdown of `session_message`/`session_part` rows by type. Start
here to see where space is going.

```json
{
  "db_bytes": 8388608,
  "wal_bytes": 0,
  "free_pages": 120,
  "reclaimable_bytes": 491520,
  "tables": { "session_v2": 6, "session_message": 640, "session_part": 1200, "event": 90210, "...": 0 },
  "table_bytes": { "session_message": 900000, "session_part": 700000, "event": 400000, "...": 0 },
  "total_data_bytes": 12345678,
  "storage": { "snapshot_bytes": 736000, "shell_bytes": 39681, "tool_output_bytes": 24000, "repos_bytes": 0, "log_bytes": 2441760 },
  "message_types": { "assistant": { "count": 400, "bytes": 900000 }, "user": { "count": 240, "bytes": 10240 } },
  "part_types": { "text": { "count": 900, "bytes": 600000 }, "reasoning": { "count": 300, "bytes": 90000 } }
}
```

| field | meaning |
| --- | --- |
| `db_bytes` | size of `opencode.db` on disk |
| `wal_bytes` | size of the WAL file |
| `free_pages` | SQLite freelist page count (space that `vacuum` can reclaim) |
| `reclaimable_bytes` | `free_pages` * `page_size`: the bytes a VACUUM would give back |
| `tables` | row count per table |
| `table_bytes` | content bytes per table, largest first |
| `total_data_bytes` | sum of content columns across all tables (`data`, `payload`, `value`, `output`, `initial_values`/`current_values`, ...) |
| `storage` | sizes of the filesystem storage beside the database: `snapshot_bytes` (`snapshot/`), `shell_bytes` (`shell/`), `tool_output_bytes` (`tool-output/`), `repos_bytes` (`repos/`), `log_bytes` (`log/`); missing directories count as 0 |
| `message_types` | `session_message` rows grouped by `type` (largest first) |
| `part_types` | `session_part` rows grouped by `type` (largest first; empty on databases without the table) |

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
before `vacuum`, to confirm the database is healthy. `--fix` applies the
supported repairs (orphan blobs/jobs/automations/events/search index
entries and dangling references) in one pass; combine it with `--dry-run`
to preview. `--max-backup-age <age>` adds an advisory warning when the
newest backup is missing or older than the age.

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
    "orphan_instruction_blobs": [],
    "orphan_media_blobs": [],
    "orphaned_jobs": [],
    "orphaned_automations": [],
    "orphaned_search_documents": [],
    "orphaned_session_rows": []
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
| `orphans.orphan_media_blobs` | `blob` rows referenced by no `blob_ref` (left by a direct delete or interrupted run; reclaim with `fs clean-blob-orphans`) |
| `orphans.orphaned_jobs` | `job` rows whose session or child session is gone (the table has no foreign keys) |
| `orphans.orphaned_automations` | `automation` rows whose `project_id` has no `project` row (automations intentionally keep a missing `session_id`) |
| `orphans.orphaned_search_documents` | `session_search_document` rows whose session or message is gone (the table has no foreign keys) |
| `orphans.orphaned_session_rows` | per-table `{ table, count }` entries for session child tables (`session_message`, `session_part`, `session_inbox`, `instruction_*`, `blob_ref`, legacy `schedule`) whose session is gone |

`orphans` collects references that no FK constraint covers, plus explicit
per-table counts for session child tables whose session is gone (folded
into `ok: false`). `doctor --fix` removes orphan blobs, jobs,
automations, events, and search index entries and clears dangling
references. `ok: false` (exit code 3) when integrity/fk/orphan checks fail.

### `report`

Read-only overview of reclaimable space, old/large sessions, large kv
caches, backups, and suggested commands. `--costs` adds cost totals by
project and by day; `--since <age>` and `--project <ref>` limit all
session-derived sections to that scope. The command never deletes anything.

```json
{
  "db_bytes": 8388608,
  "wal_bytes": 0,
  "reclaimable_bytes": 491520,
  "old_sessions": { "days": 90, "count": 2, "bytes": 1200000 },
  "largest_sessions": [ { "id": "ses_...", "title": "...", "bytes": 500000 } ],
  "kv_candidates": [ { "key": "models-dev:catalog", "bytes": 2000000 } ],
  "backups": { "count": 3, "total_bytes": 25165824 },
  "suggestions": [ { "action": "VACUUM can reclaim 480.0 KB", "command": "opencode-dbtool vacuum --online --yes" } ],
  "scope": { "since": "30d", "project": "p1" },
  "costs": { "total": 1.23, "by_project": [], "by_day": [] }
}
```

`scope` and `costs` are present only when requested. On a terminal the
output is a compact summary table.

### `service status`

One-stop diagnosis of the running service: registration (`registered`,
`pid`, `url`, `version`), API reachability with credentials (`api_ok`,
`api_error`), and whether the service has this database open
(`db_matches`). Works even when the database file is missing. Use it when
deletes unexpectedly fall back to the guard or when filing a bug report.

```json
{
  "opencode_running": true,
  "registered": true,
  "pid": 5406,
  "url": "http://127.0.0.1:49374",
  "version": "2.2.6",
  "db_matches": true,
  "api_ok": true
}
```

### `db path` / `db optimize` / `db query`

- `db path` prints the resolved database path plus `journal_mode`,
  `page_size`, `auto_vacuum`, `busy_timeout`, `user_version`, and `wal_bytes`.
- `db optimize` runs `PRAGMA optimize` to refresh query-planner statistics;
  it is safe while opencode runs.
- `db query <sql> [--limit <n>]` prepares a single read-only `SELECT`,
  `WITH`, or `EXPLAIN` statement and returns `columns`, `rows`, `row_count`,
  `limit`, and `truncated`. `--limit 0` means no limit.

```sh
opencode-dbtool db path
opencode-dbtool db optimize
opencode-dbtool db query "SELECT id, cost FROM session_v2 ORDER BY cost DESC LIMIT 5"
opencode-dbtool db query "SELECT COUNT(*) AS n FROM kv" --format table
```

## Projects

### `project list` / `project show`

Browse projects (worktrees opencode tracks) with their session counts and
data sizes. `project list` prints an array; `project show <ref>` prints one
object plus `session_list`. `<ref>` may be the full project id, a unique id
prefix, or an exact worktree path (when it maps to exactly one project).

`project list --path <dir>` filters the array to projects whose worktree
matches the directory. `--path` is repeatable (OR) and matches **every**
project registered at that directory. `--search <text>` matches the id,
worktree, or name; `--older-than <age>` keeps projects whose latest session
activity is older than the cutoff; `--min-size <size>` keeps projects at
least that large. `--sort` accepts `worktree` (default), `size`, `updated`,
and `sessions`; `--limit` caps the output (0 = no limit). `project purge
--path` deletes all matching directory projects likewise.

```json
{
  "id": "e39d872ab709d3fe972b7de3ce41c025915cf4ed",
  "worktree": "/home/user/work/...",
  "name": "...",
  "sessions": 6,
  "events": 90210,
  "session_messages": 640,
  "session_parts": 1200,
  "blob_bytes": 0,
  "size_bytes": 12345678,
  "cost": 1.2345,
  "updated": "2026-01-01T00:00:00Z"
}
```

`size_bytes` includes session content, `session_part` rows, and distinct
media blob bytes referenced by the project's sessions. `project show`'s
`session_list` entries report `events`, `session_messages`, `session_parts`,
`archived`, and `cost`.

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
`deleted: true` plus a note that the file size only shrinks after `vacuum`
and that `fs clean-blob-orphans` releases orphan blobs.

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

## Sessions

### `session list` / `session show`

Per-session breakdown: event/`session_message`/`session_part`/`inbox`
counts, data size, archived flag, and cost. Use it with `--sort size` and
`--limit` to find the biggest sessions before purging. `--sort` accepts
`size`, `cost`, `updated`, and `messages`; `--min-size <size>` keeps only
sessions at least that large; `--older-than <age>` keeps sessions updated
before the cutoff; `--project <ref>` and `--parent <ref>` restrict the
list to a project or to direct children of a parent session. `--search
<text>` keeps only sessions whose title or directory contains the text
(case-insensitive). `--limit 0` means no limit. `session list` prints an
array; `session show <ref>` prints one object. With `--messages`, previews are
oldest first and limited to 50 by default; `--limit <n>` caps the number
shown, `--last <n>` selects the newest window, and `--full` keeps whole
messages instead of 300-char previews. `--last` cannot be combined with
`--limit`. `--markdown` (requires `--messages` and cannot be combined with
`--limit` or `--last`) renders the conversation from the local database (no
server needed); `--include-tools` (requires `--markdown`) adds tool calls and
other non-text parts to the Markdown. Without `--sort`/`--limit`, `session
list` is ordered by `time_updated` descending. `size_bytes` sums
event+session_message+session_part+inbox+instructions+media blob bytes
(shared blobs count for each session that references them). In tables,
`updated`/`time` columns show relative ages (`15m ago`); `--absolute`
prints ISO timestamps.

`session show --messages` folds `session_part` rows (newer builds) into
the message `content` preview, parts first and legacy inline content
after them, matching how opencode reads a conversation.

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
  "parts": 240,
  "blob_bytes": 0,
  "size_bytes": 2100000,
  "cost": 0.42
}
```

`parent_id` is the parent session id for subagent sessions, `null` for
top-level sessions. It also appears in `project show`'s `session_list`.

### `session search`

Search message and `session_part` content without an index; expect a full
scan on large databases. `--limit <n>` caps the number of sessions
(default 20; 0 = no limit), `--snippets <n>` caps snippets per session
(default 3), and
`--since <age>` limits results to recently updated sessions. The output is a
bare array:

```json
[
  {
    "id": "ses_...",
    "title": "...",
    "directory": "/home/user/work/...",
    "updated": "2026-01-01T00:00:00Z",
    "matches": 2,
    "snippets": [
      {
        "message_id": "msg_...",
        "type": "text",
        "source": "part",
        "time": "2026-01-01T00:00:00Z",
        "snippet": "...matching text..."
      }
    ]
  }
]
```

`source` is `message` for `session_message` rows and `part` for
`session_part` rows.

### `session export` / `session import`

`session export <id>` fetches a session transfer payload from the running
server and prints it (or writes it with `--out <file>`). `--markdown` renders
from the local database instead and needs no server; `--include-tools` adds
non-text parts to the Markdown. Batch export uses `--out-dir <dir>` and can
select sessions with `--older-than <age>`, `--project <ref>`, and `--limit
<n>` (0 = no limit); `--markdown` writes `.md` files instead of JSON, and
`--include-tools` adds non-text parts to each Markdown file. Import uses the
server API (`session import <file>`) and accepts both the current transfer
payload and older `{"data": ...}` wrappers.

```sh
opencode-dbtool session export ses_123 --out session.json
opencode-dbtool session export ses_123 --markdown --include-tools
opencode-dbtool session export --out-dir exports --older-than 90d --limit 20
opencode-dbtool session import session.json
```

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
after `vacuum` and that `fs clean-blob-orphans` releases orphan blobs.
When the opencode server is running against the same database, the
deletes go through its API and the note says so.

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

`--export-dir <dir>` exports every matching session through the running
server before deleting it; it cannot be combined with `--restart-service`.

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

Reasoning lives in three places, all of which are stripped:

| location | handled how |
| --- | --- |
| durable `event` rows (`session.reasoning.started` / `.ended`, plus the legacy `session.next.reasoning.*`) | rows deleted (`.ended` holds the full text; `.delta` is live-only and never persisted) |
| `session_message` assistant `content[]` | `type: "reasoning"` elements removed from the JSON, rows rewritten only when changed |
| `session_part` rows (newer builds) | `type = 'reasoning'` rows deleted |

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
      "reasoning_parts": 4, "reasoning_part_bytes": 70000,
      "messages_rewritten": 2, "rewritten_bytes": 5000 }
  ],
  "total_sessions": 1,
  "total_reasoning_events": 4,
  "total_reasoning_event_bytes": 80000,
  "total_reasoning_parts": 4,
  "total_reasoning_part_bytes": 70000,
  "total_messages_rewritten": 2,
  "total_rewritten_bytes": 5000,
  "stripped": false
}
```

After a real run the tool verifies no reasoning events, message content, or
`session_part` rows remain for the selected sessions and reports an error
(exit 2) otherwise.

## KV cache

### `kv list` / `kv show` / `kv delete` / `kv purge`

Inspect the global `kv` table, which can dominate database size (e.g. the
multi-megabyte `models-dev:catalog` cache). `kv list [--sort size|key|updated]
[--older-than <age>] [--search <text>] [--min-size <size>] [--limit <n>]`
prints `{key, bytes, updated}` largest first by default; `kv show <key>`
prints the value truncated to 2000 chars plus `truncated` (`--raw` prints it
verbatim with no wrapper or truncation); `kv delete <key>...` removes keys
(caches regenerate on demand) and is guarded while opencode runs. `kv purge`
requires `--older-than <age>` and/or `--larger-than <size>`, then removes
every matching key in one pass. Preview all three with `--dry-run` first.
All list commands use `--limit 0` for "no limit".

## Storage cleanup

### `fs clean-snapshots`

Delete snapshot storage (git object packs used for undo/redo), optionally
scoped: `--project <id>` (repeatable) limits deletion to those projects,
`--orphans-only` deletes only directories whose project no longer exists
(e.g. left behind by `project delete`, which removes rows but not files).
Without filters everything is deleted. Full deletion destroys revert history
and is **refused while opencode runs** (exit 1); `--orphans-only` is safe
online.

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
afterwards. The `--older-than` form is safe online; a full deletion is
guarded while opencode runs because live runs append to these files.

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

### `fs clean-tool-output`

Delete truncated tool-output files (`tool-output/tool_*`). With
`--older-than <age>` only files not modified since the cutoff are
removed; without it everything goes. Safe while opencode runs (opencode
retains these files for 7 days and prunes them itself).

```json
{
  "dry_run": true,
  "dir": "/path/tool-output",
  "files": [ { "file": "tool_1a2b3c4d5e6f", "bytes": 51200 } ],
  "total_files": 3,
  "total_bytes": 153600,
  "deleted": false
}
```

### `fs clean-blob-orphans`

Delete content-addressed blobs referenced by nothing: `instruction_blob`
rows referenced by no `instruction_state`, and `blob` media rows
referenced by no `blob_ref`. Instruction blobs leak when rows are deleted
directly (the states cascade but the shared blobs have no owning FK);
media blobs are left behind by direct deletes (and an interrupted run).
Run it after `purge`/`delete` runs. Safe online: each delete re-checks
references inside its transaction, so a concurrent opencode write is
never collected.

```json
{
  "dry_run": true,
  "orphans": [
    { "hash": "d43ac7...", "bytes": 1234, "kind": "instruction" },
    { "hash": "9a12ef...", "bytes": 204800, "kind": "media" }
  ],
  "total_blobs": 2,
  "total_bytes": 206034,
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

### `fs clean-repos`

Delete cached git repositories under `repos/` (entries are directories with a
`.git` directory). `--older-than <age>` keeps entries whose newest file is
newer than the cutoff. It is refused while opencode runs.

```json
{
  "dry_run": true,
  "dir": "/path/repos",
  "entries": [ { "path": "github.com/owner/repo@main", "bytes": 1048576 } ],
  "total_entries": 1,
  "total_bytes": 1048576,
  "deleted": false
}
```

## Backups

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

`backup list [--verify] [--sort date|size] [--search <text>]
[--older-than <age>] [--min-size <size>] [--limit <n>]` lists the
timestamped backups (newest first by default) with size and creation time;
`--verify` runs an integrity check on each. A bare file name in `backup
restore`/`backup delete` is resolved in the database directory, so the names
printed by `backup list` can be used directly.

`backup restore <file>` (or `--latest`) verifies the source, takes a safety
backup of the current database (best effort, so a corrupt current database
can still be recovered), removes stale WAL/SHM files, replaces the database,
and verifies the result. It needs exclusive access (or `--restart-service`)
and `--dry-run` reports the plan without touching anything. `backup delete
<file>...` removes selected backup files (only files matching this
database's backup prefix in its directory); `backup prune` removes old
backups without creating a new one and requires `--keep <n>` or
`--older-than <age>`. Both deletion commands preview with `--dry-run` first.

```sh
opencode-dbtool backup list --verify
opencode-dbtool backup list --sort size --limit 5
opencode-dbtool backup delete opencode.db.backup-20260830T120000Z --dry-run
opencode-dbtool backup prune --keep 3 --dry-run
opencode-dbtool backup restore --latest --yes
opencode-dbtool backup restore opencode.db.backup-20260830T120000Z --yes
```

## Maintenance

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

`--into <file>` writes the compacted database to a separate file instead of
vacuuming in place; it is safe while opencode runs and cannot be combined
with `--no-backup`, `--keep-backups`, or `--online`.

With `--keep-backups <n>` (at least 1), a `backup_cleanup` block is
added on a real run (dry-run reports nothing since no backup is touched):

```json
{ "backup_cleanup": { "kept": 3, "removed_files": 7, "removed_bytes": 52428800 } }
```

### `db checkpoint`

Checkpoint the WAL into the database file. PASSIVE (the default) never
blocks; `--truncate` additionally shrinks `opencode.db-wal` when no
connection holds a read snapshot. `--dry-run` reports the plan without
running the pragma. Both modes are safe while opencode runs and report
the lock state instead of failing:

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
3. `fs clean-blob-orphans` (instruction and media blobs);
4. `fs clean-snapshots --orphans-only`;
5. `fs clean-shell --older-than <age>` and `fs clean-tool-output
   --older-than <age>` (default `7d`, change with `--fs-older-than`);
6. `fs clean-log --older-than <age>` (same cutoff);
7. `vacuum` (skipped with `--no-vacuum`; no second backup is taken because
   step 1 already backed up the pre-change database). **While opencode
   runs, the final VACUUM is skipped** with a note, because it needs the
   write lock unless `--vacuum-online` is passed; run
   `opencode-dbtool vacuum --online` afterwards or stop the service. The
   purge step is routed through the server API, so a `cleanup` with session
   filters is safe online. `cleanup --restart-service` stops the service
   first instead: the purge then uses the direct database path and the final
   VACUUM runs too. `--verify` runs `doctor` after a real run and embeds its
   result as `verify`/`summary.verify_ok`.

All `session purge` filters are accepted directly. `--keep-backups <n>`
prunes older backups after the pre-run backup. The output embeds each step's
result (without nested environment blocks) plus `action`, `filters`,
`fs_older_than`, `cleaned`, and a `note`. A compact `summary` object
aggregates the run (sessions/rows, backup path, orphan counts, file bytes,
log before/after, vacuum status, database size before/after), and on a
terminal the table output is exactly that summary instead of the nested
step dump. `--dry-run` prints the plan without changing anything and adds
a `to apply:` hint with the confirmed command line.

```sh
opencode-dbtool cleanup --older-than 30d --subagents --keep-latest-per-project 5
```

## Configuration and utilities

### `config show` / `config path`

`config show` prints the effective settings (after merging the config file and
command-line flags) plus `config_file`, `config_file_exists`, and `source`
(`file` or `defaults`). `config path` prints the resolved config, data, and
state directories, the selected database, `db_exists`, and the relevant
environment variables present in the process.

```sh
opencode-dbtool config show
opencode-dbtool config path
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
  "current_version": "0.5.0",
  "latest_version": "0.5.0",
  "target": "x86_64-unknown-linux-musl",
  "update_available": true,
  "updated": false,
  "dry_run": true,
  "status": "update-available",
  "path": "/home/user/.cargo/bin/opencode-dbtool",
  "release_url": "https://github.com/orrisroot/opencode-dbtool/releases/tag/v0.5.0"
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

## Online maintenance (opencode running)

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
- **`fs clean-shell --older-than`**, **`fs clean-tool-output --older-than`**,
  **`fs clean-snapshots --orphans-only`**, and **`fs clean-blob-orphans`**
  are safe online.
- **`report`, `session search`, `db path`, `db optimize`, and `db query`**
  read the database without modifying it.
- **`backup delete`/`backup prune`** only remove backup files, and
  **`vacuum --into`** writes a separate compacted copy, so all are safe
  online.
- **`session export`** reads through the server API (or the local database
  for `--markdown`); **`session import`** uses the server API.
- **`vacuum --online`** attempts a VACUUM while opencode runs with a long
  busy timeout; the server may block writes for the duration, and the
  command fails cleanly if it cannot win the lock.
- **`--restart-service`** (global flag) stops the registered service
  through its own `opencode service stop`, runs the command, and starts it
  again afterwards — even if the command fails. Use it for the commands
  that still need exclusive access (`doctor --fix`, `project
  delete/purge`, `session strip-reasoning`, `backup restore`, `kv
  delete`/`purge`, `fs clean-log`, `fs clean-repos`, full `fs
  clean-snapshots`/`clean-shell`, `vacuum`, and `cleanup` including its
  VACUUM). With no registered service the flag falls back to the normal
  running-instance guard, so it is safe to pass unconditionally. A running
  TUI may need to be restarted if the service comes back on a different
  port.
- `doctor --fix`, `project delete/purge`, `session strip-reasoning`,
  `backup restore`, `kv delete`/`purge`, `fs clean-log`, `fs clean-repos`,
  `fs clean-snapshots` without `--orphans-only`, and `fs clean-shell`
  without `--older-than` still require stopping opencode (exit 1) unless
  `--restart-service` is passed.

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
under `$XDG_STATE_HOME/opencode` (`~/.local/state/opencode`). API routing
is Linux-only: the server is used only after `/proc/<pid>/fd` confirms it
has the target database open. On other platforms the delete commands keep
the normal running-instance guard; use `--restart-service` or stop
opencode.

## Concurrency

opencode runs SQLite in WAL mode, and this tool opens the database with a
matching busy timeout, so concurrent reads never wedge.

- `stats`, `doctor`, `report`, `project list/show`, `session list/show`,
  `session search`, `session export` (API or local Markdown), `kv
  list/show`, `db path`, and `config show/path` read-only commands are safe
  while opencode runs. `db optimize`, `backup delete`, and `backup prune`
  are also safe.
- Running instances are detected by process name, executable path, and
  command line (`opencode`, `opencode-server`, `opencode2`) via the
  [sysinfo](https://crates.io/crates/sysinfo) crate, which works on
  Linux, macOS, and Windows. Command-line matching only accepts
  invocations (e.g. `/usr/bin/opencode`), not references to opencode's
  data files.
- Safe online: `backup`, `backup delete`, `backup prune`, `db checkpoint`,
  `db optimize`, `vacuum --into`, `fs clean-shell --older-than`,
  `fs clean-tool-output --older-than`, `fs clean-snapshots --orphans-only`,
  `fs clean-blob-orphans`, `session import` (server API), and `session
  delete`/`purge` (also the purge step of `cleanup`) when the running
  server's API is available — see "Online maintenance" above.
- Still refused while opencode runs (exit 1): `doctor --fix`, `project
  delete/purge`, `session strip-reasoning`, `backup restore`, `kv
  delete`/`purge`, `fs clean-log`, `fs clean-repos`, `fs
  clean-snapshots` without `--orphans-only`, `fs clean-shell` without
  `--older-than`, and `vacuum` without `--online` or `--into`.
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
- `job` rows for the target session or its child sessions are deleted
  explicitly: the table has no foreign keys. Project deletes also delete
  `automation` rows and project-level event aggregates (`event` /
  `event_sequence`).
- Direct deletes leave orphan `instruction_blob`/`blob` rows for
  `fs clean-blob-orphans` (also run by `cleanup`). The server API path
  releases media blobs itself.
- The `session_v2` / `project` row is deleted; `session_message`,
  `session_part`, `session_inbox`, `blob_ref`, `instruction_*`
  and the other project-scoped tables follow via `ON DELETE CASCADE` (with
  optional `session_search*` entries cleaned up explicitly too).
- After commit the tool verifies the rows are gone and reports an error
  (exit 2) if not.

When a matching opencode server is running (same database file), `session
delete`/`purge` use `DELETE /api/session/<id>` instead, deepest children
first. The server removes the same rows (including `event`/`event_sequence`,
`job`, and released media blobs) and keeps its caches consistent; the tool
verifies the result the same way.

Child sessions (subagent sessions) carry a `parent_id` pointing at their
parent. Deleting a parent session also deletes its children recursively
(nested subagents included); they appear in the output's `sessions` array
like any other target. `doctor`'s `sessions_missing_parent` still detects
children whose parent is missing for other reasons.

`strip-reasoning` runs in its own immediate transaction (foreign keys on)
that deletes matching reasoning `event` and `session_part` rows, rewrites
`session_message` rows when needed, then verifies the result.

## Recipes

```sh
# Where is the space going, and what can be reclaimed?
opencode-dbtool stats --detail
opencode-dbtool report

# Biggest sessions first, and search their content
opencode-dbtool session list --sort size --limit 10
opencode-dbtool session search "TODO" --since 30d

# Export a session for sharing, or import one sent to you
opencode-dbtool session export ses_123 --markdown --include-tools
opencode-dbtool session import session.json

# Show the effective configuration and resolved paths
opencode-dbtool config show
opencode-dbtool config path

# Is the online machinery healthy?
opencode-dbtool service status

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

# Backups
opencode-dbtool backup list --verify
opencode-dbtool backup restore opencode.db.backup-20260830T120000Z --yes

# Inspect a huge cache key
opencode-dbtool kv list
opencode-dbtool kv show models-dev:catalog
opencode-dbtool kv show models-dev:catalog --raw | jq .

# Shell completions
source <(opencode-dbtool completions bash)
```
