//! Interactive full-screen menu for the common maintenance operations.
//!
//! The menu is a thin front end: every action is re-run as
//! `opencode-dbtool <command>` in a child process, so validation, service
//! guards, locking, and output formatting stay in one place. Destructive
//! actions are always previewed with `--dry-run` first and only re-run with
//! `--yes` after an explicit confirmation. A global `--dry-run` makes the
//! whole session preview-only.

use crate::cli::{Cli, ColorWhen};
use crate::error::{AppError, Result};
use crate::output;
use crate::sys;
use crate::util::{human_age, now_ms, parse_age_ms, parse_iso_utc, parse_size_bytes, shell_quote};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use serde_json::Value;
use std::cell::Cell;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::Instant;

// ---------------------------------------------------------------------------
// action catalogue
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Safety {
    /// Does not change the database or storage.
    ReadOnly,
    /// Writes state but is safe to run directly (checkpoint, optimize, ...).
    Writes,
    /// Destructive: preview with `--dry-run`, then confirm before applying.
    Confirm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputKind {
    Text,
    Age,
    Size,
}

#[derive(Clone, Copy)]
struct InputSpec {
    prompt: &'static str,
    default: &'static str,
    hint: &'static str,
    kind: InputKind,
}

#[derive(Clone, Copy)]
struct Action {
    label: &'static str,
    description: &'static str,
    args: &'static [&'static str],
    input: Option<InputSpec>,
    safety: Safety,
}

#[derive(Clone, Copy)]
enum CategoryKind {
    Actions(&'static [Action]),
    Sessions,
    Projects,
}

struct Category {
    name: &'static str,
    short: &'static str,
    kind: CategoryKind,
}

const fn text_input(prompt: &'static str, hint: &'static str) -> Option<InputSpec> {
    Some(InputSpec {
        prompt,
        default: "",
        hint,
        kind: InputKind::Text,
    })
}

const fn age_input(default: &'static str) -> Option<InputSpec> {
    Some(InputSpec {
        prompt: "Older than",
        default,
        hint: "age such as 30d, 12h, or 2w",
        kind: InputKind::Age,
    })
}

const fn size_input(default: &'static str) -> Option<InputSpec> {
    Some(InputSpec {
        prompt: "Larger than",
        default,
        hint: "size such as 1M or 500K",
        kind: InputKind::Size,
    })
}

const fn action(
    label: &'static str,
    description: &'static str,
    args: &'static [&'static str],
    input: Option<InputSpec>,
    safety: Safety,
) -> Action {
    Action {
        label,
        description,
        args,
        input,
        safety,
    }
}

const CATEGORIES: &[Category] = &[
    Category {
        name: "Overview",
        short: "Overview",
        kind: CategoryKind::Actions(&[
            action(
                "Database overview",
                "Show table sizes, totals, storage usage, and reclaimable space.",
                &["stats"],
                None,
                Safety::ReadOnly,
            ),
            action(
                "Overview with activity",
                "Add 30-day activity and subagent shares to the overview.",
                &["stats", "--detail"],
                None,
                Safety::ReadOnly,
            ),
            action(
                "Reclaim suggestions",
                "Read-only suggestions for sessions, projects, and storage that can be removed.",
                &["report"],
                None,
                Safety::ReadOnly,
            ),
            action(
                "Cost report",
                "Add cost aggregates by project and day to the reclaim report.",
                &["report", "--costs"],
                None,
                Safety::ReadOnly,
            ),
            action(
                "Health check",
                "Run integrity and consistency checks without changing anything.",
                &["doctor"],
                None,
                Safety::ReadOnly,
            ),
            action(
                "Repair what doctor can fix",
                "Remove orphan blobs/jobs/events and dangling parent/fork/workspace references.",
                &["doctor", "--fix"],
                None,
                Safety::Confirm,
            ),
        ]),
    },
    Category {
        name: "Sessions",
        short: "Sessions",
        kind: CategoryKind::Sessions,
    },
    Category {
        name: "Projects",
        short: "Projects",
        kind: CategoryKind::Projects,
    },
    Category {
        name: "KV cache",
        short: "KV",
        kind: CategoryKind::Actions(&[
            action(
                "List cache entries",
                "List global kv entries with sizes, largest first.",
                &["kv", "list"],
                None,
                Safety::ReadOnly,
            ),
            action(
                "Show an entry",
                "Show a kv value (truncated to keep the output readable).",
                &["kv", "show"],
                text_input("Key", "exact kv key"),
                Safety::ReadOnly,
            ),
            action(
                "Delete an entry",
                "Delete one kv entry; caches regenerate on demand.",
                &["kv", "delete"],
                text_input("Key", "exact kv key"),
                Safety::Confirm,
            ),
            action(
                "Purge entries older than",
                "Delete kv entries that have not been updated since the cutoff.",
                &["kv", "purge", "--older-than"],
                age_input("30d"),
                Safety::Confirm,
            ),
            action(
                "Purge entries larger than",
                "Delete kv entries above a size threshold.",
                &["kv", "purge", "--larger-than"],
                size_input("1M"),
                Safety::Confirm,
            ),
        ]),
    },
    Category {
        name: "Storage",
        short: "Storage",
        kind: CategoryKind::Actions(&[
            action(
                "Clean orphan snapshots",
                "Delete snapshot directories whose project no longer exists (safe online).",
                &["fs", "clean-snapshots", "--orphans-only"],
                None,
                Safety::Confirm,
            ),
            action(
                "Delete all snapshots",
                "Delete undo/redo snapshot storage for every project (needs opencode closed).",
                &["fs", "clean-snapshots"],
                None,
                Safety::Confirm,
            ),
            action(
                "Clean shell output",
                "Delete old shell output files.",
                &["fs", "clean-shell", "--older-than"],
                age_input("7d"),
                Safety::Confirm,
            ),
            action(
                "Clean tool output",
                "Delete old truncated tool-output files.",
                &["fs", "clean-tool-output", "--older-than"],
                age_input("7d"),
                Safety::Confirm,
            ),
            action(
                "Prune the log",
                "Drop log lines older than the cutoff instead of truncating the whole log.",
                &["fs", "clean-log", "--older-than"],
                age_input("7d"),
                Safety::Confirm,
            ),
            action(
                "Clean repository caches",
                "Delete cached git repositories older than the cutoff.",
                &["fs", "clean-repos", "--older-than"],
                age_input("7d"),
                Safety::Confirm,
            ),
            action(
                "Clean orphan blobs",
                "Delete instruction/media blob rows that nothing references.",
                &["fs", "clean-blob-orphans"],
                None,
                Safety::Confirm,
            ),
        ]),
    },
    Category {
        name: "Database",
        short: "Database",
        kind: CategoryKind::Actions(&[
            action(
                "Database path and pragmas",
                "Show the resolved database path and the key SQLite pragmas.",
                &["db", "path"],
                None,
                Safety::ReadOnly,
            ),
            action(
                "Checkpoint the WAL",
                "Run a PASSIVE WAL checkpoint (safe while opencode runs).",
                &["db", "checkpoint"],
                None,
                Safety::Writes,
            ),
            action(
                "Checkpoint and shrink the WAL",
                "Truncate the WAL file after checkpointing (needs no active readers).",
                &["db", "checkpoint", "--truncate"],
                None,
                Safety::Writes,
            ),
            action(
                "Optimize query statistics",
                "Run PRAGMA optimize to refresh the query planner (safe online).",
                &["db", "optimize"],
                None,
                Safety::Writes,
            ),
            action(
                "Run a read-only query",
                "Run a single SELECT, WITH, or EXPLAIN statement.",
                &["db", "query", "--limit", "100"],
                text_input("SQL", "read-only SQL statement"),
                Safety::ReadOnly,
            ),
            action(
                "Vacuum the database",
                "Back up, verify, and VACUUM the database in place (needs exclusive access).",
                &["vacuum"],
                None,
                Safety::Confirm,
            ),
            action(
                "Vacuum while opencode runs",
                "Attempt a VACUUM without stopping opencode; may block the server briefly.",
                &["vacuum", "--online"],
                None,
                Safety::Confirm,
            ),
        ]),
    },
    Category {
        name: "Backups",
        short: "Backups",
        kind: CategoryKind::Actions(&[
            action(
                "Create a backup",
                "Write a verified, timestamped backup while opencode runs.",
                &["backup"],
                None,
                Safety::Confirm,
            ),
            action(
                "List backups",
                "List timestamped backups, newest first.",
                &["backup", "list"],
                None,
                Safety::ReadOnly,
            ),
            action(
                "Verify backups",
                "Run an integrity check on every backup (slower).",
                &["backup", "list", "--verify"],
                None,
                Safety::ReadOnly,
            ),
            action(
                "Restore the newest backup",
                "Restore the latest backup over the current database (needs exclusive access).",
                &["backup", "restore", "--latest"],
                None,
                Safety::Confirm,
            ),
            action(
                "Keep the newest 3 backups",
                "Delete older backup files, keeping the three newest.",
                &["backup", "prune", "--keep", "3"],
                None,
                Safety::Confirm,
            ),
            action(
                "Prune backups older than",
                "Delete backup files older than the cutoff.",
                &["backup", "prune", "--older-than"],
                age_input("30d"),
                Safety::Confirm,
            ),
        ]),
    },
    Category {
        name: "Maintenance",
        short: "Maint.",
        kind: CategoryKind::Actions(&[
            action(
                "Clean up old sessions and files",
                "Back up, purge old sessions, clean storage, and VACUUM in one run.",
                &["cleanup", "--older-than"],
                age_input("30d"),
                Safety::Confirm,
            ),
            action(
                "Clean up including subagents",
                "The same one-shot cleanup, also purging old subagent sessions.",
                &["cleanup", "--subagents", "--older-than"],
                age_input("30d"),
                Safety::Confirm,
            ),
            action(
                "Clean up and verify",
                "Run the cleanup with subagents, then include database health checks.",
                &["cleanup", "--subagents", "--verify", "--older-than"],
                age_input("30d"),
                Safety::Confirm,
            ),
            action(
                "Purge sessions older than",
                "Delete every session whose latest update is older than the cutoff.",
                &["session", "purge", "--older-than"],
                age_input("30d"),
                Safety::Confirm,
            ),
            action(
                "Purge old subagent sessions",
                "Delete old sessions that were spawned by other sessions.",
                &["session", "purge", "--subagents", "--older-than"],
                age_input("30d"),
                Safety::Confirm,
            ),
            action(
                "Purge old, keep 5 per project",
                "Delete old sessions but keep the newest five per project.",
                &[
                    "session",
                    "purge",
                    "--keep-latest-per-project",
                    "5",
                    "--older-than",
                ],
                age_input("30d"),
                Safety::Confirm,
            ),
            action(
                "Strip reasoning older than",
                "Delete only reasoning content from old sessions, keeping the rest.",
                &["session", "strip-reasoning", "--older-than"],
                age_input("30d"),
                Safety::Confirm,
            ),
        ]),
    },
    Category {
        name: "System",
        short: "System",
        kind: CategoryKind::Actions(&[
            action(
                "Service status",
                "Check the registered opencode service, API reachability, and database match.",
                &["service", "status"],
                None,
                Safety::ReadOnly,
            ),
            action(
                "Show configuration",
                "Show effective settings and where each value comes from.",
                &["config", "show"],
                None,
                Safety::ReadOnly,
            ),
            action(
                "Show paths",
                "Print the resolved config, data, state, and database paths.",
                &["config", "path"],
                None,
                Safety::ReadOnly,
            ),
            action(
                "Update the binary",
                "Check the latest GitHub release and install its binary.",
                &["self-update"],
                None,
                Safety::Confirm,
            ),
        ]),
    },
];

