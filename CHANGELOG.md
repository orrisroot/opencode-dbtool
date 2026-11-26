# Changelog

All notable changes to `opencode-dbtool` are documented in this file.

## [0.5.1] - 2026-11-27

### Added

- TUI: `Ctrl+P` command palette with section jumps, numbered section tabs, and adaptive footer hints that stay readable on narrow terminals.
- TUI: header shows database and WAL sizes and summarizes multiple opencode pids.

### Changed

- TUI: `R` refreshes the header and reloads all browsers; session and project detail panes omit empty optional fields.
- TUI: destructive previews require `a` or `y`; `Enter` and `Esc` cancel, so a double Enter cannot trigger a deletion.
- TUI: cancelling a prompt opened from the command palette restores the query.
- Updated direct dependencies to current releases (`rusqlite` 0.43, `sysinfo` 0.40, `ureq` 3.5, `toml` 1.2, `clap` 4.6.10, `clap_complete` 4.6.12, `self_update` 1.3.3) and refreshed `Cargo.lock`.
- Refreshed the README and command reference for the current schema, CLI, and TUI behavior.

### Fixed

- TUI: narrow tab bars keep the active section visible, long database paths keep the opencode status visible, and held apply keys are ignored on terminals that report key repeat.
- TUI: `R` preserves specific reload messages, such as a project that disappeared.

## [0.5.0] - 2026-11-21

### Added

- Interactive full-screen `tui` command with sections for overview/health, projects, sessions, kv cache, backups, storage, database tools, cleanup, and system commands.
- Staged project-to-session navigation (`Projects -> Sessions -> Subagents`), `[sub]` / `[+N sub]` indicators, top-level filtering (`t`), searchable action menu (`/`), and context-sensitive help (`?` / `F1`).
- Structured delete preview and completion views in the TUI with status badges (`[PREVIEW]`, `[DELETED]`, `[NO CHANGES]`, `[FAILED]`), item breakdowns, and table row counts.
- Support for current opencode 2.x schema tables across `doctor`, `project`, `session`, and `stats`: `automation`, `machine`, `account` / `account_state` / `control_account` / `credential`, and optional FTS5 `session_search` / `session_search_document` (`compact` and external-content `text` layouts).
- Explicit orphan diagnostics in `doctor` (`orphans.orphaned_automations`, `orphans.orphaned_search_documents`, `orphans.orphaned_session_rows`) and repair counters in `doctor --fix` (`fix.automations_deleted`, `fix.search_documents_deleted`).
- Standalone `CHANGELOG.md` separated from [README.md](README.md).

### Changed

- `project delete` and `project purge` now delete `automation` rows, project-level `event` / `event_sequence` aggregates, and unlinked `session_search_document` entries, and verify post-delete removal across them.
- `stats` measures `automation` content columns and `session_search_document` text columns while excluding virtual `session_search` double-counting.
- Diagnostics and previews use `automation` instead of the removed `schedule` table while retaining optional checks for legacy `schedule` tables when present.
- Simplified empty purge filter handling in `project purge`.

### Fixed

- Fixed `doctor` reporting `ok: false` with empty orphan arrays when session child tables contained orphaned rows, added `session_part.message_id` orphan detection, and added repair support for unlinked `automation` and `session_search_document` entries.
- Fixed `NOT IN` NULL-handling pitfalls and the 1000-row cap on `event_sequences_deleted` dry-run counts in `doctor`.
- Fixed WAL path (`<db>-wal`) and parent directory (`db_dir`) resolution for non-`.db` filenames and bare relative paths across `doctor`, `vacuum`, `stats`, `report`, `db path`, and `db checkpoint`, along with Windows CI filesystem space test compatibility.

## [0.4.0] - 2026-11-08

### Added

- Resident-service maintenance: `service status`, API-routed session deletes, and the global `--restart-service` flag.
- Backup management: `backup list --verify/--sort`, `backup restore`, `backup prune`, and `vacuum --into`.
- Diagnostics and planning: `doctor --fix`, `doctor --max-backup-age`, `report` with costs and scoped filters, `db path`, `db optimize`, and `db query`.
- Workflow controls: one-shot `cleanup` with summaries, `cleanup --verify`, `cleanup --vacuum-online`, `config show`/`config path`, and `--wait-lock`.
- Session tools: `session search`, JSON/Markdown/batch `session export`, `session import`, and project/parent filters for session listings.
- Output controls: CSV output, `--fields`, `--color`, `--no-pager`, and shell completions.
- Storage tools: `kv purge`, `kv list --search`, `fs clean-repos`, and `fs clean-tool-output`.
- opencode 2.2 schema support for `session_part`, `blob`/`blob_ref`, `schedule`, and `job`, including part types, media blob sizes, tool-output storage, and reasoning stripping of current events and parts.

### Changed

- Reworked the CLI for interactive and scriptable use; `--dry-run` previews include a copyable apply command.
- Session sizes and project sizes include `session_part` and referenced media blob bytes.
- Session JSON reports `parts` and `blob_bytes`; project JSON reports `session_parts` and `blob_bytes`.
- Direct deletes leave orphan blob rows for `fs clean-blob-orphans` (also run by `cleanup`); the server API path releases media blobs itself.
- Expanded docs and added scheduling templates under `contrib/`.

### Removed

- The session `pending` field and use of the removed `session_pending` table.

### Fixed

- Hardened backup restore, made backup names unique within a second, and added maintenance locking and disk-space checks.
- Addressed review findings across search, costs, cleanup, restart handling, and output formats.
- Zero-match session/project purges no longer delete unrelated orphan media blobs; direct deletes leave blob cleanup to `fs clean-blob-orphans`.
- Fixed formatting and lint failures in CI.

## [0.2.0] - 2026-10-03

### Added

- opencode 2.x database schema support (`session_v2`, `event`, instruction, and workspace tables).
- `OPENCODE_DB` and channel-database resolution.
- Detection of the `opencode2` beta executable.

### Removed

- opencode 1.x database support.

## [0.1.1] - 2026-09-05

### Added

- `self-update` from GitHub releases.

### Fixed

- Correctness, performance, and safety hardening, including database connection cleanup.

## [0.1.0] - 2026-09-01

### Added

- Initial release with `stats`, `doctor`, `project`, `session` (`list`, `show`, `delete`, `purge`, `strip-reasoning`), `fs` cleanup subcommands, and verified `backup` / `vacuum`.
- Automated GitHub release workflow for prebuilt binaries.
