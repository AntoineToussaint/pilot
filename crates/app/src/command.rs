//! Command: a description of an effect the shell should perform.
//!
//! `reduce(&mut State, Action)` returns `Vec<Command>`. The shell (`App`)
//! executes each command — that's the ONLY place where IO happens: spawning
//! a PTY, writing to SQLite, calling `gh`, sending an event, notifying the
//! OS, waking the poller.
//!
//! Why this shape: reduce can be tested as a pure function — given an input
//! state and an action, assert the resulting state and the list of commands.
//! No mocks, no tokio runtime, no fake Store.

use crate::action::{Action, PickerKind, ShellKind};
use pilot_core::{SessionKey, TaskId};
use std::path::PathBuf;

/// An effect the shell should perform. Intentionally coarse — we want
/// "compose several steps into one command" rather than leaking shell-level
/// details into reduce.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Some variants are reserved for upcoming migrations.
pub enum Command {
    // ── Terminal lifecycle ──
    /// Spawn a new terminal session for `session_key` in the given worktree.
    /// The shell wraps the chosen shell or agent in tmux for persistence.
    /// When `focus` is true, the terminal pane becomes the focused pane
    /// (user-initiated OpenSession). When false, the terminal spawns
    /// without stealing focus — used by auto-attach so sessions appearing
    /// while the user is navigating don't yank them into TERM mode.
    SpawnTerminal {
        session_key: SessionKey,
        cwd: PathBuf,
        kind: ShellKind,
        focus: bool,
    },
    /// Close and reap a terminal. Also runs pane/state cleanup in the shell.
    CloseTerminal { session_key: SessionKey },
    /// Set the active tab index in the shell's TerminalManager. Reduce
    /// updates `state.terminal_index.active_tab` for local decisions and
    /// emits this so the shell mirrors it.
    SetActiveTab { idx: usize },
    /// Write bytes to a running terminal's PTY.
    WriteToTerminal {
        session_key: SessionKey,
        bytes: Vec<u8>,
    },
    /// Resize the PTY behind a terminal.
    ResizeTerminal {
        session_key: SessionKey,
        cols: u16,
        rows: u16,
    },
    /// Ask an existing TermSession to scroll its viewport (libghostty
    /// scrollback). Lines positive → up, negative → down, zero → reset.
    ScrollTerminal {
        session_key: SessionKey,
        delta: i32,
    },

    // ── Store persistence ──
    /// Save a session's full JSON into SQLite.
    StoreSaveSession { session_key: SessionKey },
    /// Delete a session from SQLite (used on merge/close/stale purge).
    StoreDeleteSession { task_id: TaskId },
    /// Delete multiple stale sessions at once.
    StoreDeleteStaleSessions { task_ids: Vec<TaskId> },
    /// Persist read count after mark_read.
    StoreMarkRead { task_id: TaskId, seen_count: i64 },

    // ── Git / gh CLI ──
    /// `gh pr merge <num> --squash --repo <repo>`.
    RunGhMerge {
        repo: String,
        pr_number: String,
        session_key: SessionKey,
    },
    /// `gh pr review <num> --approve --repo <repo>`.
    RunGhApprove {
        repo: String,
        pr_number: String,
    },
    /// `gh pr comment <num> --body <body> [--reply-to <node_id>] --repo <repo>`.
    RunGhComment {
        repo: String,
        pr_number: String,
        body: String,
        reply_to_node_id: Option<String>,
    },
    /// `gh pr edit <num> --add-{reviewer,assignee} ... --remove-... --repo <repo>`.
    RunGhEditCollaborators {
        repo: String,
        pr_number: String,
        kind: PickerKind,
        added: Vec<String>,
        removed: Vec<String>,
    },
    /// Fetch collaborators for a repo (populates the picker).
    FetchCollaborators {
        repo: String,
        kind: PickerKind,
        session_key: SessionKey,
        pr_number: String,
    },
    /// Check out (clone/worktree) the branch for a session.
    CheckoutWorktree {
        owner: String,
        repo: String,
        branch: String,
        session_key: SessionKey,
        /// After checkout succeeds, re-dispatch this action so the flow resumes.
        then: Option<Box<Action>>,
    },
    /// Fetch the default branch for a repo (cached).
    FetchDefaultBranch { owner: String, repo: String },

    // ── Monitor IO ──
    /// Check if a PR has merge conflicts via `gh pr view`. Dispatches a
    /// follow-up `Action::NeedsRebaseResult` with the answer.
    CheckNeedsRebase {
        session_key: SessionKey,
        repo: String,
        pr_number: String,
        wt_path: PathBuf,
        default_branch: String,
    },
    /// Run `git fetch + rebase + push --force-with-lease` in a worktree,
    /// dispatching `Action::MonitorTick` on success.
    RunRebase {
        session_key: SessionKey,
        wt_path: PathBuf,
        default_branch: String,
    },
    /// Write the Claude context markdown file + `latest` symlink for a
    /// session. Used when queuing a prompt — reduce stages the prompt in
    /// State; the shell only writes the on-disk copy.
    WriteMonitorContext {
        session_key: SessionKey,
        content: String,
    },
    /// Enumerate live tmux sessions and dispatch `Action::TmuxSessionsRefreshed`.
    RefreshLiveTmuxSessions,

    // ── External services ──
    /// POST a JSON body to a URL (used for Slack webhooks).
    HttpPostJson { url: String, body: serde_json::Value },
    /// Open a URL in the OS default browser.
    OpenUrl { url: String },
    /// Fire a desktop notification.
    Notify { title: String, body: String },

    // ── App infrastructure ──
    /// Wake the GitHub poller for an immediate refresh.
    WakePoller,
    /// Write-through to the shared `Arc<Mutex<HashSet<String>>>` monitored
    /// set so helper tasks see the update without going through the loop.
    UpdateMonitoredSet {
        session_key: SessionKey,
        monitored: bool,
    },
    /// Re-emit an action into the loop (e.g. after async work resolves).
    DispatchAction(Action),
    /// Set a status message (tiny convenience — reduce usually writes status
    /// directly into State, but async Commands use this to report results).
    SetStatus(String),

}