// ---------------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------------

pub fn cmd_tui(cli: &Cli, db_path: &Path) -> Result<()> {
    if cli.no_input {
        return Err(AppError::usage(
            "tui needs interactive input; drop --no-input",
        ));
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(AppError::usage(
            "tui requires an interactive terminal (stdin and stdout must be a TTY)",
        ));
    }
    let mut app = Tui::new(cli, db_path);
    let mut session = TerminalSession::enter()?;
    app.run(&mut session)
}

// ---------------------------------------------------------------------------
// application state
// ---------------------------------------------------------------------------

struct Tui<'a> {
    cli: &'a Cli,
    db_path: PathBuf,
    db_size: Option<u64>,
    running: String,
    service: Option<String>,
    use_color: bool,
    tab: usize,
    selected: Vec<usize>,
    sessions: ListBrowser,
    projects: ListBrowser,
    view: View,
    status: Option<String>,
    preview_only: bool,
}

enum View {
    Menu,
    Prompt(Prompt),
    Output(OutputView),
}

#[derive(Clone, Copy)]
enum PromptPurpose {
    Action(&'static Action),
    SessionFilter,
    SessionOlder,
    SessionPurge,
    SessionStrip,
    SessionSearch,
    ProjectFilter,
    ProjectPurge,
}

struct Prompt {
    purpose: PromptPurpose,
    label: &'static str,
    default: &'static str,
    kind: InputKind,
    allow_empty: bool,
    text: Vec<char>,
    cursor: usize,
    error: Option<String>,
}

struct PromptSubmission {
    purpose: PromptPurpose,
    value: String,
}

#[derive(Clone)]
struct RunSpec {
    label: String,
    args: Vec<String>,
    safety: Safety,
}

#[derive(Default)]
struct ListBrowser {
    rows: Vec<serde_json::Value>,
    selected: usize,
    sort: usize,
    filter: String,
    older_than: String,
    loaded: bool,
    error: Option<String>,
}

const SESSION_SORTS: &[(&str, Option<&str>)] = &[
    ("updated", Some("updated")),
    ("size", Some("size")),
    ("cost", Some("cost")),
    ("messages", Some("messages")),
];

const PROJECT_SORTS: &[(&str, Option<&str>)] = &[
    ("worktree", None),
    ("size", Some("size")),
    ("updated", Some("updated")),
    ("sessions", Some("sessions")),
];

const SESSION_LIST_LIMIT: usize = 500;

struct OutputView {
    spec: RunSpec,
    command: String,
    status: String,
    ok: bool,
    body: String,
    scroll: Cell<usize>,
    can_apply: bool,
    preview_only: bool,
    applied: bool,
}

enum Control {
    Continue,
    Quit,
    Run(RunSpec),
    Apply(RunSpec),
    PromptCancel,
    Submitted(PromptSubmission),
    OutputBack,
}

impl<'a> Tui<'a> {
    fn new(cli: &'a Cli, db_path: &Path) -> Self {
        let use_color = !cli.no_color
            && std::env::var_os("NO_COLOR").is_none()
            && cli.color != Some(ColorWhen::Never);
        let mut app = Tui {
            cli,
            db_path: db_path.to_path_buf(),
            db_size: None,
            running: "unknown".to_string(),
            service: None,
            use_color,
            tab: 0,
            selected: vec![0; CATEGORIES.len()],
            sessions: ListBrowser::default(),
            projects: ListBrowser::default(),
            view: View::Menu,
            status: None,
            preview_only: cli.dry_run,
        };
        app.refresh(None);
        app
    }

    fn refresh(&mut self, note: Option<&str>) {
        self.db_size = fs::metadata(&self.db_path).ok().map(|m| m.len());
        self.running = match sys::running_pids() {
            Ok(pids) if pids.is_empty() => "not running".to_string(),
            Ok(pids) => format!(
                "running (pid {})",
                pids.iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Err(e) => format!("unknown ({e})"),
        };
        self.service = crate::service::discover().map(|s| format!("pid {}", s.pid));
        if let Some(note) = note {
            self.status = Some(note.to_string());
        }
    }

    fn run(&mut self, session: &mut TerminalSession) -> Result<()> {
        loop {
            self.render(session)?;
            let event = match event::read() {
                Ok(event) => event,
                Err(e) => return Err(AppError::db(format!("cannot read terminal input: {e}"))),
            };
            let Event::Key(key) = event else {
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match self.handle_key(key) {
                Control::Continue => {}
                Control::Quit => return Ok(()),
                Control::Run(spec) => {
                    self.view = View::Menu;
                    self.status = Some(format!("running {} ...", spec.label));
                    self.render(session)?;
                    let view = self.execute(&spec, false);
                    self.view = View::Output(view);
                    self.status = None;
                }
                Control::Apply(spec) => {
                    self.view = View::Menu;
                    self.status = Some(format!("applying {} ...", spec.label));
                    self.render(session)?;
                    let view = self.execute(&spec, true);
                    self.view = View::Output(view);
                    self.status = None;
                }
                Control::PromptCancel | Control::Submitted(_) | Control::OutputBack => {}
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Control {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return Control::Quit;
        }
        // Take the view so handlers can borrow `self` (for status) without
        // holding a second mutable borrow of the field.
        match std::mem::replace(&mut self.view, View::Menu) {
            View::Menu => self.handle_menu_key(key),
            View::Prompt(mut prompt) => {
                let control = handle_prompt_key(key, &mut prompt);
                match control {
                    Control::PromptCancel => {
                        self.status = Some("cancelled".to_string());
                        Control::Continue
                    }
                    Control::Submitted(submission) => self.apply_submission(submission),
                    Control::Run(_) | Control::Quit => control,
                    other => {
                        self.view = View::Prompt(prompt);
                        other
                    }
                }
            }
            View::Output(mut output) => {
                let summary = format!("{}: {}", output.spec.label, output.status);
                let reload = output.applied && output.ok;
                let control = handle_output_key(key, &mut output);
                match control {
                    Control::OutputBack => {
                        self.status = Some(summary);
                        self.refresh(None);
                        if reload {
                            self.sessions.loaded = false;
                            self.projects.loaded = false;
                            self.ensure_browser();
                        }
                        Control::Continue
                    }
                    Control::Apply(_) | Control::Run(_) | Control::Quit => control,
                    other => {
                        self.view = View::Output(output);
                        other
                    }
                }
            }
        }
    }

    fn handle_menu_key(&mut self, key: KeyEvent) -> Control {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Control::Quit,
            KeyCode::Left | KeyCode::Char('h') | KeyCode::BackTab => {
                self.switch_tab((self.tab + CATEGORIES.len() - 1) % CATEGORIES.len());
                return Control::Continue;
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
                self.switch_tab((self.tab + 1) % CATEGORIES.len());
                return Control::Continue;
            }
            KeyCode::Char('R') => {
                self.refresh(Some("status refreshed"));
                return Control::Continue;
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let index = c.to_digit(10).unwrap_or(0) as usize;
                if (1..=CATEGORIES.len()).contains(&index) {
                    self.switch_tab(index - 1);
                }
                return Control::Continue;
            }
            _ => {}
        }
        match CATEGORIES[self.tab].kind {
            CategoryKind::Actions(actions) => self.handle_action_key(key, actions),
            CategoryKind::Sessions => self.handle_sessions_key(key),
            CategoryKind::Projects => self.handle_projects_key(key),
        }
    }

    fn handle_action_key(&mut self, key: KeyEvent, actions: &'static [Action]) -> Control {
        let count = actions.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.status = None;
                self.selected[self.tab] = self.selected[self.tab].saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.status = None;
                if self.selected[self.tab] + 1 < count {
                    self.selected[self.tab] += 1;
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.status = None;
                self.selected[self.tab] = 0;
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.status = None;
                self.selected[self.tab] = count.saturating_sub(1);
            }
            KeyCode::Enter => {
                let action = &actions[self.selected[self.tab]];
                if action.input.is_some() {
                    self.status = None;
                    self.view = View::Prompt(Prompt::for_action(action));
                } else {
                    return Control::Run(action_spec(action, None));
                }
            }
            _ => {}
        }
        Control::Continue
    }

    fn handle_sessions_key(&mut self, key: KeyEvent) -> Control {
        self.ensure_browser();
        self.status = None;
        let count = self.sessions.rows.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.sessions.selected = self.sessions.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.sessions.selected + 1 < count {
                    self.sessions.selected += 1;
                }
            }
            KeyCode::Home | KeyCode::Char('g') => self.sessions.selected = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.sessions.selected = count.saturating_sub(1);
            }
            KeyCode::Enter => {
                if let Some(spec) = self
                    .sessions
                    .rows
                    .get(self.sessions.selected)
                    .and_then(session_show_spec)
                {
                    return Control::Run(spec);
                }
            }
            KeyCode::Char('d') => {
                if let Some(spec) = self
                    .sessions
                    .rows
                    .get(self.sessions.selected)
                    .and_then(session_delete_spec)
                {
                    return Control::Run(spec);
                }
            }
            KeyCode::Char('/') => {
                let initial = self.sessions.filter.clone();
                self.view = View::Prompt(Prompt::browser(
                    PromptPurpose::SessionFilter,
                    "Filter title or directory",
                    "",
                    InputKind::Text,
                    true,
                    &initial,
                ));
            }
            KeyCode::Char('o') => {
                let initial = self.sessions.older_than.clone();
                self.view = View::Prompt(Prompt::browser(
                    PromptPurpose::SessionOlder,
                    "Only sessions older than",
                    "",
                    InputKind::Age,
                    true,
                    &initial,
                ));
            }
            KeyCode::Char('s') => {
                self.sessions.sort = (self.sessions.sort + 1) % SESSION_SORTS.len();
                self.load_sessions(true);
            }
            KeyCode::Char('c') => {
                self.sessions.filter.clear();
                self.sessions.older_than.clear();
                self.load_sessions(true);
            }
            KeyCode::Char('r') => self.load_sessions(false),
            KeyCode::Char('F') => {
                self.view = View::Prompt(Prompt::browser(
                    PromptPurpose::SessionSearch,
                    "Search message content",
                    "",
                    InputKind::Text,
                    false,
                    "",
                ));
            }
            KeyCode::Char('P') => {
                self.view = View::Prompt(Prompt::browser(
                    PromptPurpose::SessionPurge,
                    "Purge sessions older than",
                    "30d",
                    InputKind::Age,
                    false,
                    "",
                ));
            }
            KeyCode::Char('S') => {
                self.view = View::Prompt(Prompt::browser(
                    PromptPurpose::SessionStrip,
                    "Strip reasoning older than",
                    "30d",
                    InputKind::Age,
                    false,
                    "",
                ));
            }
            _ => {}
        }
        Control::Continue
    }

