# opencode-dbtool

Maintenance tool for the [opencode](https://opencode.ai) 2.x SQLite database
(`~/.local/share/opencode/opencode.db`): inspect sizes and integrity, clean up
sessions/projects/kv caches, and manage backups.

Written in Rust. Output is a human-readable table on a terminal, JSON when
piped, and CSV with `--format csv`. `--format table|json|csv` forces a format;
`--fields` selects table/CSV columns. Destructive commands preview their
impact and ask `Proceed? [y/N]` on a terminal; `--dry-run` and `--yes` keep
them scriptable. Supports Linux, macOS, and Windows.

- [Command reference](COMMANDS.md) — complete command syntax, filters, output
  shapes, and examples
- [Release history](CHANGELOG.md)
- [Scheduled maintenance templates](contrib/README.md)

## Compatibility

Requires the opencode 2.x database schema (`session_v2`, `session_message`,
`session_inbox`, `instruction_*`, `worktree`, `event`, ...). V1 databases
are not supported and fail with exit code 3; open the DB once with
opencode 2.x so its migrations run, then retry.

Current releases track the latest opencode 2.x schema (`account*`,
`credential`, `machine`, `automation`, `session_part`, `blob`/`blob_ref`,
`job`, and optional `session_search*` full-text index tables). Databases
opened by older 2.x builds that lack newer optional tables still work;
their sizes report zero for the missing storage. Session output reports
`parts`/`session_parts` for the newer content rows.

## Install

Prebuilt binaries (Linux musl, macOS, Windows; arm64 + x86_64) are attached to
each [GitHub Release](https://github.com/orrisroot/opencode-dbtool/releases),
pushed as a `v<version>` tag matching `Cargo.toml`.

## Build

```sh
cargo build --release   # -> target/release/opencode-dbtool
```

## Quick start

Check the database and the running service:

```sh
opencode-dbtool stats --detail
opencode-dbtool doctor
opencode-dbtool service status
```

Find large sessions and preview a retention policy:

```sh
opencode-dbtool session list --sort size --limit 10
opencode-dbtool session purge --older-than 30d --keep-latest-per-project 5 --dry-run
```

Apply the policy, then reclaim the freed space:

```sh
opencode-dbtool session purge --older-than 30d --keep-latest-per-project 5
opencode-dbtool vacuum --online
```

For a single guarded run that also removes orphaned blobs and old files, use
`cleanup`:

```sh
opencode-dbtool cleanup --older-than 30d --subagents
```

## Command overview

| area | commands |
| --- | --- |
| Inspect | `stats`, `doctor`, `report`, `service status`, `db path` |
| Browse | `project list`/`project show`, `session list`/`session show`/`session search`, `kv list`/`kv show`, `backup list` |
| Clean | `project delete`/`purge`, `session delete`/`purge`, `session strip-reasoning`, `kv delete`/`purge`, `fs clean-*` |
| Maintain | `backup`, `backup restore`/`backup prune`, `vacuum`, `db checkpoint`/`db optimize`/`db query`, `cleanup` |
| Configure | `config show`/`config path`, `completions`, `self-update` |
| Interactive | `tui` |

All commands accept the [global flags](COMMANDS.md#global-flags). Run
`opencode-dbtool <command> --help` for built-in help. Full syntax, filters,
JSON output, and examples are in the [command reference](COMMANDS.md).

## Output and scripting

On a terminal, results render as tables; when stdout is piped, the same
commands emit JSON; `--format table|json|csv` forces a format. `--fields`
selects table/CSV columns, `--absolute` prints ISO timestamps instead of
relative ages, and `--no-pager` disables the automatic pager. The interactive
prompt and errors go to stderr, so stdout stays machine-readable.

Result-object commands such as `stats`, `doctor`, `vacuum`, and `cleanup`
start with an environment block that reports `opencode_running`, `pids`, and
the resolved `db` path. If the running process cannot be determined, the
block carries `"opencode_running": null`, `"pids": null`, and `pid_error`.
See [Common fields](COMMANDS.md#common-fields) and
[Output formats](COMMANDS.md#output-formats) for details.

## Safety and online use

Destructive commands (`delete`, `purge`, `strip-reasoning`, `session import`,
`kv delete`/`purge`, backup restore/delete/prune, `doctor --fix`, `fs clean-*`,
`vacuum`, `cleanup`, and `self-update`) preview their impact. On a terminal
they ask `Proceed? [y/N]`; without a terminal, pass `--yes`. `--dry-run`
always previews without changing anything and is safe while opencode runs.

Read-only commands, verified backups, WAL checkpoints, and most storage
cleanup steps work while opencode runs. Commands that need exclusive access
refuse to run (exit code 1) unless you stop opencode or pass
`--restart-service`. Run `opencode-dbtool service status` to inspect the
running service. The complete online/offline matrix is in
[Online maintenance](COMMANDS.md#online-maintenance-opencode-running) and
[Concurrency](COMMANDS.md#concurrency).

## Configuration

Optional defaults can live in `config.toml`:

```toml
# $XDG_CONFIG_HOME/opencode-dbtool/config.toml
format = "table"          # or "json" / "csv"
quiet = false
restart_service = true    # as if --restart-service were always passed
no_input = false
fs_older_than = "14d"     # cleanup's shell/tool-output/log cutoff

[purge]                   # defaults for session purge / strip-reasoning / cleanup
older_than = "30d"
subagents = true
archived = false
empty = false
# larger_than = "50M"
# keep_latest = 10
keep_latest_per_project = 5
# path = ["/home/user/work/a"]
# path_prefix = ["/home/user/work"]
```

Command-line flags always win over config values. Use `--config <file>` or
`$OPENCODE_DBTOOL_CONFIG` for a different location. A malformed config (or
a missing file passed explicitly) is a usage error; a missing file at the
standard location is ignored.

## Environment

| variable | meaning |
| --- | --- |
| `OPENCODE_DATA_DIR` | override data dir |
| `OPENCODE_DB` | override database path (same rules as opencode: `:memory:` and absolute paths as-is, relative resolves against the data dir) |
| `OPENCODE_DISABLE_CHANNEL_DB` | skip the channel-database fallback and always target `opencode.db` |
| `XDG_DATA_HOME` | data dir defaults to `$XDG_DATA_HOME/opencode` |
| `XDG_STATE_HOME` | state dir for service discovery defaults to `$XDG_STATE_HOME/opencode` (`~/.local/state/opencode`) |
| `XDG_CONFIG_HOME` | config file defaults to `$XDG_CONFIG_HOME/opencode-dbtool/config.toml` (`~/.config/...`) |
| `OPENCODE_DBTOOL_CONFIG` | explicit config file path (same as `--config`) |
| `NO_COLOR` | disable colors in table output (same as `--no-color`) |
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

The filesystem storage (`snapshot/`, `shell/`, `tool-output/`, `repos/`,
`log/`) always follows the data dir, never the database file's location —
matching opencode, which keeps these directories in the data dir even when
`OPENCODE_DB` points elsewhere.

## Exit codes

| code | meaning |
| --- | --- |
| 0 | success |
| 1 | opencode is running and the command is not safe online (see [Online maintenance](COMMANDS.md#online-maintenance-opencode-running); `--dry-run` still works) |
| 2 | not found / bad arguments |
| 3 | database or network error, or running-process detection failed |

## Scheduling

`contrib/` contains systemd (Linux) and launchd (macOS) templates for
running `cleanup --restart-service --yes` on a schedule, plus installation
notes. See [contrib/README.md](contrib/README.md).

## License

[MIT](LICENSE)