    fn handle_projects_key(&mut self, key: KeyEvent) -> Control {
        self.ensure_browser();
        self.status = None;
        let count = self.visible_project_count();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.projects.selected = self.projects.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.projects.selected + 1 < count {
                    self.projects.selected += 1;
                }
            }
            KeyCode::Home | KeyCode::Char('g') => self.projects.selected = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.projects.selected = count.saturating_sub(1);
            }
            KeyCode::Enter => {
                if let Some(spec) = self.selected_project_row().and_then(project_show_spec) {
                    return Control::Run(spec);
                }
            }
            KeyCode::Char('d') => {
                if let Some(spec) = self.selected_project_row().and_then(project_delete_spec) {
                    return Control::Run(spec);
                }
            }
            KeyCode::Char('/') => {
                let initial = self.projects.filter.clone();
                self.view = View::Prompt(Prompt::browser(
                    PromptPurpose::ProjectFilter,
                    "Filter id, worktree, or name",
                    "",
                    InputKind::Text,
                    true,
                    &initial,
                ));
            }
            KeyCode::Char('s') => {
                self.projects.sort = (self.projects.sort + 1) % PROJECT_SORTS.len();
                self.load_projects(true);
            }
            KeyCode::Char('c') => {
                self.projects.filter.clear();
                self.clamp_selection();
            }
            KeyCode::Char('r') => self.load_projects(false),
            KeyCode::Char('E') => {
                return Control::Run(RunSpec {
                    label: "Purge empty projects".to_string(),
                    args: vec!["project".into(), "purge".into(), "--empty".into()],
                    safety: Safety::Confirm,
                });
            }
            KeyCode::Char('P') => {
                self.view = View::Prompt(Prompt::browser(
                    PromptPurpose::ProjectPurge,
                    "Purge projects older than",
                    "90d",
                    InputKind::Age,
                    false,
                    "",
                ));
            }
            _ => {}
        }
        Control::Continue
    }

    fn switch_tab(&mut self, tab: usize) {
        self.tab = tab;
        self.status = None;
        self.clamp_selection();
        self.ensure_browser();
    }

    fn clamp_selection(&mut self) {
        match CATEGORIES[self.tab].kind {
            CategoryKind::Actions(actions) => {
                let selected = &mut self.selected[self.tab];
                *selected = (*selected).min(actions.len().saturating_sub(1));
            }
            CategoryKind::Sessions => {
                self.sessions.selected = self
                    .sessions
                    .selected
                    .min(self.sessions.rows.len().saturating_sub(1));
            }
            CategoryKind::Projects => {
                let count = self.visible_project_count();
                self.projects.selected = self.projects.selected.min(count.saturating_sub(1));
            }
        }
    }

    fn ensure_browser(&mut self) {
        match CATEGORIES[self.tab].kind {
            CategoryKind::Sessions if !self.sessions.loaded => self.load_sessions(true),
            CategoryKind::Projects if !self.projects.loaded => self.load_projects(true),
            _ => {}
        }
    }

    fn load_sessions(&mut self, reset: bool) {
        let args = session_list_args(&self.sessions);
        match self.run_json(&args) {
            Ok(rows) => {
                self.sessions.rows = rows;
                self.sessions.error = None;
            }
            Err(error) => {
                self.sessions.rows.clear();
                self.sessions.error = Some(error);
            }
        }
        self.sessions.loaded = true;
        if reset || self.sessions.error.is_some() {
            self.sessions.selected = 0;
        } else {
            self.sessions.selected = self
                .sessions
                .selected
                .min(self.sessions.rows.len().saturating_sub(1));
        }
    }

    fn load_projects(&mut self, reset: bool) {
        let args = project_list_args(&self.projects);
        match self.run_json(&args) {
            Ok(rows) => {
                self.projects.rows = rows;
                self.projects.error = None;
            }
            Err(error) => {
                self.projects.rows.clear();
                self.projects.error = Some(error);
            }
        }
        self.projects.loaded = true;
        if reset || self.projects.error.is_some() {
            self.projects.selected = 0;
        } else {
            let count = self.visible_project_count();
            self.projects.selected = self.projects.selected.min(count.saturating_sub(1));
        }
    }

    fn apply_submission(&mut self, submission: PromptSubmission) -> Control {
        let value = submission.value;
        match submission.purpose {
            PromptPurpose::Action(action) => {
                return Control::Run(action_spec(action, Some(&value)));
            }
            PromptPurpose::SessionFilter => {
                self.sessions.filter = value;
                self.load_sessions(true);
            }
            PromptPurpose::SessionOlder => {
                self.sessions.older_than = value;
                self.load_sessions(true);
            }
            PromptPurpose::SessionPurge => {
                return Control::Run(RunSpec {
                    label: "Purge sessions older than".to_string(),
                    args: vec![
                        "session".into(),
                        "purge".into(),
                        "--older-than".into(),
                        value,
                    ],
                    safety: Safety::Confirm,
                });
            }
            PromptPurpose::SessionStrip => {
                return Control::Run(RunSpec {
                    label: "Strip reasoning older than".to_string(),
                    args: vec![
                        "session".into(),
                        "strip-reasoning".into(),
                        "--older-than".into(),
                        value,
                    ],
                    safety: Safety::Confirm,
                });
            }
            PromptPurpose::SessionSearch => {
                return Control::Run(RunSpec {
                    label: format!("Search message content: {value}"),
                    args: vec![
                        "session".into(),
                        "search".into(),
                        "--limit".into(),
                        "20".into(),
                        value,
                    ],
                    safety: Safety::ReadOnly,
                });
            }
            PromptPurpose::ProjectFilter => {
                self.projects.filter = value;
                self.clamp_selection();
            }
            PromptPurpose::ProjectPurge => {
                return Control::Run(RunSpec {
                    label: "Purge projects older than".to_string(),
                    args: vec![
                        "project".into(),
                        "purge".into(),
                        "--older-than".into(),
                        value,
                    ],
                    safety: Safety::Confirm,
                });
            }
        }
        Control::Continue
    }

    fn visible_project_count(&self) -> usize {
        self.visible_project_rows().len()
    }

    fn visible_project_rows(&self) -> Vec<&serde_json::Value> {
        let needle = self.projects.filter.trim().to_lowercase();
        self.projects
            .rows
            .iter()
            .filter(|row| {
                if needle.is_empty() {
                    return true;
                }
                ["id", "worktree", "name"].iter().any(|key| {
                    row.get(*key)
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|value| value.to_lowercase().contains(&needle))
                })
            })
            .collect()
    }

    fn selected_project_row(&self) -> Option<&serde_json::Value> {
        let rows = self.visible_project_rows();
        rows.get(self.projects.selected).copied()
    }

    fn run_json(&self, args: &[String]) -> std::result::Result<Vec<serde_json::Value>, String> {
        let mut full = args.to_vec();
        full.extend(global_child_args(self.cli, true));
        let output = self.run_child(&full)?;
        let stderr = strip_ansi(&String::from_utf8_lossy(&output.stderr));
        if !output.status.success() {
            let message = stderr.trim();
            return Err(if message.is_empty() {
                format!("command failed (exit {:?})", output.status.code())
            } else {
                message.to_string()
            });
        }
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("invalid JSON from opencode-dbtool: {e}"))?;
        match value {
            serde_json::Value::Array(rows) => Ok(rows),
            _ => Err("expected a JSON array from opencode-dbtool".to_string()),
        }
    }

    fn run_child(&self, args: &[String]) -> std::result::Result<std::process::Output, String> {
        let columns = terminal::size().map(|(w, _)| w.max(1)).unwrap_or(80);
        let exe = std::env::current_exe()
            .map_err(|e| format!("cannot locate the current executable: {e}"))?;
        ProcessCommand::new(exe)
            .args(args)
            .env("COLUMNS", columns.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("cannot run opencode-dbtool: {e}"))
    }

    fn execute(&self, spec: &RunSpec, apply: bool) -> OutputView {
        let preview = !apply && (spec.safety == Safety::Confirm || self.preview_only);
        let mut args: Vec<String> = spec.args.clone();
        args.extend(global_child_args(self.cli, false));
        let mut shown_args = spec.args.clone();
        if apply {
            args.push("--yes".to_string());
            shown_args.push("--yes".to_string());
        } else if preview {
            args.push("--dry-run".to_string());
            shown_args.push("--dry-run".to_string());
        }
        let command = command_line(&shown_args);
        let started = Instant::now();
        let result = self.run_child(&args);
        let elapsed = started.elapsed();
        let (status, ok, body) = match result {
            Ok(result) => {
                let ok = result.status.success();
                let status = match result.status.code() {
                    Some(code) if elapsed.as_millis() < 1_000 => {
                        format!("exit {code} · {} ms", elapsed.as_millis())
                    }
                    Some(code) => format!("exit {code} · {:.1} s", elapsed.as_secs_f64()),
                    None => "terminated by signal".to_string(),
                };
                let mut body = String::new();
                let stdout = strip_ansi(&String::from_utf8_lossy(&result.stdout));
                let stderr = strip_ansi(&String::from_utf8_lossy(&result.stderr));
                if !stdout.trim().is_empty() {
                    body.push_str(stdout.trim_end());
                }
                if !stderr.trim().is_empty() {
                    if !body.is_empty() {
                        body.push_str("\n\n");
                    }
                    body.push_str("stderr:\n");
                    body.push_str(stderr.trim_end());
                }
                if body.is_empty() {
                    body.push_str("(no output)");
                }
                (status, ok, body)
            }
            Err(message) => (message.clone(), false, message),
        };
        let can_apply = preview && ok && !self.preview_only;
        OutputView {
            spec: spec.clone(),
            command,
            status,
            ok,
            body,
            scroll: Cell::new(0),
            can_apply,
            preview_only: self.preview_only,
            applied: apply,
        }
    }

    // -----------------------------------------------------------------------
    // rendering
    // -----------------------------------------------------------------------

    fn render(&self, session: &mut TerminalSession) -> Result<()> {
        let (width, height) = terminal::size().unwrap_or((80, 24));
        let width = if width == 0 { 80 } else { width as usize };
        let height = if height == 0 { 24 } else { height as usize };
        let out = &mut session.out;
        queue!(out, Hide)?;
        for row in 0..height {
            queue!(out, MoveTo(0, row as u16), Clear(ClearType::CurrentLine))?;
        }
        if width < 40 || height < 8 {
            put_line(
                out,
                0,
                0,
                &Line::from(vec![Span::new(
                    "Terminal too small - resize to at least 40x8.",
                )
                .bold()]),
                width,
                self.use_color,
            )?;
            out.flush()?;
            return Ok(());
        }
        match &self.view {
            View::Menu => self.render_menu(out, width, height, None),
            View::Prompt(prompt) => self.render_menu(out, width, height, Some(prompt)),
            View::Output(output) => self.render_output(out, width, height, output),
        }?;
        queue!(out, MoveTo(0, height.saturating_sub(1) as u16))?;
        out.flush()?;
        Ok(())
    }

    fn render_menu(
        &self,
        out: &mut io::Stdout,
        width: usize,
        height: usize,
        prompt: Option<&Prompt>,
    ) -> io::Result<()> {
        self.render_top(out, width)?;
        match CATEGORIES[self.tab].kind {
            CategoryKind::Actions(actions) => {
                self.render_actions_body(out, width, height, actions)?;
            }
            CategoryKind::Sessions | CategoryKind::Projects => {
                self.render_browser_body(out, width, height)?;
            }
        }

        if let Some(prompt) = prompt {
            let line = prompt_line(prompt, width);
            put_line(
                out,
                height.saturating_sub(2),
                0,
                &line,
                width,
                self.use_color,
            )?;
            put_line(
                out,
                height.saturating_sub(1),
                0,
                &Line::from(vec![Span::new(
                    "Enter run   Esc cancel   Left/Right edit   Ctrl+U clear",
                )
                .dim()]),
                width,
                self.use_color,
            )?;
        } else {
            put_line(
                out,
                height.saturating_sub(2),
                0,
                &self.menu_status_line(),
                width,
                self.use_color,
            )?;
            put_line(
                out,
                height.saturating_sub(1),
                0,
                &Line::from(vec![Span::new(self.menu_footer()).dim()]),
                width,
                self.use_color,
            )?;
        }
        Ok(())
    }

    fn render_top(&self, out: &mut io::Stdout, width: usize) -> io::Result<()> {
        let title = Line::from(vec![
            Span::new(" opencode-dbtool ").bold().reverse(),
            Span::new(format!(" v{}  interactive menu", env!("CARGO_PKG_VERSION"))).dim(),
        ]);
        put_line(out, 0, 0, &title, width, self.use_color)?;

        let size = self
            .db_size
            .map(|size| output::human_bytes(size.min(i64::MAX as u64) as i64))
            .unwrap_or_else(|| "missing".to_string());
        let status_color = match self.running.as_str() {
            "not running" => Color::Green,
            s if s.starts_with("running") => Color::Yellow,
            _ => Color::Red,
        };
        let mut info = vec![
            Span::new("DB: ").dim(),
            Span::new(self.db_path.display().to_string()),
            Span::new(format!(" ({size})")).dim(),
            Span::new("   opencode: ").dim(),
            Span::new(self.running.clone()).fg(status_color),
        ];
        if let Some(service) = &self.service {
            info.push(Span::new(format!("   service: {service}")).dim());
        }
        if self.cli.restart_service {
            info.push(Span::new("   RESTART SERVICE").fg(Color::Yellow).bold());
        }
        if self.preview_only {
            info.push(
                Span::new("   PREVIEW ONLY (--dry-run)")
                    .fg(Color::Yellow)
                    .bold(),
            );
        }
        put_line(out, 1, 0, &Line::from(info), width, self.use_color)?;

        put_line(out, 2, 0, &self.tab_line(width), width, self.use_color)?;
        put_line(
            out,
            3,
            0,
            &Line::from(vec![Span::new("-".repeat(width)).dim()]),
            width,
            self.use_color,
        )?;
        Ok(())
    }

    fn render_actions_body(
        &self,
        out: &mut io::Stdout,
        width: usize,
        height: usize,
        actions: &'static [Action],
    ) -> io::Result<()> {
        let body_top = 4usize;
        let body_bottom = height.saturating_sub(2);
        let body_height = body_bottom.saturating_sub(body_top);
        let left_width = (width / 2).clamp(20, 40).min(width.saturating_sub(18));

        if body_height == 0 {
            return Ok(());
        }
        let selected = self.selected[self.tab];
        let start = window_start(selected, actions.len(), body_height);
        let detail_width = width.saturating_sub(left_width + 2);
        let details = actions
            .get(selected)
            .map(|action| detail_lines(action, detail_width))
            .unwrap_or_default();
        for (offset, row) in (body_top..body_bottom).enumerate() {
            clear_row(out, row)?;
            if let Some(action) = actions.get(start + offset) {
                let index = start + offset;
                let marker = if index == selected { "> " } else { "  " };
                let mut line = vec![Span::new(marker).dim()];
                let label = Span::new(action.label);
                if index == selected {
                    line.push(label.bold().reverse());
                } else {
                    line.push(label);
                }
                put_line(out, row, 0, &Line::from(line), left_width, self.use_color)?;
            }
            put_divider(out, row, left_width, self.use_color)?;
            if let Some(line) = details.get(offset) {
                put_line(out, row, left_width + 2, line, detail_width, self.use_color)?;
            }
        }
        Ok(())
    }

    fn render_browser_body(
        &self,
        out: &mut io::Stdout,
        width: usize,
        height: usize,
    ) -> io::Result<()> {
        let header_row = 4usize;
        let body_bottom = height.saturating_sub(2);
        let body_height = body_bottom.saturating_sub(header_row + 1);
        let show_detail = width >= 80;
        let left_width = if show_detail {
            (width * 3 / 5).clamp(40, 72).min(width.saturating_sub(28))
        } else {
            width
        };
        let detail_width = width.saturating_sub(left_width + 2);
        let kind = CATEGORIES[self.tab].kind;
        let rows = self.browser_rows();
        let selected = self.browser_selected();

        clear_row(out, header_row)?;
        let header = match kind {
            CategoryKind::Sessions => session_header_text(left_width),
            CategoryKind::Projects => project_header_text(left_width),
            CategoryKind::Actions(_) => String::new(),
        };
        put_line(
            out,
            header_row,
            0,
            &Line::from(vec![Span::new(header).dim()]),
            left_width,
            self.use_color,
        )?;
        if show_detail {
            put_divider(out, header_row, left_width, self.use_color)?;
            put_line(
                out,
                header_row,
                left_width + 2,
                &Line::from(vec![Span::new("DETAIL").dim()]),
                detail_width,
                self.use_color,
            )?;
        }

        let details = rows
            .get(selected)
            .map(|row| match kind {
                CategoryKind::Sessions => session_detail_lines(row, detail_width),
                CategoryKind::Projects => project_detail_lines(row, detail_width),
                CategoryKind::Actions(_) => Vec::new(),
            })
            .unwrap_or_else(|| vec![Line::from(vec![Span::new("No item selected.").dim()])]);
        let start = window_start(selected, rows.len(), body_height);
        for offset in 0..body_height {
            let row = header_row + 1 + offset;
            clear_row(out, row)?;
            if let Some(item) = rows.get(start + offset) {
                let text = match kind {
                    CategoryKind::Sessions => session_row_text(item, left_width),
                    CategoryKind::Projects => project_row_text(item, left_width),
                    CategoryKind::Actions(_) => String::new(),
                };
                let span = if start + offset == selected {
                    Span::new(text).bold().reverse()
                } else {
                    Span::new(text)
                };
                put_line(
                    out,
                    row,
                    0,
                    &Line::from(vec![span]),
                    left_width,
                    self.use_color,
                )?;
            } else if offset == 0 {
                let (text, color) = match self.browser_error() {
                    Some(error) => (format!("error: {error}"), Color::Red),
                    None => (self.empty_browser_message().to_string(), Color::DarkGrey),
                };
                put_line(
                    out,
                    row,
                    0,
                    &Line::from(vec![Span::new(text).fg(color)]),
                    left_width,
                    self.use_color,
                )?;
            }
            if show_detail {
                put_divider(out, row, left_width, self.use_color)?;
                if let Some(line) = details.get(offset) {
                    put_line(out, row, left_width + 2, line, detail_width, self.use_color)?;
                }
            }
        }
        Ok(())
    }

    fn browser_rows(&self) -> Vec<&Value> {
        match CATEGORIES[self.tab].kind {
            CategoryKind::Sessions => self.sessions.rows.iter().collect(),
            CategoryKind::Projects => self.visible_project_rows(),
            CategoryKind::Actions(_) => Vec::new(),
        }
    }

    fn browser_selected(&self) -> usize {
        match CATEGORIES[self.tab].kind {
            CategoryKind::Sessions => self.sessions.selected,
            CategoryKind::Projects => self.projects.selected,
            CategoryKind::Actions(_) => 0,
        }
    }

    fn browser_error(&self) -> Option<&str> {
        match CATEGORIES[self.tab].kind {
            CategoryKind::Sessions => self.sessions.error.as_deref(),
            CategoryKind::Projects => self.projects.error.as_deref(),
            CategoryKind::Actions(_) => None,
        }
    }

    fn empty_browser_message(&self) -> &'static str {
        match CATEGORIES[self.tab].kind {
            CategoryKind::Sessions => "(no sessions match)",
            CategoryKind::Projects => "(no projects match)",
            CategoryKind::Actions(_) => "",
        }
    }

    fn menu_status_line(&self) -> Line {
        if let Some(error) = self.browser_error() {
            return Line::from(vec![Span::new(format!("error: {error}")).fg(Color::Red)]);
        }
        if let Some(status) = &self.status {
            return Line::from(vec![Span::new(status.clone()).dim()]);
        }
        match CATEGORIES[self.tab].kind {
            CategoryKind::Actions(_) => Line::from(vec![Span::new(
                "Select an action. Destructive actions are previewed first.",
            )
            .dim()]),
            CategoryKind::Sessions | CategoryKind::Projects => {
                Line::from(vec![Span::new(self.browser_summary()).dim()])
            }
        }
    }

    fn browser_summary(&self) -> String {
        match CATEGORIES[self.tab].kind {
            CategoryKind::Sessions => {
                let browser = &self.sessions;
                let total = browser.rows.len();
                let selected = if total == 0 { 0 } else { browser.selected + 1 };
                let mut parts = vec![format!("{total} sessions")];
                if total == SESSION_LIST_LIMIT {
                    parts.push(format!("first {SESSION_LIST_LIMIT}"));
                }
                if !browser.filter.is_empty() {
                    parts.push(format!("filter \"{}\"", browser.filter));
                }
                if !browser.older_than.is_empty() {
                    parts.push(format!("older than {}", browser.older_than));
                }
                parts.push(format!("sort {}", SESSION_SORTS[browser.sort].0));
                parts.push(format!("{selected}/{total}"));
                parts.join("  ·  ")
            }
            CategoryKind::Projects => {
                let browser = &self.projects;
                let total = self.visible_project_count();
                let selected = if total == 0 { 0 } else { browser.selected + 1 };
                let mut parts = vec![format!("{total} projects")];
                if !browser.filter.is_empty() {
                    parts.push(format!("filter \"{}\"", browser.filter));
                }
                parts.push(format!("sort {}", PROJECT_SORTS[browser.sort].0));
                parts.push(format!("{selected}/{total}"));
                parts.join("  ·  ")
            }
            CategoryKind::Actions(_) => String::new(),
        }
    }

    fn menu_footer(&self) -> &'static str {
        match CATEGORIES[self.tab].kind {
            CategoryKind::Actions(_) => {
                "Up/Down move   Left/Right or 1-9 section   Enter select   R refresh   q quit"
            }
            CategoryKind::Sessions => {
                "Enter view d del / find s sort c clear r reload F search P purge S strip q quit"
            }
            CategoryKind::Projects => {
                "Enter view d del / find s sort c clear r reload E empty P old q quit"
            }
        }
    }

    fn tab_line(&self, width: usize) -> Line {
        let labels: Vec<String> = CATEGORIES
            .iter()
            .enumerate()
            .map(|(index, category)| {
                let name = if index == self.tab {
                    category.name
                } else {
                    category.short
                };
                format!(" {name} ")
            })
            .collect();
        let (start, end) = tab_window(&labels, self.tab, width);
        let mut spans = Vec::new();
        if start > 0 {
            spans.push(Span::new("...").dim());
        }
        for (index, label) in labels.iter().enumerate().take(end).skip(start) {
            if index > start || start > 0 {
                spans.push(Span::new(" "));
            }
            let span = Span::new(label.clone());
            if index == self.tab {
                spans.push(span.bold().reverse());
            } else {
                spans.push(span.dim());
            }
        }
        if end < labels.len() {
            spans.push(Span::new(" ...").dim());
        }
        Line::from(spans)
    }

    fn render_output(
        &self,
        out: &mut io::Stdout,
        width: usize,
        height: usize,
        output: &OutputView,
    ) -> io::Result<()> {
        let title_text = if output.applied {
            format!("{} · applied", output.spec.label)
        } else if output.can_apply {
            format!("{} · preview", output.spec.label)
        } else if output.preview_only && output.spec.safety == Safety::Confirm {
            format!("{} · preview only", output.spec.label)
        } else {
            output.spec.label.clone()
        };
        let status_color = if output.ok { Color::Green } else { Color::Red };
        let title = Line::from(vec![
            Span::new(" ").reverse(),
            Span::new(format!(" {title_text} ")).bold().reverse(),
            Span::new("  "),
            Span::new(output.status.clone()).fg(status_color),
        ]);
        put_line(out, 0, 0, &title, width, self.use_color)?;
        put_line(
            out,
            1,
            0,
            &Line::from(vec![
                Span::new("$ ").dim(),
                Span::new(output.command.clone()),
            ]),
            width,
            self.use_color,
        )?;
        put_line(
            out,
            2,
            0,
            &Line::from(vec![Span::new("-".repeat(width)).dim()]),
            width,
            self.use_color,
        )?;

        let body_top = 3usize;
        let body_bottom = height.saturating_sub(2);
        let body_height = body_bottom.saturating_sub(body_top);
        let body = wrapped_body(&output.body, width);
        let max_start = body.len().saturating_sub(body_height);
        let start = output.scroll.get().min(max_start);
        output.scroll.set(start);
        for (offset, row) in (body_top..body_bottom).enumerate() {
            clear_row(out, row)?;
            if let Some(line) = body.get(start + offset) {
                put_line(
                    out,
                    row,
                    0,
                    &Line::from(vec![Span::new(line.clone())]),
                    width,
                    self.use_color,
                )?;
            }
        }

        let shown_end = (start + body_height).min(body.len());
        let position = if body.is_empty() {
            "no output".to_string()
        } else {
            format!(
                "lines {}-{} of {}",
                start + 1,
                shown_end.max(start + 1),
                body.len()
            )
        };
        put_line(
            out,
            height.saturating_sub(2),
            0,
            &Line::from(vec![
                Span::new(position).dim(),
                Span::new("   "),
                Span::new(if output.can_apply {
                    "Enter apply"
                } else {
                    "Esc back"
                })
                .dim(),
            ]),
            width,
            self.use_color,
        )?;
        let help = if output.can_apply {
            "Enter/a apply   r rerun preview   Up/Down scroll   PgUp/PgDn page   Esc back"
        } else {
            "r rerun   Up/Down scroll   PgUp/PgDn page   Home/End top/bottom   Esc back"
        };
        put_line(
            out,
            height.saturating_sub(1),
            0,
            &Line::from(vec![Span::new(help).dim()]),
            width,
            self.use_color,
        )?;
        Ok(())
    }
}

fn handle_prompt_key(key: KeyEvent, prompt: &mut Prompt) -> Control {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('u') => {
                prompt.text.clear();
                prompt.cursor = 0;
                prompt.error = None;
            }
            KeyCode::Char('a') => prompt.cursor = 0,
            KeyCode::Char('e') => prompt.cursor = prompt.text.len(),
            _ => {}
        }
        return Control::Continue;
    }
    match key.code {
        KeyCode::Esc => return Control::PromptCancel,
        KeyCode::Enter => {
            let typed: String = prompt.text.iter().collect();
            let value = if typed.trim().is_empty() {
                if !prompt.default.is_empty() {
                    prompt.default.to_string()
                } else if prompt.allow_empty {
                    String::new()
                } else {
                    prompt.error = Some("a value is required".to_string());
                    return Control::Continue;
                }
            } else {
                typed.trim().to_string()
            };
            if !value.is_empty() {
                if let Some(message) = validate(prompt.kind, &value) {
                    prompt.error = Some(message);
                    return Control::Continue;
                }
            }
            return Control::Submitted(PromptSubmission {
                purpose: prompt.purpose,
                value,
            });
        }
        KeyCode::Left => prompt.cursor = prompt.cursor.saturating_sub(1),
        KeyCode::Right => prompt.cursor = (prompt.cursor + 1).min(prompt.text.len()),
        KeyCode::Home => prompt.cursor = 0,
        KeyCode::End => prompt.cursor = prompt.text.len(),
        KeyCode::Backspace => {
            if prompt.cursor > 0 {
                prompt.cursor -= 1;
                prompt.text.remove(prompt.cursor);
                prompt.error = None;
            }
        }
        KeyCode::Delete => {
            if prompt.cursor < prompt.text.len() {
                prompt.text.remove(prompt.cursor);
                prompt.error = None;
            }
        }
        KeyCode::Char(c) if !c.is_control() => {
            prompt.text.insert(prompt.cursor, c);
            prompt.cursor += 1;
            prompt.error = None;
        }
        _ => {}
    }
    Control::Continue
}

fn handle_output_key(key: KeyEvent, output: &mut OutputView) -> Control {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => return Control::OutputBack,
        KeyCode::Up | KeyCode::Char('k') => {
            output.scroll.set(output.scroll.get().saturating_sub(1));
        }
        KeyCode::Down | KeyCode::Char('j') => {
            output.scroll.set(output.scroll.get().saturating_add(1));
        }
        KeyCode::PageUp => output.scroll.set(output.scroll.get().saturating_sub(10)),
        KeyCode::PageDown => output.scroll.set(output.scroll.get().saturating_add(10)),
        KeyCode::Home | KeyCode::Char('g') => output.scroll.set(0),
        KeyCode::End | KeyCode::Char('G') => output.scroll.set(usize::MAX / 2),
        KeyCode::Enter | KeyCode::Char('a') if output.can_apply => {
            return Control::Apply(output.spec.clone());
        }
        KeyCode::Char('r') => return Control::Run(output.spec.clone()),
        _ => {}
    }
    Control::Continue
}

// ---------------------------------------------------------------------------
// terminal lifecycle and drawing
// ---------------------------------------------------------------------------

struct TerminalSession {
    out: io::Stdout,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()
            .map_err(|e| AppError::db(format!("cannot enable raw terminal mode: {e}")))?;
        let mut out = io::stdout();
        if let Err(e) = execute!(out, EnterAlternateScreen, Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(AppError::db(format!(
                "cannot enter the alternate screen: {e}"
            )));
        }
        Ok(TerminalSession { out })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = execute!(self.out, Show, LeaveAlternateScreen, ResetColor);
        let _ = terminal::disable_raw_mode();
    }
}

#[derive(Clone, Default)]
struct Span {
    text: String,
    fg: Option<Color>,
    bold: bool,
    dim: bool,
    reverse: bool,
}

impl Span {
    fn new(text: impl Into<String>) -> Self {
        Span {
            text: text.into(),
            ..Span::default()
        }
    }

    fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }
}

struct Line(Vec<Span>);

impl From<Vec<Span>> for Line {
    fn from(spans: Vec<Span>) -> Self {
        Line(spans)
    }
}

fn clear_row(out: &mut io::Stdout, row: usize) -> io::Result<()> {
    queue!(out, MoveTo(0, row as u16), Clear(ClearType::CurrentLine))
}

fn put_divider(out: &mut io::Stdout, row: usize, col: usize, use_color: bool) -> io::Result<()> {
    queue!(out, MoveTo(col as u16, row as u16))?;
    if use_color {
        queue!(out, SetForegroundColor(Color::DarkGrey))?;
    }
    queue!(out, Print("|"), ResetColor, SetAttribute(Attribute::Reset))
}

fn put_line(
    out: &mut io::Stdout,
    row: usize,
    col: usize,
    line: &Line,
    max_width: usize,
    use_color: bool,
) -> io::Result<()> {
    if max_width == 0 {
        return Ok(());
    }
    queue!(out, MoveTo(col as u16, row as u16))?;
    let mut used = 0usize;
    for span in &line.0 {
        if used >= max_width {
            break;
        }
        let remaining = max_width - used;
        let (text, width, truncated) = truncate_chars(&span.text, remaining);
        if !text.is_empty() {
            queue!(out, SetAttribute(Attribute::Reset))?;
            if span.bold {
                queue!(out, SetAttribute(Attribute::Bold))?;
            }
            if span.dim {
                queue!(out, SetAttribute(Attribute::Dim))?;
            }
            if span.reverse {
                queue!(out, SetAttribute(Attribute::Reverse))?;
            }
            if use_color {
                if let Some(color) = span.fg {
                    queue!(out, SetForegroundColor(color))?;
                }
            }
            queue!(out, Print(text))?;
            used += width;
        }
        if truncated {
            break;
        }
    }
    queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
    Ok(())
}

fn truncate_chars(text: &str, max: usize) -> (String, usize, bool) {
    let count = text.chars().count();
    if count <= max {
        return (text.to_string(), count, false);
    }
    if max == 0 {
        return (String::new(), 0, true);
    }
    if max == 1 {
        return ("…".to_string(), 1, true);
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    (out, max, true)
}

fn window_start(selected: usize, len: usize, height: usize) -> usize {
    if len <= height || height == 0 {
        return 0;
    }
    if selected < height {
        0
    } else {
        (selected + 1).saturating_sub(height)
    }
}

fn tab_window(labels: &[String], active: usize, width: usize) -> (usize, usize) {
    let mut start = active;
    let mut end = active + 1;
    let mut used = labels[active].chars().count();
    if used >= width {
        return (start, end);
    }
    loop {
        let mut grew = false;
        if end < labels.len() {
            let extra = labels[end].chars().count() + 1;
            if used + extra <= width {
                used += extra;
                end += 1;
                grew = true;
            }
        }
        if start > 0 {
            let extra = labels[start - 1].chars().count() + 1;
            if used + extra <= width {
                used += extra;
                start -= 1;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    (start, end)
}

// ---------------------------------------------------------------------------
// command construction
// ---------------------------------------------------------------------------

impl Prompt {
    fn for_action(action: &'static Action) -> Self {
        let input = action.input.expect("action prompt has input");
        Prompt {
            purpose: PromptPurpose::Action(action),
            label: input.prompt,
            default: input.default,
            kind: input.kind,
            allow_empty: false,
            text: Vec::new(),
            cursor: 0,
            error: None,
        }
    }

    fn browser(
        purpose: PromptPurpose,
        label: &'static str,
        default: &'static str,
        kind: InputKind,
        allow_empty: bool,
        initial: &str,
    ) -> Self {
        let text: Vec<char> = initial.chars().collect();
        Prompt {
            purpose,
            label,
            default,
            kind,
            allow_empty,
            cursor: text.len(),
            text,
            error: None,
        }
    }
}

fn session_list_args(browser: &ListBrowser) -> Vec<String> {
    let mut args = vec![
        "session".to_string(),
        "list".to_string(),
        "--limit".to_string(),
        SESSION_LIST_LIMIT.to_string(),
    ];
    if let Some((_, Some(sort))) = SESSION_SORTS.get(browser.sort) {
        args.push("--sort".to_string());
        args.push((*sort).to_string());
    }
    if !browser.filter.trim().is_empty() {
        args.push("--search".to_string());
        args.push(browser.filter.clone());
    }
    if !browser.older_than.trim().is_empty() {
        args.push("--older-than".to_string());
        args.push(browser.older_than.clone());
    }
    args
}

fn project_list_args(browser: &ListBrowser) -> Vec<String> {
    let mut args = vec!["project".to_string(), "list".to_string()];
    if let Some((_, Some(sort))) = PROJECT_SORTS.get(browser.sort) {
        args.push("--sort".to_string());
        args.push((*sort).to_string());
    }
    args
}

fn session_show_spec(row: &Value) -> Option<RunSpec> {
    let id = row_str(row, "id");
    if id.is_empty() {
        return None;
    }
    Some(RunSpec {
        label: format!("Session {}", short_id(&id)),
        args: vec![
            "session".into(),
            "show".into(),
            "--messages".into(),
            "--limit".into(),
            "50".into(),
            id,
        ],
        safety: Safety::ReadOnly,
    })
}

fn session_delete_spec(row: &Value) -> Option<RunSpec> {
    let id = row_str(row, "id");
    if id.is_empty() {
        return None;
    }
    Some(RunSpec {
        label: format!("Delete session {}", short_id(&id)),
        args: vec!["session".into(), "delete".into(), id],
        safety: Safety::Confirm,
    })
}

fn project_show_spec(row: &Value) -> Option<RunSpec> {
    let id = row_str(row, "id");
    if id.is_empty() {
        return None;
    }
    Some(RunSpec {
        label: format!("Project {}", project_label(row)),
        args: vec!["project".into(), "show".into(), id],
        safety: Safety::ReadOnly,
    })
}

fn project_delete_spec(row: &Value) -> Option<RunSpec> {
    let id = row_str(row, "id");
    if id.is_empty() {
        return None;
    }
    Some(RunSpec {
        label: format!("Delete project {}", project_label(row)),
        args: vec!["project".into(), "delete".into(), id],
        safety: Safety::Confirm,
    })
}

fn row_str(row: &Value, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn row_i64(row: &Value, key: &str) -> i64 {
    row.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn row_f64(row: &Value, key: &str) -> f64 {
    row.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn row_bool(row: &Value, key: &str) -> bool {
    row.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn row_age(row: &Value) -> String {
    let updated = row_str(row, "updated");
    match parse_iso_utc(&updated) {
        Some(ms) => human_age(now_ms().unwrap_or(0), ms),
        None if updated.is_empty() => "-".to_string(),
        None => updated,
    }
}

fn short_id(id: &str) -> String {
    truncate_chars(id, 12).0
}

fn project_label(row: &Value) -> String {
    let name = row_str(row, "name");
    if !name.trim().is_empty() {
        return name;
    }
    let worktree = row_str(row, "worktree");
    let base = basename(&worktree);
    if base.is_empty() {
        row_str(row, "id")
    } else {
        base
    }
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_string()
}

fn fit_cell(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let (text, used, _) = truncate_chars(text, width);
    format!("{text}{}", " ".repeat(width.saturating_sub(used)))
}

fn fit_right(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let (text, used, _) = truncate_chars(text, width);
    format!("{}{text}", " ".repeat(width.saturating_sub(used)))
}

fn session_header_text(width: usize) -> String {
    let fixed = 12 + 10 + 10 + 3;
    let title_width = width.saturating_sub(fixed);
    format!(
        "{} {} {} {}",
        fit_cell("ID", 12),
        fit_cell("TITLE", title_width),
        fit_right("UPDATED", 10),
        fit_right("SIZE", 10)
    )
}

fn session_row_text(row: &Value, width: usize) -> String {
    let fixed = 12 + 10 + 10 + 3;
    let title_width = width.saturating_sub(fixed);
    let mut title = row_str(row, "title");
    if title.trim().is_empty() {
        title = "(untitled)".to_string();
    }
    let size = output::human_bytes(row_i64(row, "size_bytes"));
    format!(
        "{} {} {} {}",
        fit_cell(&short_id(&row_str(row, "id")), 12),
        fit_cell(&title, title_width),
        fit_right(&row_age(row), 10),
        fit_right(&size, 10)
    )
}

fn project_header_text(width: usize) -> String {
    let fixed = 5 + 10 + 10 + 3;
    let name_width = width.saturating_sub(fixed);
    format!(
        "{} {} {} {}",
        fit_cell("PROJECT", name_width),
        fit_right("SESS", 5),
        fit_right("SIZE", 10),
        fit_right("UPDATED", 10)
    )
}

fn project_row_text(row: &Value, width: usize) -> String {
    let fixed = 5 + 10 + 10 + 3;
    let name_width = width.saturating_sub(fixed);
    format!(
        "{} {} {} {}",
        fit_cell(&project_label(row), name_width),
        fit_right(&row_i64(row, "sessions").to_string(), 5),
        fit_right(&output::human_bytes(row_i64(row, "size_bytes")), 10),
        fit_right(&row_age(row), 10)
    )
}

fn field_lines(label: &str, value: &str, width: usize) -> Vec<Line> {
    let prefix = format!("{label:<10} ");
    let indent = prefix.chars().count();
    let value_width = width.saturating_sub(indent).max(1);
    let text = if value.is_empty() { "-" } else { value };
    wrap_line(text, value_width)
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            if index == 0 {
                Line::from(vec![Span::new(prefix.clone()).dim(), Span::new(chunk)])
            } else {
                Line::from(vec![Span::new(" ".repeat(indent)), Span::new(chunk).dim()])
            }
        })
        .collect()
}

fn session_detail_lines(row: &Value, width: usize) -> Vec<Line> {
    let mut lines = field_lines("Title", &row_str(row, "title"), width);
    lines.extend(field_lines("ID", &row_str(row, "id"), width));
    lines.extend(field_lines("Directory", &row_str(row, "directory"), width));
    let updated = format!("{} ({})", row_age(row), row_str(row, "updated"));
    lines.extend(field_lines("Updated", &updated, width));
    lines.extend(field_lines(
        "Size",
        &output::human_bytes(row_i64(row, "size_bytes")),
        width,
    ));
    lines.extend(field_lines(
        "Messages",
        &format!(
            "{} messages / {} parts",
            row_i64(row, "session_messages"),
            row_i64(row, "parts")
        ),
        width,
    ));
    lines.extend(field_lines(
        "Events",
        &row_i64(row, "events").to_string(),
        width,
    ));
    lines.extend(field_lines(
        "Media",
        &output::human_bytes(row_i64(row, "blob_bytes")),
        width,
    ));
    lines.extend(field_lines(
        "Cost",
        &format!("${:.4}", row_f64(row, "cost")),
        width,
    ));
    let parent = row_str(row, "parent_id");
    lines.extend(field_lines(
        "Parent",
        if parent.is_empty() { "-" } else { &parent },
        width,
    ));
    lines.extend(field_lines(
        "Archived",
        if row_bool(row, "archived") {
            "yes"
        } else {
            "no"
        },
        width,
    ));
    lines
}

fn project_detail_lines(row: &Value, width: usize) -> Vec<Line> {
    let mut lines = field_lines("Project", &project_label(row), width);
    lines.extend(field_lines("ID", &row_str(row, "id"), width));
    lines.extend(field_lines("Worktree", &row_str(row, "worktree"), width));
    lines.extend(field_lines(
        "Sessions",
        &row_i64(row, "sessions").to_string(),
        width,
    ));
    lines.extend(field_lines(
        "Messages",
        &format!(
            "{} messages / {} parts",
            row_i64(row, "session_messages"),
            row_i64(row, "session_parts")
        ),
        width,
    ));
    lines.extend(field_lines(
        "Events",
        &row_i64(row, "events").to_string(),
        width,
    ));
    lines.extend(field_lines(
        "Media",
        &output::human_bytes(row_i64(row, "blob_bytes")),
        width,
    ));
    lines.extend(field_lines(
        "Size",
        &output::human_bytes(row_i64(row, "size_bytes")),
        width,
    ));
    lines.extend(field_lines(
        "Cost",
        &format!("${:.4}", row_f64(row, "cost")),
        width,
    ));
    lines.extend(field_lines(
        "Updated",
        &format!("{} ({})", row_age(row), row_str(row, "updated")),
        width,
    ));
    lines
}

fn action_spec(action: &'static Action, input: Option<&str>) -> RunSpec {
    let mut args: Vec<String> = action.args.iter().map(|s| s.to_string()).collect();
    if let Some(spec) = action.input {
        let value = input.unwrap_or(spec.default);
        args.push(value.to_string());
    }
    RunSpec {
        label: action.label.to_string(),
        args,
        safety: action.safety,
    }
}

fn validate(kind: InputKind, value: &str) -> Option<String> {
    match kind {
        InputKind::Text => None,
        InputKind::Age => parse_age_ms(value).err().map(|e| e.message),
        InputKind::Size => parse_size_bytes(value).err().map(|e| e.message),
    }
}

fn global_child_args(cli: &Cli, json: bool) -> Vec<String> {
    let mut args = vec![
        "--format".to_string(),
        if json { "json" } else { "table" }.to_string(),
        "--no-pager".to_string(),
        "--no-input".to_string(),
        "--no-color".to_string(),
    ];
    if let Some(config) = &cli.config {
        args.push("--config".to_string());
        args.push(config.display().to_string());
    }
    if cli.absolute {
        args.push("--absolute".to_string());
    }
    if cli.quiet {
        args.push("--quiet".to_string());
    }
    if cli.restart_service {
        args.push("--restart-service".to_string());
    }
    if let Some(wait) = &cli.wait_lock {
        args.push("--wait-lock".to_string());
        args.push(wait.clone());
    }
    args
}

fn command_line(args: &[String]) -> String {
    let mut parts = vec!["opencode-dbtool".to_string()];
    parts.extend(args.iter().map(|arg| shell_quote(arg)));
    parts.join(" ")
}

fn display_command(action: &Action) -> String {
    let mut parts = vec!["opencode-dbtool".to_string()];
    parts.extend(action.args.iter().map(|arg| shell_quote(arg)));
    if let Some(input) = action.input {
        parts.push(format!(
            "<{}>",
            input.prompt.to_lowercase().replace(' ', "-")
        ));
    }
    match action.safety {
        Safety::ReadOnly => {}
        Safety::Writes => {}
        Safety::Confirm => parts.push("[--dry-run, then --yes]".to_string()),
    }
    parts.join(" ")
}

fn detail_lines(action: &Action, width: usize) -> Vec<Line> {
    let mut lines = vec![
        Line::from(vec![Span::new(action.label).bold()]),
        Line::from(vec![]),
    ];
    for text in wrap_line(action.description, width) {
        lines.push(Line::from(vec![Span::new(text)]));
    }
    lines.push(Line::from(vec![]));
    lines.push(Line::from(vec![Span::new("Command").dim()]));
    for text in wrap_line(&display_command(action), width) {
        lines.push(Line::from(vec![Span::new(text).fg(Color::Cyan)]));
    }
    lines.push(Line::from(vec![]));
    let (label, color) = match action.safety {
        Safety::ReadOnly => ("Read-only", Color::Green),
        Safety::Writes => ("Writes state (runs directly)", Color::Yellow),
        Safety::Confirm => ("Destructive: preview first, then confirm", Color::Red),
    };
    lines.push(Line::from(vec![Span::new(label).fg(color).bold()]));
    if let Some(input) = action.input {
        lines.push(Line::from(vec![]));
        lines.push(Line::from(vec![Span::new(format!(
            "Input: {}",
            input.hint
        ))
        .dim()]));
        if !input.default.is_empty() {
            lines.push(Line::from(vec![Span::new(format!(
                "Default when left empty: {}",
                input.default
            ))
            .dim()]));
        }
    }
    lines
}

fn prompt_line(prompt: &Prompt, width: usize) -> Line {
    let label = format!("{}: ", prompt.label);
    let label_width = label.chars().count();
    let mut spans = vec![Span::new(label).bold()];
    if prompt.text.is_empty() {
        if !prompt.default.is_empty() {
            spans.push(Span::new(format!("({})", prompt.default)).dim());
        }
        spans.push(Span::new("▏").reverse());
    } else {
        // Keep the cursor visible on narrow terminals by showing a window
        // of input text around it rather than always starting at char 0.
        let max = width.saturating_sub(label_width + 3).max(4);
        let start = prompt.cursor.saturating_sub(max.saturating_sub(1));
        let end = (start + max).min(prompt.text.len());
        if start > 0 {
            spans.push(Span::new("…").dim());
        }
        for (index, ch) in prompt.text.iter().enumerate().take(end).skip(start) {
            let text = ch.to_string();
            if index == prompt.cursor {
                spans.push(Span::new(text).reverse());
            } else {
                spans.push(Span::new(text));
            }
        }
        if prompt.cursor >= prompt.text.len() {
            spans.push(Span::new("▏").reverse());
        }
        if end < prompt.text.len() {
            spans.push(Span::new("…").dim());
        }
    }
    if let Some(error) = &prompt.error {
        spans.push(Span::new(format!("   ! {error}")).fg(Color::Red));
    }
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// text helpers
// ---------------------------------------------------------------------------

fn wrap_line(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    if text.is_empty() {
        return vec![String::new()];
    }
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(width)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect()
}

fn wrapped_body(body: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in body.split('\n') {
        out.extend(wrap_line(line, width));
    }
    out
}

/// Drop carriage returns, expand tabs, and remove CSI escape sequences from
/// child output before it is drawn in the alternate screen.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {}
            '\t' => out.push_str("    "),
            '\u{1b}' => {
                if matches!(chars.peek(), Some('[')) {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
            }
            c if c.is_control() && c != '\n' => {}
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    #[test]
    fn catalogue_labels_are_unique_and_non_empty() {
        let mut labels = std::collections::HashSet::new();
        for category in CATEGORIES {
            if let CategoryKind::Actions(actions) = category.kind {
                assert!(!actions.is_empty(), "{} is empty", category.name);
                for action in actions {
                    assert!(
                        labels.insert(action.label),
                        "duplicate action label {}",
                        action.label
                    );
                    assert!(!action.label.is_empty());
                    assert!(!action.description.is_empty());
                    assert!(!action.args.is_empty());
                    if let Some(input) = action.input {
                        assert!(!input.prompt.is_empty());
                        assert!(!input.hint.is_empty());
                        if !input.default.is_empty() {
                            assert!(
                                validate(input.kind, input.default).is_none(),
                                "bad default for {}: {}",
                                action.label,
                                input.default
                            );
                        }
                    }
                }
            }
        }
        assert!(matches!(CATEGORIES[1].kind, CategoryKind::Sessions));
        assert!(matches!(CATEGORIES[2].kind, CategoryKind::Projects));
    }

    #[test]
    fn every_action_parses_with_the_cli() {
        let globals = global_child_args(&cli(&["opencode-dbtool", "tui"]), false);
        for category in CATEGORIES {
            let CategoryKind::Actions(actions) = category.kind else {
                continue;
            };
            for action in actions {
                let mut argv = vec!["opencode-dbtool".to_string()];
                argv.extend(action.args.iter().map(|arg| arg.to_string()));
                if let Some(input) = action.input {
                    argv.push(
                        match input.kind {
                            InputKind::Text => "placeholder",
                            InputKind::Age => "30d",
                            InputKind::Size => "1M",
                        }
                        .to_string(),
                    );
                }
                argv.extend(globals.iter().cloned());
                argv.push("--dry-run".to_string());
                let parsed = Cli::try_parse_from(argv.iter().map(String::as_str));
                assert!(
                    parsed.is_ok(),
                    "{} {:?}: {}",
                    action.label,
                    argv,
                    parsed.err().map(|e| e.to_string()).unwrap_or_default()
                );
            }
        }
    }

    fn action_named(label: &str) -> &'static Action {
        CATEGORIES
            .iter()
            .find_map(|category| match category.kind {
                CategoryKind::Actions(actions) => {
                    actions.iter().find(|action| action.label == label)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing action {label}"))
    }

    #[test]
    fn action_spec_empty_input_uses_the_action_default() {
        let action = action_named("Purge entries larger than");
        let spec = action_spec(action, None);
        assert_eq!(spec.args, vec!["kv", "purge", "--larger-than", "1M"]);
        assert_eq!(spec.safety, Safety::Confirm);

        let action = action_named("Show an entry");
        let spec = action_spec(action, Some("cache:key"));
        assert_eq!(spec.args, vec!["kv", "show", "cache:key"]);
    }

    #[test]
    fn session_list_args_include_sort_and_filters() {
        let browser = ListBrowser {
            sort: 1,
            filter: "needle".to_string(),
            older_than: "30d".to_string(),
            ..ListBrowser::default()
        };
        assert_eq!(
            session_list_args(&browser),
            vec![
                "session",
                "list",
                "--limit",
                "500",
                "--sort",
                "size",
                "--search",
                "needle",
                "--older-than",
                "30d",
            ]
        );
    }

    #[test]
    fn browser_specs_use_the_selected_item_id() {
        let row = serde_json::json!({
            "id": "ses_0123456789abcdef",
            "title": "hello",
            "worktree": "/work/a",
            "name": "a"
        });
        let specs = [
            session_show_spec(&row).unwrap(),
            session_delete_spec(&row).unwrap(),
            project_show_spec(&row).unwrap(),
            project_delete_spec(&row).unwrap(),
        ];
        for spec in &specs {
            let mut argv = vec!["opencode-dbtool".to_string()];
            argv.extend(spec.args.iter().cloned());
            let parsed = Cli::try_parse_from(argv.iter().map(String::as_str));
            assert!(
                parsed.is_ok(),
                "{}: {}",
                spec.label,
                parsed.err().map(|e| e.to_string()).unwrap_or_default()
            );
        }
        assert_eq!(
            specs[1].args,
            vec!["session", "delete", "ses_0123456789abcdef"]
        );
        assert_eq!(specs[1].safety, Safety::Confirm);
        assert_eq!(
            specs[3].args,
            vec!["project", "delete", "ses_0123456789abcdef"]
        );
        assert_eq!(project_label(&row), "a");
    }

    #[test]
    fn project_list_args_omit_the_default_sort() {
        let mut browser = ListBrowser::default();
        assert_eq!(project_list_args(&browser), vec!["project", "list"]);
        browser.sort = 2;
        assert_eq!(
            project_list_args(&browser),
            vec!["project", "list", "--sort", "updated"]
        );
    }

    #[test]
    fn browser_rows_are_fixed_width() {
        let row = serde_json::json!({
            "id": "ses_0123456789abcdef",
            "title": "a very long title that does not fit",
            "size_bytes": 2048,
            "updated": "2023-11-14T22:13:20Z"
        });
        let text = session_row_text(&row, 60);
        assert_eq!(text.chars().count(), 60);
        assert!(text.contains("ses_012345"));
        assert!(text.contains("2.0 KB"));
    }

    #[test]
    fn validation_rejects_bad_ages_and_sizes() {
        assert!(validate(InputKind::Age, "30d").is_none());
        assert!(validate(InputKind::Age, "yesterday").is_some());
        assert!(validate(InputKind::Size, "1M").is_none());
        assert!(validate(InputKind::Size, "large").is_some());
        assert!(validate(InputKind::Text, "anything").is_none());
    }

    #[test]
    fn child_args_force_non_interactive_output() {
        let cli = cli(&["opencode-dbtool", "tui", "--absolute", "--wait-lock", "5m"]);
        let args = global_child_args(&cli, false);
        for expected in [
            "--format",
            "table",
            "--no-pager",
            "--no-input",
            "--no-color",
        ] {
            assert!(args.iter().any(|a| a == expected), "missing {expected}");
        }
        assert!(args.iter().any(|a| a == "--absolute"));
        assert!(args.windows(2).any(|w| w == ["--wait-lock", "5m"]));

        let args = global_child_args(&cli, true);
        assert!(args.windows(2).any(|w| w == ["--format", "json"]));
    }

    #[test]
    fn command_line_hides_global_flags() {
        let line = command_line(&[
            "session".to_string(),
            "purge".to_string(),
            "--older-than".to_string(),
            "30d".to_string(),
            "--dry-run".to_string(),
        ]);
        assert_eq!(
            line,
            "opencode-dbtool session purge --older-than 30d --dry-run"
        );
    }

    #[test]
    fn window_start_keeps_selection_visible() {
        assert_eq!(window_start(0, 10, 5), 0);
        assert_eq!(window_start(4, 10, 5), 0);
        assert_eq!(window_start(5, 10, 5), 1);
        assert_eq!(window_start(9, 10, 5), 5);
        assert_eq!(window_start(3, 3, 5), 0);
    }

    #[test]
    fn tab_window_keeps_the_active_tab_visible() {
        let labels: Vec<String> = (0..10).map(|i| format!(" Tab{i} ")).collect();
        let (start, end) = tab_window(&labels, 8, 30);
        assert!(start <= 8 && 8 < end, "active tab missing: {start}..{end}");
        assert!(
            labels[start..end]
                .iter()
                .map(|l| l.chars().count() + 1)
                .sum::<usize>()
                <= 31
        );
    }

    #[test]
    fn wraps_and_strips_terminal_sequences() {
        assert_eq!(wrap_line("abcdef", 3), vec!["abc", "def"]);
        assert_eq!(wrap_line("", 3), vec![""]);
        assert_eq!(
            strip_ansi("a\r\nb\tc\u{1b}[31mred\u{1b}[0m"),
            "a\nb    cred"
        );
    }
}
