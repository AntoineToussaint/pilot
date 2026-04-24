use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self as ct_event, Event as CtEvent, EventStream};
use futures::StreamExt;
use pilot_auth::{CommandProvider, CredentialChain, EnvProvider};
use pilot_config::Config;
use pilot_core::{Session, SessionState};
use pilot_events::{EventProducer, event_bus};
use pilot_gh::{GhClient, GhPoller};
use pilot_store::{SqliteStore, Store};
use pilot_tui_term::{PtySize, TermSession};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::action::{Action, ShellKind};
use crate::input::{InputMode, TextInputKind};
use crate::keys;
use crate::nav::{handle_sidebar_click, resort_sessions, selected_session_from_nav};
use crate::pane::PaneContent;
use crate::session_manager::SessionManager;
use crate::terminal_manager::TerminalManager;
use crate::ui;

/// Top-level application state.
/// The application. Split into:
/// - `state: State` — all pure model data (testable without IO, see `state.rs`).
/// - IO / shell fields below — PTYs, store, channels, shared Arc handles.
///
/// The intent: business logic reads/writes `app.state`; only the shell touches
/// the IO fields. Long-term, `handle_action` should become `let cmds =
/// reduce(&mut self.state, action); for c in cmds { self.execute(c); }`.
pub struct App {
    /// Pure model. Everything the reducer needs — and nothing else.
    pub state: crate::state::State,

    // ── IO / shell resources ──
    /// Live PTY sessions. Non-Send, holds reader threads. Shell-only.
    pub terminals: TerminalManager,
    /// Persistent store (SQLite).
    pub store: Arc<dyn Store>,
    /// Event bus producer — providers push events, app pulls them.
    pub event_tx: EventProducer,
    /// Session keys currently in monitor mode. Shared so helper tasks
    /// can check membership without going through the action loop.
    /// `state.monitored_sessions` is the canonical mirror.
    pub monitored_sessions: crate::state::SharedMonitoredSessions,
    /// Wake handle to trigger an immediate GitHub poll.
    pub poller_wake: Option<Arc<tokio::sync::Notify>>,
}

impl App {
    pub async fn new(config: Config) -> Result<Self> {
        let (event_tx, _consumer) = event_bus();
        let store: Arc<dyn Store> = Arc::new(SqliteStore::default_path()?);
        tracing::info!("State database opened at ~/.pilot/state.db");

        // Load cached sessions from SQLite for instant startup.
        let mut sessions = SessionManager::new();
        let mut loaded = false;

        sessions.load_from_store(store.as_ref());
        if !sessions.is_empty() {
            loaded = true;
            tracing::info!("Restored {} cached sessions from SQLite", sessions.len());
        }

        let mut state = crate::state::State::with_config(config, String::new());
        state.sessions = sessions;
        state.loaded = loaded;
        state.sidebar_pct = 50;
        state.status = "Loading…".into();
        state.credentials_ok = true;

        Ok(Self {
            state,
            terminals: TerminalManager::new(),
            store,
            event_tx,
            monitored_sessions: Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new())),
            poller_wake: None,
        })
    }

    #[allow(dead_code)]
    pub fn selected_session_key(&self) -> Option<String> {
        selected_session_from_nav(self)
    }

    /// The session key of the terminal shown in the *focused* pane, if any.
    /// This is the destination for keystrokes, paste, and mouse scroll when
    /// the user is interacting with the terminal — NOT `active_tab_key`,
    /// which tracks the tab-bar's selected tab and can diverge from the
    /// visible pane after pane-focus changes.
    pub fn focused_terminal_key(&self) -> Option<String> {
        match self.state.panes.focused_content() {
            Some(crate::pane::PaneContent::Terminal(k)) => Some(k),
            _ => None,
        }
    }

    /// Get the currently selected session (if cursor is on one).
    #[allow(dead_code)]
    pub fn selected_session(&self) -> Option<&Session> {
        self.selected_session_key()
            .and_then(|k| self.state.sessions.get(&k))
    }

    /// Close a terminal and clean up ALL associated state — including the
    /// pane tree. Without pane cleanup, leaves would keep pointing at dead
    /// terminal keys, causing phantom "TERM" mode and empty panes the user
    /// can't escape.
    pub fn close_terminal(&mut self, key: &str) {
        self.terminals.close(key);
        self.state.agent_states.remove(key);
        self.state.pending_prompts.remove(key);
        self.state.notified_asking.remove(key);
        // Forget the last hook state so a re-attach doesn't inherit
        // stale "asking" / "idle" from the previous run.
        crate::claude_hooks::clear_state(key);
        // Sweep any pane leaves still pointing at dead terminal keys.
        let live: std::collections::BTreeSet<String> = self.terminals.keys().cloned().collect();
        self.state.panes.prune_dead_terminals(&live);
        // Recompute input_mode from the (now-pruned) pane tree.
        apply_determined_mode(self);
    }

    /// Refresh `state.terminal_index` — the pure projection of the live
    /// `TerminalManager`. Called after any terminal mutation so reduce sees
    /// up-to-date tab info.
    pub fn refresh_terminal_index(&mut self) {
        self.state.terminal_index.tab_order = self.terminals.tab_order().to_vec();
        self.state.terminal_index.active_tab = if self.terminals.tab_order().is_empty() {
            None
        } else {
            Some(self.terminals.active_tab())
        };
        self.state.terminal_index.keys = self.terminals.keys().cloned().collect();
    }

    /// Execute a `Command` emitted by `reduce`. This is the ONLY place in
    /// the app where side effects happen (PTY spawns, store writes, shell
    /// commands, HTTP, notifications). `reduce` stays pure.
    pub fn execute(
        &mut self,
        cmd: crate::command::Command,
        action_tx: &mpsc::UnboundedSender<Action>,
    ) {
        use crate::command::Command as C;
        match cmd {
            C::SetStatus(msg) => self.state.status = msg,
            C::DispatchAction(action) => {
                let _ = action_tx.send(action);
            }
            C::WakePoller => {
                if let Some(notify) = &self.poller_wake {
                    notify.notify_one();
                }
            }

            // ── Store ──
            C::StoreSaveSession { session_key } => {
                if let Some(session) = self.state.sessions.get(session_key.as_str()) {
                    let json = serde_json::to_string(session).ok();
                    let record = pilot_store::SessionRecord {
                        task_id: session_key.to_string(),
                        seen_count: session.seen_count as i64,
                        last_viewed_at: session.last_viewed_at,
                        created_at: session.created_at,
                        session_json: json,
                        metadata: None,
                    };
                    if let Err(e) = self.store.save_session(&record) {
                        tracing::error!("save_session {session_key}: {e}");
                    }
                }
            }
            C::StoreDeleteSession { task_id } => {
                if let Err(e) = self.store.delete_session(&task_id) {
                    tracing::error!("delete_session: {e}");
                }
            }
            C::StoreDeleteStaleSessions { task_ids } => {
                for id in task_ids {
                    if let Err(e) = self.store.delete_session(&id) {
                        tracing::error!("delete stale: {e}");
                    }
                }
            }
            C::StoreMarkRead {
                task_id,
                seen_count,
            } => {
                if let Err(e) = self.store.mark_read(&task_id, seen_count) {
                    tracing::warn!("mark_read: {e}");
                }
            }

            // ── External services ──
            C::OpenUrl { url } => {
                crate::notify::open_url(&url);
            }
            C::Notify { title, body } => {
                tokio::spawn(async move {
                    crate::notify::send_notification(&title, &body).await;
                });
            }
            C::HttpPostJson { url, body } => {
                tokio::spawn(async move {
                    let out = tokio::process::Command::new("curl")
                        .args([
                            "-s",
                            "-X",
                            "POST",
                            "-H",
                            "Content-Type: application/json",
                            "-d",
                            &serde_json::to_string(&body).unwrap_or_default(),
                            &url,
                        ])
                        .output()
                        .await;
                    if let Err(e) = out {
                        tracing::error!("http post: {e}");
                    }
                });
            }

            // ── gh CLI ──
            C::RunGhMerge {
                repo,
                pr_number,
                session_key,
            } => {
                let tx = action_tx.clone();
                tokio::spawn(async move {
                    let out = tokio::process::Command::new("gh")
                        .args(["pr", "merge", &pr_number, "--squash", "--repo", &repo])
                        .output()
                        .await;
                    match out {
                        Ok(o) if o.status.success() => {
                            let _ = tx.send(Action::MergeCompleted { session_key });
                            let _ = tx
                                .send(Action::StatusMessage(format!("Merged {repo}#{pr_number}")));
                        }
                        Ok(o) => {
                            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                            let _ = tx.send(Action::StatusMessage(format!("Merge failed: {err}")));
                        }
                        Err(e) => {
                            let _ = tx.send(Action::StatusMessage(format!("Merge error: {e}")));
                        }
                    }
                });
            }
            C::RunGhApprove { repo, pr_number } => {
                let tx = action_tx.clone();
                tokio::spawn(async move {
                    let out = tokio::process::Command::new("gh")
                        .args(["pr", "review", &pr_number, "--approve", "--repo", &repo])
                        .output()
                        .await;
                    match out {
                        Ok(o) if o.status.success() => {
                            let _ = tx.send(Action::StatusMessage(format!(
                                "Approved {repo}#{pr_number}"
                            )));
                        }
                        Ok(o) => {
                            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                            let _ =
                                tx.send(Action::StatusMessage(format!("Approve failed: {err}")));
                        }
                        Err(e) => {
                            let _ = tx.send(Action::StatusMessage(format!("Approve error: {e}")));
                        }
                    }
                });
            }
            C::RunGhUpdateBranch {
                repo,
                pr_number,
                session_key,
            } => {
                // `gh api -X PUT /repos/<repo>/pulls/<num>/update-branch` is
                // the exact same API the github.com "Update branch" button
                // uses. Works for both merge and rebase — GitHub chooses
                // based on the repo's default update strategy.
                let tx = action_tx.clone();
                let repo_clone = repo.clone();
                let pr_clone = pr_number.clone();
                tokio::spawn(async move {
                    let path = format!("/repos/{repo_clone}/pulls/{pr_clone}/update-branch");
                    let out = tokio::process::Command::new("gh")
                        .args(["api", "--method", "PUT", &path])
                        .output()
                        .await;
                    match out {
                        Ok(o) if o.status.success() => {
                            let _ = tx.send(Action::StatusMessage(format!(
                                "Updated branch: {repo_clone}#{pr_clone}"
                            )));
                            // Kick a poll so the sidebar's ⇣ glyph clears
                            // once GitHub recomputes mergeStateStatus.
                            let _ = tx.send(Action::Refresh);
                            let _ = session_key;
                        }
                        Ok(o) => {
                            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                            let _ = tx.send(Action::StatusMessage(format!(
                                "Update branch failed: {err}"
                            )));
                        }
                        Err(e) => {
                            let _ =
                                tx.send(Action::StatusMessage(format!("Update branch error: {e}")));
                        }
                    }
                });
            }
            C::RunGhComment {
                repo,
                pr_number,
                body,
                reply_to_node_id,
            } => {
                let tx = action_tx.clone();
                tokio::spawn(async move {
                    let mut args = vec![
                        "pr".to_string(),
                        "comment".to_string(),
                        pr_number,
                        "--body".to_string(),
                        body,
                        "--repo".to_string(),
                        repo,
                    ];
                    if let Some(id) = reply_to_node_id {
                        args.push("--reply-to".into());
                        args.push(id);
                    }
                    let out = tokio::process::Command::new("gh")
                        .args(&args)
                        .output()
                        .await;
                    match out {
                        Ok(o) if o.status.success() => {
                            let _ = tx.send(Action::StatusMessage("Reply posted".into()));
                        }
                        Ok(o) => {
                            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                            let _ = tx.send(Action::StatusMessage(format!("Error: {err}")));
                        }
                        Err(e) => {
                            let _ = tx.send(Action::StatusMessage(format!("Error: {e}")));
                        }
                    }
                });
            }
            C::RunGhEditCollaborators {
                repo,
                pr_number,
                kind,
                added,
                removed,
            } => {
                use crate::action::PickerKind;
                let (add_flag, remove_flag) = match kind {
                    PickerKind::Reviewer => ("--add-reviewer", "--remove-reviewer"),
                    PickerKind::Assignee => ("--add-assignee", "--remove-assignee"),
                };
                let tx = action_tx.clone();
                tokio::spawn(async move {
                    let mut args = vec![
                        "pr".to_string(),
                        "edit".to_string(),
                        pr_number,
                        "--repo".to_string(),
                        repo,
                    ];
                    for user in &added {
                        args.push(add_flag.into());
                        args.push(user.clone());
                    }
                    for user in &removed {
                        args.push(remove_flag.into());
                        args.push(user.clone());
                    }
                    let out = tokio::process::Command::new("gh")
                        .args(&args)
                        .output()
                        .await;
                    match out {
                        Ok(o) if o.status.success() => {
                            let _ = tx.send(Action::StatusMessage("Collaborators updated".into()));
                        }
                        Ok(o) => {
                            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                            let _ = tx.send(Action::StatusMessage(format!("Error: {err}")));
                        }
                        Err(e) => {
                            let _ = tx.send(Action::StatusMessage(format!("Error: {e}")));
                        }
                    }
                });
            }
            C::FetchCollaborators {
                repo,
                kind,
                session_key,
                pr_number,
            } => {
                let tx = action_tx.clone();
                tokio::spawn(async move {
                    // Fetch repo collaborators via gh CLI.
                    let out = tokio::process::Command::new("gh")
                        .args([
                            "api",
                            &format!("repos/{repo}/collaborators"),
                            "--jq",
                            ".[].login",
                        ])
                        .output()
                        .await;
                    let collaborators: Vec<String> = match out {
                        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                            .lines()
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                        _ => vec![],
                    };
                    let _ = tx.send(Action::CollaboratorsLoaded(Box::new(
                        crate::action::CollaboratorsLoaded {
                            repo,
                            kind,
                            session_key,
                            pr_number,
                            collaborators,
                            current: vec![], // populated by reducer
                        },
                    )));
                });
            }
            C::CheckoutWorktree {
                owner,
                repo,
                branch,
                base,
                session_key,
                then,
            } => {
                let tx = action_tx.clone();
                tracing::info!(
                    "CheckoutWorktree start: {owner}/{repo} branch={branch} base={base:?} key={session_key}"
                );
                tokio::spawn(async move {
                    let started = std::time::Instant::now();
                    let worktrees = pilot_git_ops::WorktreeManager::default_base();
                    let result = match &base {
                        Some(base_branch) => {
                            worktrees
                                .checkout_new_branch(&owner, &repo, &branch, base_branch)
                                .await
                        }
                        None => worktrees.checkout(&owner, &repo, &branch).await,
                    };
                    let elapsed = started.elapsed();
                    match result {
                        Ok(wt) => {
                            tracing::info!(
                                "CheckoutWorktree ok ({elapsed:?}): path={}",
                                wt.path.display()
                            );
                            let _ = tx.send(Action::WorktreeReady {
                                session_key,
                                path: wt.path,
                            });
                            if let Some(a) = then {
                                let _ = tx.send(*a);
                            }
                        }
                        Err(e) => {
                            tracing::error!("CheckoutWorktree failed ({elapsed:?}): {e}");
                            let _ = tx.send(Action::WorktreeFailed {
                                session_key,
                                error: e.to_string(),
                            });
                        }
                    }
                });
            }
            C::FetchDefaultBranch { owner, repo } => {
                let tx = action_tx.clone();
                tokio::spawn(async move {
                    let out = tokio::process::Command::new("gh")
                        .args([
                            "api",
                            &format!("repos/{owner}/{repo}"),
                            "--jq",
                            ".default_branch",
                        ])
                        .output()
                        .await;
                    if let Ok(o) = out
                        && o.status.success()
                    {
                        let b = String::from_utf8_lossy(&o.stdout).trim().to_string();
                        if !b.is_empty() {
                            let _ = tx.send(Action::CacheDefaultBranch {
                                repo: format!("{owner}/{repo}"),
                                branch: b,
                            });
                        }
                    }
                });
            }

            // ── Terminal ──
            C::WriteToTerminal { session_key, bytes } => {
                if let Some(term) = self.terminals.get_mut(&session_key)
                    && let Err(e) = term.write(&bytes)
                {
                    tracing::error!("terminal write: {e}");
                }
            }
            C::ResizeTerminal {
                session_key,
                cols,
                rows,
            } => {
                if let Some(term) = self.terminals.get_mut(&session_key) {
                    let _ = term.resize(pilot_tui_term::PtySize {
                        cols,
                        rows,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
            C::ScrollTerminal { session_key, delta } => {
                if let Some(term) = self.terminals.get_mut(&session_key) {
                    match delta.cmp(&0) {
                        std::cmp::Ordering::Less => term.scroll_up((-delta) as usize),
                        std::cmp::Ordering::Greater => term.scroll_down(delta as usize),
                        std::cmp::Ordering::Equal => term.scroll_reset(),
                    }
                }
            }
            C::CloseTerminal { session_key } => {
                self.close_terminal(&session_key);
            }
            C::SetActiveTab { idx } => {
                self.terminals.set_active_tab(idx);
            }
            C::FocusTerminalPane { session_key } => {
                let key_str: &str = session_key.as_str();
                // Retarget or create a terminal pane for this key, then focus it.
                let existing = self
                    .state
                    .panes
                    .find_pane(|c| matches!(c, PaneContent::Terminal(_)));
                match existing {
                    Some(term_id) => {
                        self.state
                            .panes
                            .set_content(term_id, PaneContent::Terminal(key_str.to_string()));
                        self.state.panes.focus(term_id);
                    }
                    None => {
                        if let Some(detail_id) = self
                            .state
                            .panes
                            .find_pane(|c| matches!(c, PaneContent::Detail(_)))
                        {
                            self.state.panes.focused = detail_id;
                            self.state
                                .panes
                                .split_vertical_above(PaneContent::Terminal(key_str.to_string()));
                        }
                    }
                }
                apply_determined_mode(self);
            }
            C::SpawnTerminal {
                session_key,
                cwd,
                kind,
                focus,
            } => {
                spawn_terminal(self, &session_key, cwd, kind, focus);
            }

            // ── Monitor IO ──
            C::CheckNeedsRebase {
                session_key,
                repo,
                pr_number,
                wt_path,
                default_branch,
            } => {
                let tx = action_tx.clone();
                tokio::spawn(async move {
                    let out = tokio::process::Command::new("gh")
                        .args([
                            "pr",
                            "view",
                            &pr_number,
                            "--repo",
                            &repo,
                            "--json",
                            "mergeable",
                        ])
                        .output()
                        .await;
                    let needs = match out {
                        Ok(o) if o.status.success() => {
                            let json: serde_json::Value =
                                serde_json::from_slice(&o.stdout).unwrap_or_default();
                            json.get("mergeable").and_then(|v| v.as_str()) == Some("CONFLICTING")
                        }
                        _ => false,
                    };
                    let _ = tx.send(Action::NeedsRebaseResult {
                        session_key,
                        needs_rebase: needs,
                        wt_path,
                        default_branch,
                    });
                });
            }
            C::RunRebase {
                session_key,
                wt_path,
                default_branch,
            } => {
                let tx = action_tx.clone();
                tokio::spawn(async move {
                    let fetch = tokio::process::Command::new("git")
                        .current_dir(&wt_path)
                        .args(["fetch", "origin", &default_branch])
                        .output()
                        .await;
                    if !fetch.map(|o| o.status.success()).unwrap_or(false) {
                        tracing::error!("Monitor: git fetch failed for {session_key}");
                        return;
                    }
                    let rebase_target = format!("origin/{default_branch}");
                    let rebase = tokio::process::Command::new("git")
                        .current_dir(&wt_path)
                        .args(["rebase", &rebase_target])
                        .output()
                        .await;
                    match rebase {
                        Ok(o) if o.status.success() => {
                            let push = tokio::process::Command::new("git")
                                .current_dir(&wt_path)
                                .args(["push", "--force-with-lease"])
                                .output()
                                .await;
                            if let Ok(p) = push
                                && p.status.success()
                            {
                                let _ = tx.send(Action::MonitorTick { session_key });
                            } else {
                                tracing::error!("Monitor: push after rebase failed");
                            }
                        }
                        _ => {
                            tracing::warn!("Monitor: rebase failed, aborting");
                            let _ = tokio::process::Command::new("git")
                                .current_dir(&wt_path)
                                .args(["rebase", "--abort"])
                                .output()
                                .await;
                        }
                    }
                });
            }
            C::RefreshLiveTmuxSessions => {
                let tx = action_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let sessions: std::collections::HashSet<String> =
                        list_tmux_sessions().into_iter().collect();
                    let _ = tx.send(Action::TmuxSessionsRefreshed { sessions });
                });
            }
            C::KillTmuxSession { tmux_name } => {
                let tx = action_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let status = std::process::Command::new("tmux")
                        .args(["kill-session", "-t", &tmux_name])
                        .status();
                    match status {
                        Ok(s) if s.success() => {
                            tracing::info!("Killed tmux session: {tmux_name}");
                        }
                        Ok(s) => {
                            tracing::warn!("tmux kill-session {tmux_name} exited with {s}");
                        }
                        Err(e) => {
                            tracing::warn!("tmux kill-session {tmux_name} failed: {e}");
                        }
                    }
                    // Refresh the live set so the sidebar indicator clears.
                    let sessions: std::collections::HashSet<String> =
                        list_tmux_sessions().into_iter().collect();
                    let _ = tx.send(Action::TmuxSessionsRefreshed { sessions });
                });
            }
            C::WriteMonitorContext {
                session_key,
                content,
            } => {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                let context_dir = std::path::PathBuf::from(&home)
                    .join(".pilot")
                    .join("context");
                if let Err(e) = std::fs::create_dir_all(&context_dir) {
                    tracing::error!("Failed to create context dir: {e}");
                } else {
                    let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
                    let safe_key = session_key.replace([':', '/'], "_");
                    let context_file =
                        context_dir.join(format!("{safe_key}_monitor_{timestamp}.md"));
                    if let Err(e) = std::fs::write(&context_file, &content) {
                        tracing::error!("Failed to write monitor context: {e}");
                    } else {
                        let latest = context_dir.join(format!("{safe_key}.md"));
                        let _ = std::fs::remove_file(&latest);
                        let _ = std::fs::copy(&context_file, &latest);
                    }
                }
            }

            // ── Shared state write-through ──
            C::UpdateMonitoredSet {
                session_key,
                monitored,
            } => {
                let mut set = lock_monitored(&self.monitored_sessions);
                if monitored {
                    set.insert(session_key.to_string());
                } else {
                    set.remove(session_key.as_str());
                }
            }
        }
    }

    /// Fully forget a session: memory AND persistent store. Use this in
    /// every path that wants a session gone for good (merged, closed, stale
    /// purge, user-driven removal). Removing only from memory leaves SQLite
    /// stale — on restart the session reloads and the user sees a "zombie".
    pub fn forget_session(&mut self, key: &str) {
        self.state.sessions.remove(key);
        self.close_terminal(key);
        lock_monitored(&self.monitored_sessions).remove(key);
        if self.state.viewing_since.as_ref().map(|(k, _)| k.as_str()) == Some(key) {
            self.state.viewing_since = None;
        }
        if let Some(task_id) = parse_task_id(key)
            && let Err(e) = self.store.delete_session(&task_id)
        {
            tracing::error!("Failed to delete session {key} from store: {e}");
        }
    }

    /// Run cross-cutting invariants that repair the UI state. Called after
    /// every handle_action so callers don't each have to remember to do it.
    /// This is the centralized fix for the "forgot to call X after Y" class
    /// of bugs.
    ///
    /// Enforces:
    /// 1. Reap finished PTYs (e.g. Claude exited because credits ran out).
    ///    Without this, the reader thread has EOF'd but we only swept in the
    ///    Tick handler — so any action between Ticks could render a phantom
    ///    TERM state with no visible terminal.
    /// 2. Pane tree never points at a dead terminal key.
    /// 3. The active terminal tab, if any, is visible in some pane.
    /// 4. `selected` is within nav_items bounds.
    /// 5. `input_mode` is consistent with the focused pane.
    pub fn enforce_invariants(&mut self) {
        // Keep the state's terminal projection fresh first. Reduce reads
        // `state.terminal_index` to make tab/pane decisions; if it's stale,
        // those decisions are wrong.
        self.refresh_terminal_index();

        // (1) Reap finished PTYs. This is the single place where terminal
        // lifecycle cleanup happens — Tick-time cleanup was folded in here so
        // there's no path where a PTY exits mid-frame and the state tied to
        // it (pending prompts, monitor state, Claude "asking" flag) is left
        // half-updated.
        let exited = self.terminals.collect_finished();
        for key in &exited {
            if self.state.pending_prompts.contains_key(key) {
                tracing::warn!("Pending prompt lost for {key} (terminal exited)");
                self.state.status = "Warning: queued prompt lost — terminal exited".into();
            }
            self.state.agent_states.remove(key);
            self.state.pending_prompts.remove(key);
            self.state.notified_asking.remove(key);
            if let Some(s) = self.state.sessions.get_mut(key) {
                s.state = pilot_core::SessionState::Active;
                // Monitor: Claude exited while fixing CI → transition to WaitingCi.
                if let Some(pilot_core::MonitorState::CiFixing { attempt }) = &s.monitor {
                    let attempt = *attempt;
                    s.monitor = Some(pilot_core::MonitorState::WaitingCi {
                        after_attempt: attempt,
                    });
                    tracing::info!(
                        "Monitor: Claude exited for {key}, waiting for CI (attempt {attempt})"
                    );
                }
            }
        }
        if !exited.is_empty() && !self.state.status.starts_with("Warning:") {
            self.state.status = format!("Terminal exited: {}", exited.join(", "));
        }

        // (2-3) Pane tree cleanup.
        let live: std::collections::BTreeSet<String> = self.terminals.keys().cloned().collect();
        let active = self.terminals.active_tab_key().cloned();
        self.state
            .panes
            .enforce_terminal_invariant(&live, active.as_deref());

        // (4) Selection bounds.
        crate::nav::clamp_selected(self);

        // (5) Mode derivation from the now-healthy pane tree.
        apply_determined_mode(self);
    }

    /// Report an error to the status bar.
    #[allow(dead_code)]
    pub fn report_error(&mut self, msg: impl std::fmt::Display) {
        tracing::error!("{msg}");
        self.state.status = format!("Error: {msg}");
    }

    /// Report a status message.
    #[allow(dead_code)]
    pub fn report_status(&mut self, msg: impl Into<String>) {
        self.state.status = msg.into();
    }
}

/// Main event loop.
pub async fn run(app: &mut App) -> Result<()> {
    let (action_tx, mut action_rx) = mpsc::unbounded_channel::<Action>();

    // 1. Crossterm event reader.
    let tx = action_tx.clone();
    tokio::spawn(async move {
        let mut stream = EventStream::new();
        while let Some(Ok(evt)) = stream.next().await {
            let action = match evt {
                CtEvent::Key(key) => {
                    // Drop Release events only. Modern terminals (iTerm2
                    // with modifyOtherKeys, Kitty keyboard protocol) emit
                    // Press + Release for every keystroke — without this
                    // guard, one Tab fires twice and focus skips past
                    // Detail. Repeat is kept so held arrow keys still
                    // scroll lists naturally, and we DO NOT filter more
                    // aggressively than this: terminals that emit only
                    // one of {Press, Repeat} per keystroke still work.
                    if matches!(key.kind, crossterm::event::KeyEventKind::Release) {
                        continue;
                    }
                    Action::Key(key)
                }
                CtEvent::Mouse(mouse) => Action::Mouse(mouse),
                CtEvent::Paste(text) => Action::Paste(text),
                CtEvent::Resize(w, h) => Action::Resize {
                    width: w,
                    height: h,
                },
                _ => continue,
            };
            if tx.send(action).is_err() {
                break;
            }
        }
    });

    // 2. Tick timer.
    let tx = action_tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        loop {
            interval.tick().await;
            if tx.send(Action::Tick).is_err() {
                break;
            }
        }
    });

    // 3. GitHub provider.
    let tx = action_tx.clone();
    let github_creds = CredentialChain::new()
        .with(EnvProvider::new("GH_TOKEN"))
        .with(EnvProvider::new("GITHUB_TOKEN"))
        .with(CommandProvider::new("gh", &["auth", "token"]));

    match github_creds.resolve("github").await {
        Ok(cred) => {
            tracing::info!(source = %cred.source, "GitHub credential resolved");
            let filters: Vec<String> = app
                .state
                .config
                .providers
                .github
                .filters
                .iter()
                .filter_map(|f| f.to_search_qualifier())
                .collect();
            let poll_interval = app.state.config.providers.github.poll_interval;

            match GhClient::from_credential(cred).await {
                Ok(gh) => {
                    let watch_repos: Vec<String> = app
                        .state
                        .config
                        .providers
                        .github
                        .filters
                        .iter()
                        .filter_map(|f| f.watch_repo().map(|r| r.to_string()))
                        .collect();
                    let gh = gh.with_filters(filters).with_watch_repos(watch_repos);
                    app.state.username = gh.username().to_string();
                    app.state.status = format!(
                        "Connected as {} ({})",
                        gh.username(),
                        gh.credential_source()
                    );
                    let mut consumer = app.event_tx.subscribe();
                    let poller = GhPoller::new(gh, app.event_tx.clone(), poll_interval);
                    app.poller_wake = Some(poller.wake_handle());
                    tokio::spawn(async move {
                        poller.run().await;
                    });
                    tokio::spawn(async move {
                        while let Some(event) = consumer.recv().await {
                            if tx.send(Action::ExternalEvent(Box::new(event))).is_err() {
                                break;
                            }
                        }
                    });
                }
                Err(e) => {
                    app.state.status = format!("GitHub auth failed: {e}");
                    app.state.credentials_ok = false;
                }
            }
        }
        Err(e) => {
            app.state.status = format!("No GitHub credential: {e}");
            app.state.credentials_ok = false;
        }
    }

    // ── Reattach tmux sessions that survived the last pilot quit ──
    //
    // On clean quit, pilot doesn't send Ctrl-C to tmux — it just drops the
    // PTY. Tmux sessions keep running detached. Here we look for tmux
    // sessions whose name matches one of our loaded pilot sessions, and
    // spawn a pilot terminal for each (which attaches via `tmux -A`).
    resume_tmux_sessions(app);

    // ── TUI setup ──
    let mut terminal = ratatui::init();
    crossterm::execute!(
        std::io::stdout(),
        ct_event::EnableMouseCapture,
        ct_event::EnableBracketedPaste,
        // Enable Kitty keyboard protocol if the terminal supports it. This
        // is what lets crossterm tell Shift-Enter apart from plain Enter —
        // without it, terminals collapse them and users lose "newline in
        // Claude prompt" capability.
        ct_event::PushKeyboardEnhancementFlags(
            ct_event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | ct_event::KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS,
        ),
    )?;

    // ── Main loop ──
    loop {
        while let Ok(action) = action_rx.try_recv() {
            handle_action(app, action, &action_tx);
            app.enforce_invariants();
        }
        if app.state.should_quit {
            break;
        }

        // Invariants must hold at render time too, not just after actions —
        // otherwise async events (PTY exit on its own thread) can leave us
        // with a stale "TERM" mode between the Tick that reaped the PTY
        // and the next user action.
        app.enforce_invariants();

        terminal.draw(|frame| {
            app.state.last_term_area = (frame.area().width, frame.area().height);
            ui::render(app, frame);
        })?;

        if let Some(action) = action_rx.recv().await {
            handle_action(app, action, &action_tx);
            app.enforce_invariants();
        }
        if app.state.should_quit {
            break;
        }
    }

    crossterm::execute!(
        std::io::stdout(),
        ct_event::DisableMouseCapture,
        ct_event::DisableBracketedPaste,
        ct_event::PopKeyboardEnhancementFlags,
    )?;
    ratatui::restore();
    Ok(())
}

fn handle_action(app: &mut App, action: Action, action_tx: &mpsc::UnboundedSender<Action>) {
    // During the MVC migration, try the pure reducer first. If it handles
    // the action, execute any emitted commands and return. Otherwise fall
    // through to the legacy handler below.
    if crate::reduce::handled_by_reduce(&action) {
        // Remember if this action moves the sidebar cursor — we need to
        // retarget Detail / Terminal leaves afterward so the right pane
        // follows the selection. Otherwise Tab/j/k changes selected but
        // the panes keep showing the OLD session's content, and every
        // Terminal tab looks identical.
        let moves_cursor = matches!(
            &action,
            Action::SelectNext
                | Action::SelectPrev
                | Action::ToggleRepo(_)
                | Action::CollapseSelected
                | Action::ExpandSelected
                | Action::JumpToNextAsking
                | Action::ResetLayout
                | Action::NewSessionConfirm { .. }
                | Action::ToggleMailbox
        );
        let clock = crate::reduce::Clock::now();
        let cmds = crate::reduce::reduce(&mut app.state, action, &clock);
        for cmd in cmds {
            app.execute(cmd, action_tx);
        }
        if moves_cursor {
            update_detail_pane(app);
        }
        return;
    }
    match action {
        Action::Tick => {
            app.state.tick_count += 1;
            // Process pending PTY output for all terminals (needed for ghostty backend).
            app.terminals.process_pending();
            // Update Claude state detection for each Claude terminal.
            {
                use crate::agent_state::{AgentState, detect_state};
                let asking_patterns = &app.state.config.agent.config.asking_patterns;
                let claude_keys: Vec<String> = app
                    .terminals
                    .keys()
                    .filter(|k| {
                        app.terminals
                            .kind(k)
                            .map(|kind| matches!(kind, ShellKind::Claude))
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect();

                for key in &claude_keys {
                    if let Some(term) = app.terminals.get(key) {
                        let prev = app
                            .state
                            .agent_states
                            .get(key)
                            .copied()
                            .unwrap_or(AgentState::Active);
                        // Prefer Claude's lifecycle hooks, but override when
                        // the PTY clearly shows an in-turn dialog that the
                        // hook system doesn't fire for. Examples:
                        //   - Rate-limit / "What do you want to do?" prompts
                        //   - AskUserQuestion tool (no hook — open issue)
                        //   - Custom Claude dialogs mid-turn
                        //
                        // All of these render "Esc to cancel" / "Tab to
                        // amend" at the bottom of a dialog box. Those
                        // strings are product-stable and only appear on
                        // an OPEN dialog, so treat them as a reliable
                        // "asking" override regardless of hook state.
                        let hook = crate::claude_hooks::read_state(key);
                        let silent_ms = term.last_output_at().elapsed().as_millis();
                        let dialog_visible = crate::agent_state::detect_asking(
                            term.recent_output(),
                            asking_patterns,
                        );
                        let new_state = if dialog_visible {
                            AgentState::Asking
                        } else {
                            match hook {
                                Some((crate::claude_hooks::HookState::Working, _))
                                    if silent_ms > 2000 =>
                                {
                                    // Hook says working but PTY is quiet —
                                    // likely an interrupted turn. Fall back.
                                    detect_state(
                                        term.last_output_at(),
                                        term.recent_output(),
                                        prev,
                                        asking_patterns,
                                    )
                                }
                                Some((hs, _)) => match hs {
                                    crate::claude_hooks::HookState::Working => AgentState::Active,
                                    crate::claude_hooks::HookState::Asking => AgentState::Asking,
                                    crate::claude_hooks::HookState::Idle => AgentState::Idle,
                                    crate::claude_hooks::HookState::Stopped => AgentState::Idle,
                                },
                                None => detect_state(
                                    term.last_output_at(),
                                    term.recent_output(),
                                    prev,
                                    asking_patterns,
                                ),
                            }
                        };
                        // Handle transitions.
                        if new_state != prev {
                            if new_state == AgentState::Active {
                                app.state.notified_asking.remove(key);
                            }
                            if new_state == AgentState::Asking
                                && !app.state.notified_asking.contains(key)
                            {
                                app.state.notified_asking.insert(key.clone());
                                let title = app
                                    .state
                                    .sessions
                                    .get(key)
                                    .map(|s| s.display_name.clone())
                                    .unwrap_or_else(|| key.clone());
                                app.state.status = format!("Claude needs input: {title}");
                                // Ring the outer terminal's bell so iTerm2 /
                                // Terminal.app highlights the tab. A plain
                                // BEL is safe across all terminals; OSC 9 is
                                // iTerm-specific and Terminal.app can route
                                // it through AppleScript (opens Script
                                // Editor!), so we don't send OSC anymore.
                                tokio::task::spawn_blocking(|| {
                                    use std::io::Write;
                                    let mut out = std::io::stdout().lock();
                                    let _ = out.write_all(b"\x07");
                                    let _ = out.flush();
                                });
                                let title_clone = title.clone();
                                tokio::spawn(async move {
                                    crate::notify::send_notification(
                                        &format!("pilot: {title_clone}"),
                                        "Claude needs your input",
                                    )
                                    .await;
                                });
                            }
                        }
                        app.state.agent_states.insert(key.clone(), new_state);
                    }
                }

                // Clean up states for removed terminals.
                app.state
                    .agent_states
                    .retain(|k, _| app.terminals.contains_key(k));
            }
            // Inject pending prompts when Claude becomes idle (or after 5s timeout).
            {
                use crate::agent_state::AgentState;
                let ready_keys: Vec<String> = app
                    .state
                    .pending_prompts
                    .keys()
                    .filter(|key| {
                        let is_idle = app
                            .state
                            .agent_states
                            .get(*key)
                            .map(|s| *s == AgentState::Idle)
                            .unwrap_or(false);
                        // Also inject if terminal exists and we've waited 5+ seconds.
                        let has_terminal = app.terminals.contains_key(key);
                        let waited_long = app
                            .state
                            .last_claude_send
                            .map(|t| t.elapsed().as_secs() >= 5)
                            .unwrap_or(false);
                        is_idle || (has_terminal && waited_long)
                    })
                    .cloned()
                    .collect();
                for key in ready_keys {
                    if let Some(prompt) = app.state.pending_prompts.remove(&key)
                        && let Some(term) = app.terminals.get_mut(&key)
                    {
                        if let Err(e) = term.write(prompt.as_bytes()) {
                            tracing::error!(
                                "Terminal write failed for prompt injection into {key}: {e}"
                            );
                        } else if let Err(e) = term.write(b"\r") {
                            tracing::error!(
                                "Terminal write failed for prompt newline into {key}: {e}"
                            );
                        } else {
                            tracing::info!("Injected pending prompt into {key}");
                            app.state.status = "Prompt sent to Claude".into();
                        }
                    }
                }
            }

            // Note: stale sessions are handled by TaskRemoved events from the poller.
            // We don't purge from SQLite on startup — the nav filters hide merged/closed PRs.

            // Auto-mark-read: if cursor has been on the same session for 2+s,
            // mark it read. Delegates to reduce so the Clock is the single
            // time boundary (otherwise this touched `Instant::now()` and
            // `chrono::Utc::now()` directly here).
            let tick_clock = crate::reduce::Clock::now();
            crate::reduce::auto_mark_read_tick(&mut app.state, &tick_clock);

            // Save all sessions every ~3s (30 ticks at 100ms).
            if app.state.tick_count.is_multiple_of(30) && !app.state.sessions.is_empty() {
                app.state.sessions.save_all(app.store.as_ref());
            }
            // Refresh live tmux sessions every ~5s so the sidebar indicator
            // stays honest as sessions come and go outside pilot.
            if app.state.tick_count.is_multiple_of(50) {
                app.execute(crate::command::Command::RefreshLiveTmuxSessions, action_tx);
            }
            // Purge stale SQLite sessions ~5s after first load (50 ticks).
            // This delay ensures all first-poll TaskUpdated events have been processed.
            //
            // Skip if the first poll reported ANY provider error — an
            // incomplete result set would cause us to wipe stored sessions
            // the user still cares about (the "PR reappears then vanishes"
            // bug). Better to leave a stale row than to delete a live one;
            // the user can always hit `x` to close what they don't want.
            if app.state.loaded
                && !app.state.purged_stale
                && app.state.tick_count >= 50
                && !app.state.first_poll_keys.is_empty()
                && !app.state.first_poll_had_errors
            {
                // Remember the cursor target BEFORE mutation. If the first
                // poll had errors, this purge code fires on a *later*
                // successful poll (not at startup) — and teleporting the
                // cursor back to row 0 while the user is doing something
                // is infuriating. Restore by-key afterward.
                let prior_nav = crate::nav::selected_nav_item_from_state(&app.state);

                app.state.purged_stale = true;
                let stale: Vec<String> = app
                    .state
                    .sessions
                    .order()
                    .iter()
                    .filter(|k| k.starts_with("github:") && !app.state.first_poll_keys.contains(*k))
                    .cloned()
                    .collect();
                for key in &stale {
                    tracing::info!("Purging stale session: {key}");
                    app.forget_session(key);
                }
                app.state.first_poll_keys.clear(); // Free memory.
                if !stale.is_empty() {
                    resort_sessions(app);
                }
                // Re-resolve the prior nav item; clamp if it's gone.
                let items = crate::nav::nav_items_from_state(&app.state);
                let new_idx = prior_nav.as_ref().and_then(|prior| match prior {
                    crate::nav::NavItem::Session(k) => items
                        .iter()
                        .position(|i| matches!(i, crate::nav::NavItem::Session(x) if x == k)),
                    crate::nav::NavItem::Repo(r) => items
                        .iter()
                        .position(|i| matches!(i, crate::nav::NavItem::Repo(x) if x == r)),
                });
                if let Some(idx) = new_idx {
                    app.state.selected = idx;
                } else {
                    let n = items.len();
                    app.state.selected = if n == 0 {
                        0
                    } else {
                        app.state.selected.min(n - 1)
                    };
                }
                update_detail_pane(app);
                app.state.status = format!(
                    "Loaded — {} as {}",
                    app.state
                        .config
                        .providers
                        .github
                        .filters
                        .iter()
                        .filter_map(|f| f.org.as_ref())
                        .next()
                        .unwrap_or(&"all repos".to_string()),
                    app.state.username
                );
            } else if app.state.loaded
                && !app.state.purged_stale
                && app.state.tick_count.is_multiple_of(300)
                && app.state.first_poll_had_errors
            {
                // First poll had errors — we can't trust the key set.
                // Clear the error flag and drop the partial key list so
                // the NEXT error-free poll fills in fresh keys and
                // triggers the purge. Logged every 30s so the user
                // sees retry attempts in the log. Note we do NOT set
                // purged_stale=true here — that would permanently
                // suppress purging for this session.
                tracing::info!("Retrying stale-purge accumulation after provider error");
                app.state.first_poll_had_errors = false;
                app.state.first_poll_keys.clear();
            }
            // Terminal reaping + all associated cleanup happens in
            // `enforce_invariants()`, which runs right after this handler
            // returns. Keeping it there means every async-exited PTY
            // (between ticks, from anywhere) gets the full treatment.

            // Periodic merge conflict check for monitored sessions (~30s).
            if app.state.tick_count.is_multiple_of(300) {
                let rebase_candidates: Vec<_> = app
                    .state
                    .sessions
                    .iter()
                    .filter(|(_, s)| matches!(s.monitor, Some(pilot_core::MonitorState::Idle)))
                    .filter_map(|(k, s)| {
                        let wt_path = s.worktree_path.clone()?;
                        let repo = s.primary_task.repo.clone()?;
                        let (_, pr) = s.primary_task.id.key.rsplit_once('#')?;
                        Some((k.clone(), repo, pr.to_string(), wt_path))
                    })
                    .collect();

                for (key, repo, pr_num, wt_path) in rebase_candidates {
                    let Some(default_branch) = app.state.default_branch_cache.get(&repo).cloned()
                    else {
                        // Cache miss — fire a fetch via execute() and skip this cycle.
                        if let Some((owner, r)) = repo.split_once('/') {
                            app.execute(
                                crate::command::Command::FetchDefaultBranch {
                                    owner: owner.to_string(),
                                    repo: r.to_string(),
                                },
                                action_tx,
                            );
                        }
                        continue;
                    };
                    app.execute(
                        crate::command::Command::CheckNeedsRebase {
                            session_key: key.into(),
                            repo,
                            pr_number: pr_num,
                            wt_path,
                            default_branch,
                        },
                        action_tx,
                    );
                }
            }

            // ── CheckingOut watchdog ──
            // A session in CheckingOut with no worktree_path after 60 s
            // means the git process died without signalling us (user quit,
            // SIGKILL, whatever). Auto-fail so the UI stops spinning and
            // the user can retry or Shift-X it. We check infrequently
            // (once per ~2 s at 10 Hz) to keep the cost negligible.
            if app.state.tick_count.is_multiple_of(20) {
                let now = chrono::Utc::now();
                let mut stuck: Vec<(String, String)> = Vec::new();
                for (key, session) in app.state.sessions.iter() {
                    if matches!(session.state, pilot_core::SessionState::CheckingOut)
                        && session.worktree_path.is_none()
                    {
                        let age = now.signed_duration_since(session.primary_task.updated_at);
                        if age.num_seconds() > 60 {
                            stuck.push((
                                key.clone(),
                                format!("timed out after {}s", age.num_seconds()),
                            ));
                        }
                    }
                }
                for (key, reason) in stuck {
                    tracing::warn!("CheckingOut watchdog: {key} {reason}");
                    let _ = action_tx.send(Action::WorktreeFailed {
                        session_key: key.into(),
                        error: format!("checkout {reason}"),
                    });
                }
            }
        }

        Action::Key(key) => {
            use crossterm::event::{KeyCode, KeyModifiers};
            tracing::debug!("KEY {:?} in mode {:?}", key.code, app.state.input_mode);

            // ── Confirmation clearing ──
            // quit_pending and merge_pending are "double-press" guards.
            // Clear them on any key that isn't the confirming key.
            // This runs BEFORE mode dispatch so the guard resets regardless
            // of which overlay is active.
            if app.state.quit_pending && key.code != KeyCode::Char('q') {
                app.state.quit_pending = false;
                app.state.status = String::new();
            }
            if app.state.merge_pending.is_some() && key.code != KeyCode::Char('M') {
                app.state.merge_pending = None;
                app.state.status = String::new();
            }
            if app.state.kill_pending.is_some() && key.code != KeyCode::Char('X') {
                app.state.kill_pending = None;
                app.state.status = String::new();
            }

            // ── Absolute-priority Tab handler ──
            // Tab MUST always cycle panes. Even inside a text overlay Tab
            // gets out: dismiss the overlay first, THEN cycle. Users
            // complain HARD about being trapped in a dialog.
            if key.code == KeyCode::Tab && key.modifiers.is_empty() {
                if let InputMode::TextInput(ref kind) = app.state.input_mode {
                    let cancel = match kind {
                        TextInputKind::Search => Action::SearchClear,
                        TextInputKind::NewSession => Action::NewSessionCancel,
                        TextInputKind::QuickReply => Action::QuickReplyCancel,
                    };
                    handle_action(app, cancel, action_tx);
                }
                handle_action(app, Action::FocusPaneNext, action_tx);
                return;
            }

            // ── Universal overlay-kill: Esc, Ctrl-C, or Ctrl-\ always
            // dismisses whatever overlay is up. Belt-and-suspenders on top
            // of the per-overlay Esc handlers below: if anything ever
            // shadows the per-overlay match we still have an escape hatch.
            let is_dismiss = matches!(
                (key.code, key.modifiers),
                (KeyCode::Esc, _)
                    | (KeyCode::Char('c'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('\\'), KeyModifiers::CONTROL)
            );
            if is_dismiss {
                if let InputMode::TextInput(ref kind) = app.state.input_mode {
                    let cancel = match kind {
                        TextInputKind::Search => Action::SearchClear,
                        TextInputKind::NewSession => Action::NewSessionCancel,
                        TextInputKind::QuickReply => Action::QuickReplyCancel,
                    };
                    handle_action(app, cancel, action_tx);
                    app.state.input_mode = determine_mode(app);
                    return;
                }
            }

            // ── Input mode state machine ──
            // Exactly one arm runs per key event. No fallthrough.
            match app.state.input_mode {
                // 1. Help overlay -- any key dismisses.
                InputMode::Help => {
                    app.state.show_help = false;
                    app.state.input_mode = determine_mode(app);
                }

                // 2. Text input overlays -- search, new session, quick reply.
                InputMode::TextInput(ref kind) => {
                    match kind {
                        TextInputKind::Search => {
                            match key.code {
                                KeyCode::Esc => {
                                    handle_action(app, Action::SearchClear, action_tx);
                                    app.state.input_mode = determine_mode(app);
                                }
                                KeyCode::Enter => {
                                    app.state.search_active = false; // keep filter, exit typing
                                    app.state.input_mode = determine_mode(app);
                                }
                                KeyCode::Backspace => {
                                    handle_action(app, Action::SearchBackspace, action_tx);
                                }
                                KeyCode::Char(c) => {
                                    handle_action(app, Action::SearchInput(c), action_tx);
                                }
                                _ => {}
                            }
                        }
                        TextInputKind::NewSession => {
                            match key.code {
                                KeyCode::Esc => {
                                    handle_action(app, Action::NewSessionCancel, action_tx);
                                    app.state.input_mode = determine_mode(app);
                                }
                                KeyCode::Enter => {
                                    let desc =
                                        app.state.new_session_input.clone().unwrap_or_default();
                                    if desc.trim().is_empty() {
                                        // Silent no-op was the worst option: user thinks
                                        // Enter is broken. Keep the overlay up but surface
                                        // a status so they know what's missing.
                                        app.state.status =
                                            "Type a branch name before Enter (Esc to cancel)"
                                                .into();
                                    } else {
                                        handle_action(
                                            app,
                                            Action::NewSessionConfirm { description: desc },
                                            action_tx,
                                        );
                                        app.state.input_mode = determine_mode(app);
                                    }
                                }
                                KeyCode::Backspace => {
                                    if let Some(ref mut input) = app.state.new_session_input {
                                        input.pop();
                                    }
                                }
                                KeyCode::Char(c) => {
                                    if let Some(ref mut input) = app.state.new_session_input {
                                        input.push(c);
                                    }
                                }
                                _ => {}
                            }
                        }
                        TextInputKind::QuickReply => match key.code {
                            KeyCode::Esc => {
                                handle_action(app, Action::QuickReplyCancel, action_tx);
                                app.state.input_mode = determine_mode(app);
                            }
                            KeyCode::Enter => {
                                let body = app
                                    .state
                                    .quick_reply_input
                                    .as_ref()
                                    .map(|(_, t, _)| t.clone())
                                    .unwrap_or_default();
                                if !body.trim().is_empty() {
                                    handle_action(
                                        app,
                                        Action::QuickReplyConfirm { body },
                                        action_tx,
                                    );
                                }
                                app.state.input_mode = determine_mode(app);
                            }
                            KeyCode::Backspace => {
                                if let Some((_, ref mut text, _)) = app.state.quick_reply_input {
                                    text.pop();
                                }
                            }
                            KeyCode::Char(c) => {
                                if let Some((_, ref mut text, _)) = app.state.quick_reply_input {
                                    text.push(c);
                                }
                            }
                            _ => {}
                        },
                    }
                }

                // 4. Picker overlay -- reviewer/assignee selection.
                InputMode::Picker => {
                    match key.code {
                        KeyCode::Esc => {
                            handle_action(app, Action::PickerCancel, action_tx);
                            app.state.input_mode = determine_mode(app);
                        }
                        KeyCode::Enter => {
                            // If nothing was changed yet, toggle the current item first.
                            if let Some(ref mut picker) = app.state.picker {
                                let any_changed =
                                    picker.items.iter().any(|i| i.selected != i.was_selected);
                                if !any_changed {
                                    let filtered = picker.filtered_indices();
                                    if let Some(&real_idx) = filtered.get(picker.cursor) {
                                        picker.items[real_idx].selected =
                                            !picker.items[real_idx].selected;
                                    }
                                }
                            }
                            handle_action(app, Action::PickerConfirm, action_tx);
                            app.state.input_mode = determine_mode(app);
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            if let Some(ref mut picker) = app.state.picker {
                                let count = picker.filtered_indices().len();
                                if count > 0 {
                                    picker.cursor = (picker.cursor + 1) % count;
                                }
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            if let Some(ref mut picker) = app.state.picker {
                                let count = picker.filtered_indices().len();
                                if count > 0 {
                                    picker.cursor = if picker.cursor == 0 {
                                        count - 1
                                    } else {
                                        picker.cursor - 1
                                    };
                                }
                            }
                        }
                        KeyCode::Char(' ') => {
                            if let Some(ref mut picker) = app.state.picker {
                                let filtered = picker.filtered_indices();
                                if let Some(&real_idx) = filtered.get(picker.cursor) {
                                    picker.items[real_idx].selected =
                                        !picker.items[real_idx].selected;
                                }
                            }
                        }
                        KeyCode::Backspace => {
                            if let Some(ref mut picker) = app.state.picker {
                                picker.filter.pop();
                                picker.cursor = 0;
                            }
                        }
                        KeyCode::Char(c) => {
                            if let Some(ref mut picker) = app.state.picker {
                                picker.filter.push(c);
                                picker.cursor = 0;
                            }
                        }
                        _ => {}
                    }
                }

                // 5. Pane prefix -- one-shot pane operation after Ctrl-w.
                InputMode::PanePrefix => {
                    let mapped = keys::map_pane_prefix(key);
                    app.state.input_mode = determine_mode(app);
                    if !matches!(mapped, Action::None) {
                        handle_action(app, mapped, action_tx);
                    }
                }

                // 6. Terminal / Normal / Detail -- regular key mapping.
                InputMode::Normal | InputMode::Detail | InputMode::Terminal => {
                    // ALWAYS derive the mode fresh from the pane tree here —
                    // `app.state.input_mode` may be stale if async state changed
                    // since the last `enforce_invariants()`. The pane tree
                    // is the source of truth; trust it.
                    let derived = determine_mode(app);
                    if derived != app.state.input_mode {
                        app.state.input_mode = derived.clone();
                    }
                    let effective_mode = derived.to_key_mode();
                    let mapped = keys::map_key(key, effective_mode);
                    match mapped {
                        Action::WaitingPrefix => {
                            app.state.input_mode = InputMode::PanePrefix;
                        }
                        Action::None if derived == InputMode::Terminal => {
                            // Keys go to the terminal the user SEES. The
                            // renderer picks the selected session's
                            // terminal (render_right_pane), so route to
                            // the same target. The pane tree's Terminal
                            // leaf key can lag the sidebar cursor — if
                            // we routed there, keystrokes would hit an
                            // invisible PTY.
                            let term_key = app
                                .selected_session_key()
                                .filter(|k| app.terminals.contains_key(k))
                                .or_else(|| {
                                    app.focused_terminal_key()
                                        .filter(|k| app.terminals.contains_key(k))
                                });
                            if let Some(term_key) = term_key {
                                if let Some(term) = app.terminals.get_mut(&term_key) {
                                    term.scroll_reset();
                                    if let Some(bytes) = keys::key_to_bytes(&key)
                                        && let Err(e) = term.write(&bytes)
                                    {
                                        tracing::error!("PTY write failed: {e}");
                                        app.state.status =
                                            format!("Error: terminal write failed: {e}");
                                    }
                                } else {
                                    tracing::warn!(
                                        "Terminal mode but no live terminal for {term_key}"
                                    );
                                    app.state.input_mode = InputMode::Normal;
                                }
                            } else {
                                tracing::warn!("Terminal mode but no selected/focused terminal");
                                app.state.input_mode = InputMode::Normal;
                            }
                        }
                        other => {
                            handle_action(app, other, action_tx);
                        }
                    }
                }
            }
        }

        Action::Mouse(mouse) => {
            use crossterm::event::{MouseButton, MouseEventKind};
            let (term_w, _term_h) = app.state.last_term_area;
            // The sidebar border is at sidebar_pct% of the terminal width.
            let border_col = (term_w as u32 * app.state.sidebar_pct as u32 / 100) as u16;

            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    // Check if clicking on the sidebar/detail divider (±1 col).
                    if mouse.column.abs_diff(border_col) <= 1 {
                        app.state.drag_resize = true;
                        return;
                    }

                    // Click on sidebar area → select item or toggle repo.
                    if mouse.column < border_col {
                        set_mode(app, InputMode::Normal);
                        // Map click row to a sidebar item.
                        // Row 0 = title bar, 1 = search, 2+ = items.
                        let click_row = mouse.row.saturating_sub(2) as usize;
                        handle_sidebar_click(app, click_row, action_tx);
                    } else {
                        // Click on detail/terminal area.
                        // Check if there's a running terminal for the selected session.
                        let has_term = app
                            .selected_session_key()
                            .and_then(|k| app.terminals.get(&k).map(|_| ()))
                            .is_some();
                        // Click in upper part → detail, lower part → terminal (if exists).
                        let right_area_height = _term_h;
                        let detail_cutoff = right_area_height * 30 / 100; // 30% for detail
                        if has_term && mouse.row > detail_cutoff {
                            focus_terminal_pane(app);
                            apply_determined_mode(app);
                        } else {
                            set_mode(app, InputMode::Detail);
                        }
                        update_detail_pane(app);
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if app.state.drag_resize {
                        // Resize sidebar by mouse position.
                        if term_w > 0 {
                            let new_pct = (mouse.column as u32 * 100 / term_w as u32) as u16;
                            app.state.sidebar_pct = new_pct.clamp(20, 80);
                        }
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    app.state.drag_resize = false;
                }
                MouseEventKind::ScrollUp => {
                    forward_scroll(app, mouse.column, mouse.row, true);
                }
                MouseEventKind::ScrollDown => {
                    forward_scroll(app, mouse.column, mouse.row, false);
                }
                _ => {}
            }
        }

        Action::Paste(text) => {
            // Same routing rule as keystrokes: selected first, focused
            // as fallback — targets what's rendered.
            let term_key = app
                .selected_session_key()
                .filter(|k| app.terminals.contains_key(k))
                .or_else(|| {
                    app.focused_terminal_key()
                        .filter(|k| app.terminals.contains_key(k))
                });
            if let Some(term_key) = term_key
                && let Some(term) = app.terminals.get_mut(&term_key)
            {
                let result = term
                    .write(b"\x1b[200~")
                    .and_then(|()| term.write(text.as_bytes()))
                    .and_then(|()| term.write(b"\x1b[201~"));
                if let Err(e) = result {
                    tracing::error!("Terminal paste write failed: {e}");
                }
            }
        }

        // ── Detail pane ──
        Action::FixWithClaude => {
            fix_or_reply_with_claude(app, action_tx, "fix");
        }
        Action::ReplyWithClaude => {
            fix_or_reply_with_claude(app, action_tx, "reply");
        }

        // Everything else is handled by `reduce` at the top of this function.
        // If we fall here, it's a logic bug — log and swallow.
        other => {
            tracing::warn!("handle_action fallthrough: {other:?} — should have been reduced");
        }
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────────

/// Compute the tmux session name we use for a pilot session key.
/// Must match the naming inside `spawn_terminal`.
fn tmux_name_for(session_key: &str) -> String {
    session_key.replace([':', '/'], "_")
}

/// List active tmux sessions (via `tmux list-sessions -F '#{session_name}'`).
/// Returns an empty vec on any error — tmux might not be installed, or no
/// sessions are running. Neither case is a problem: we just skip auto-resume.
fn list_tmux_sessions() -> Vec<String> {
    match std::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// On startup, find tmux sessions matching persisted pilot sessions and
/// spawn a pilot terminal for each so the user can continue where they
/// left off. Assumes `ShellKind::Claude` (the common case); users who were
/// in a shell can close the pane and press `b` to reopen as a shell.
fn resume_tmux_sessions(app: &mut App) {
    let live_tmux: std::collections::HashSet<String> = list_tmux_sessions().into_iter().collect();
    app.state.live_tmux_sessions.clone_from(&live_tmux);
    if live_tmux.is_empty() {
        return;
    }

    // Iterate over persisted session keys, collecting the ones to resume.
    let to_resume: Vec<(String, std::path::PathBuf)> = app
        .state
        .sessions
        .iter()
        .filter(|(_, s)| {
            s.primary_task.state == pilot_core::TaskState::Open
                || s.primary_task.state == pilot_core::TaskState::Draft
                || s.primary_task.state == pilot_core::TaskState::InReview
                || s.primary_task.state == pilot_core::TaskState::InProgress
        })
        .filter_map(|(key, s)| {
            if !live_tmux.contains(&tmux_name_for(key)) {
                return None;
            }
            let cwd = s.worktree_path.clone().unwrap_or_else(|| {
                std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
            });
            Some((key.clone(), cwd))
        })
        .collect();

    if to_resume.is_empty() {
        return;
    }

    let count = to_resume.len();
    for (key, cwd) in to_resume {
        tracing::info!("Resuming tmux session for {key}");
        // focus=false — starting in INBOX, not terminal.
        spawn_terminal(app, &key, cwd, ShellKind::Claude, false);
    }
    app.state.status = format!(
        "Resumed {count} tmux session{}",
        if count == 1 { "" } else { "s" }
    );

    // `spawn_terminal` focuses each new Terminal pane as it lands, which
    // leaves the app in Terminal mode at startup. Pilot should always open
    // in the inbox — move focus back to the Inbox pane.
    if let Some(inbox_id) = app
        .state
        .panes
        .find_pane(|c| matches!(c, crate::pane::PaneContent::Inbox))
    {
        app.state.panes.focus(inbox_id);
    }
    app.state.input_mode = InputMode::Normal;
}

pub(crate) fn spawn_terminal(
    app: &mut App,
    session_key: &str,
    cwd: std::path::PathBuf,
    kind: ShellKind,
    focus: bool,
) {
    // Bail if the session was removed between the user action and here
    // (e.g. PR got merged/closed mid-spawn). Without this, env vars would
    // silently default and the terminal would come up disconnected from any PR.
    if !app.state.sessions.contains_key(session_key) {
        app.state.status = format!("spawn_terminal: session '{session_key}' no longer exists");
        return;
    }

    let (cols, rows) = app.state.last_term_area;
    let size = PtySize {
        rows: rows.max(10),
        cols: cols.max(20),
        pixel_width: 0,
        pixel_height: 0,
    };

    // Build the inner command (claude or shell).
    let inner_cmd: Vec<String> = match kind {
        ShellKind::Claude => app.state.config.agent.config.spawn_command(false),
        ShellKind::Shell => vec![app.state.config.shell.command.clone()],
    };

    // Wrap in tmux so the process survives pilot quit.
    // -A: attach if exists, create if not.
    let tmux_name = tmux_name_for(session_key);
    let inner_joined = inner_cmd.join(" ");
    let cmd_strs: Vec<String> = vec![
        "tmux".into(),
        "new-session".into(),
        "-A".into(),
        "-s".into(),
        tmux_name,
        inner_joined,
    ];
    let cmd: Vec<&str> = cmd_strs.iter().map(|s| s.as_str()).collect();

    // Hide tmux chrome — no status bar.
    let _ = std::process::Command::new("tmux")
        .args(["set-option", "-g", "status", "off"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // Mark that Claude has been used in this session.
    if matches!(kind, ShellKind::Claude)
        && let Some(session) = app.state.sessions.get_mut(session_key)
    {
        session.had_claude = true;
    }

    // Env vars that get inherited by the inner Claude / shell process.
    let session = app.state.sessions.get(session_key);
    let task_id = session.map(|s| s.task_id.to_string()).unwrap_or_default();
    let pr_number = session
        .and_then(|s| s.primary_task.id.key.rsplit_once('#'))
        .map(|(_, n)| n.to_string())
        .unwrap_or_default();
    let repo = session
        .and_then(|s| s.primary_task.repo.as_ref())
        .cloned()
        .unwrap_or_default();

    let mut env = vec![
        ("PILOT_SESSION".to_string(), task_id),
        ("PILOT_PR_NUMBER".to_string(), pr_number),
        ("PILOT_REPO".to_string(), repo),
    ];
    if matches!(kind, ShellKind::Claude) {
        // Ask Claude Code to use its fullscreen renderer. Without this
        // Claude uses the primary screen and ignores mouse events, so
        // our SGR wheel forwarding has no receiver. See
        // https://code.claude.com/docs/en/fullscreen.
        env.push(("CLAUDE_CODE_NO_FLICKER".to_string(), "1".to_string()));
    }

    // Install Claude Code hooks so we get deterministic state
    // transitions (working / asking / idle) instead of guessing from
    // PTY output. Only meaningful for the Claude kind; harmless for
    // Shell since Claude never runs there to read the settings file.
    if matches!(kind, ShellKind::Claude) {
        crate::claude_hooks::clear_state(session_key);
        if let Err(e) = crate::claude_hooks::install_hooks(&cwd, session_key) {
            tracing::warn!("Failed to install Claude hooks for {session_key}: {e}");
        }
    }

    let term_result = TermSession::spawn(&cmd, size, Some(&cwd), env);

    match term_result {
        Ok(term) => {
            // Remember the pre-spawn focused pane so we can restore focus
            // when `focus == false` (auto-attach).
            let prior_focus = app.state.panes.focused;

            app.terminals.insert(session_key.to_string(), term, kind);
            if let Some(session) = app.state.sessions.get_mut(session_key) {
                session.state = SessionState::Working;
            }
            // Auto-split: if no terminal pane exists, split the detail pane.
            let has_term_pane = app
                .state
                .panes
                .find_pane(|c| matches!(c, PaneContent::Terminal(_)))
                .is_some();
            if !has_term_pane {
                if let Some(detail_id) = app
                    .state
                    .panes
                    .find_pane(|c| matches!(c, PaneContent::Detail(_)))
                {
                    app.state.panes.focused = detail_id;
                    app.state
                        .panes
                        .split_vertical_above(PaneContent::Terminal(session_key.to_string()));
                }
            } else if let Some(term_id) = app
                .state
                .panes
                .find_pane(|c| matches!(c, PaneContent::Terminal(_)))
            {
                app.state
                    .panes
                    .set_content(term_id, PaneContent::Terminal(session_key.to_string()));
            }

            if focus {
                focus_terminal_pane(app);
            } else {
                app.state.panes.focus(prior_focus);
            }
            apply_determined_mode(app);
            app.state.status = match kind {
                ShellKind::Claude => format!("Claude Code started in {}", cwd.display()),
                ShellKind::Shell => format!("Shell started in {}", cwd.display()),
            };
        }
        Err(e) => {
            if let Some(session) = app.state.sessions.get_mut(session_key) {
                session.state = SessionState::Active;
            }
            app.state.status = format!("Terminal spawn failed: {e}");
            tracing::error!("Terminal spawn failed: {e}");
        }
    }
}

/// Construct a context prompt from selected comments and paste it into the
/// active Claude terminal session.
/// Route a trackpad / mouse-wheel scroll to the right place:
///   1. Terminal with mouse tracking ON → forward as an SGR mouse wheel
///      event, so tmux / Claude Code / vim / less handle it natively.
///   2. Terminal on alt-screen WITHOUT mouse tracking → libghostty has
///      no scrollback to move, and forwarding anything (arrow keys!)
///      corrupts the input. Do nothing.
///   3. Terminal on the primary screen (plain shell) → scroll
///      libghostty's scrollback buffer.
///   4. Detail pane (no focused terminal) → scroll the markdown.
///
/// Matches what xterm / iTerm2 do natively, so habits transfer.
fn forward_scroll(app: &mut App, column: u16, row: u16, up: bool) {
    // Same routing rule as keystrokes: selected first, focused fallback
    // — targets the rendered terminal.
    let term_key = app
        .selected_session_key()
        .filter(|k| app.terminals.contains_key(k))
        .or_else(|| {
            app.focused_terminal_key()
                .filter(|k| app.terminals.contains_key(k))
        });
    let Some(term_key) = term_key else {
        if up {
            app.state.detail_scroll = app.state.detail_scroll.saturating_sub(3);
        } else {
            app.state.detail_scroll = app.state.detail_scroll.saturating_add(3);
        }
        return;
    };
    let Some(term) = app.terminals.get_mut(&term_key) else {
        return;
    };

    // Scroll routing by what the inner app actually supports:
    //
    // Primary screen (plain shell): libghostty scrollback.
    //
    // Alt-screen + mouse tracking on (fullscreen Claude Code,
    // tmux with proper forwarding, vim with `:set mouse=a`):
    //   → SGR-1006 wheel events, the app scrolls natively.
    //
    // Alt-screen + no mouse tracking (default Claude Code v2.x,
    // tmux without `mouse on`, less, man): send PgUp / PgDn.
    //   Arrow keys are wrong — Claude maps them to input history.
    //   Wheel bytes as literal input are worse — they get typed
    //   into the prompt (see anthropics/claude-code#42297).
    //   PgUp/PgDn are universally treated as "scroll the viewport"
    //   across Claude fullscreen, tmux copy-mode, and pagers.
    if term.in_alternate_screen() {
        if term.is_mouse_tracking() {
            let button = if up { 64 } else { 65 };
            let x = column.saturating_add(1);
            let y = row.saturating_add(1);
            let seq = format!("\x1b[<{button};{x};{y}M");
            let chunk = seq.repeat(3);
            let _ = term.write(chunk.as_bytes());
        } else {
            let key = if up { b"\x1b[5~" } else { b"\x1b[6~" };
            let mut chunk = Vec::with_capacity(key.len() * 3);
            for _ in 0..3 {
                chunk.extend_from_slice(key);
            }
            let _ = term.write(&chunk);
        }
    } else if up {
        term.scroll_up(3);
    } else {
        term.scroll_down(3);
    }
}

fn fix_or_reply_with_claude(app: &mut App, action_tx: &mpsc::UnboundedSender<Action>, mode: &str) {
    // Debounce: ignore if we sent something in the last 1.5s.
    let now = std::time::Instant::now();
    if let Some(last) = app.state.last_claude_send
        && now.duration_since(last).as_millis() < 1500
    {
        app.state.status = "Wait — Claude was just fed. Press again in a sec.".into();
        return;
    }

    let Some(session_key) = app.selected_session_key() else {
        app.state.status = "No session selected".into();
        return;
    };

    // If no terminal running, open one first and queue the prompt.
    let just_spawned = !app.terminals.contains_key(&session_key);
    if just_spawned {
        handle_action(app, Action::OpenSession(ShellKind::Claude), action_tx);
    }

    let Some(session) = app.state.sessions.get(&session_key) else {
        return;
    };

    let task = &session.primary_task;

    // Detect what needs fixing: conflicts, CI failure, review comments, or combination.
    let ci_failing = task.ci == pilot_core::CiStatus::Failure;
    let has_failed_checks = task
        .checks
        .iter()
        .any(|c| c.status == pilot_core::CiStatus::Failure);
    let has_conflicts = task.has_conflicts;

    // Gather selected comments (or all unread if none selected).
    let indices: Vec<usize> = if app.state.selected_comments.is_empty() {
        (0..session.unread_count()).collect()
    } else {
        let mut v: Vec<usize> = app.state.selected_comments.iter().copied().collect();
        v.sort();
        v
    };

    let has_comments = !indices.is_empty();

    // Must have SOMETHING to fix.
    if !ci_failing && !has_comments && !has_conflicts {
        app.state.status = "Nothing to fix — CI green, no conflicts, no unread comments".into();
        return;
    }

    // Build context-aware prompt.
    let mut prompt = String::new();

    // Determine the task description based on what's broken.
    let mut issues: Vec<&str> = vec![];
    if has_conflicts {
        issues.push("resolve merge conflicts");
    }
    if ci_failing {
        issues.push("fix CI failures");
    }
    if has_comments && mode == "fix" {
        issues.push("address review comments");
    }
    if has_comments && mode == "reply" {
        issues.push("draft replies to review comments");
    }

    let action_word = if issues.is_empty() {
        "investigate this PR"
    } else {
        // Leak is fine — this is a one-off prompt string.
        &*issues.join(" AND ").leak()
    };

    prompt.push_str(&format!("# Task: {action_word}\n\n"));
    prompt.push_str("## PR\n\n");
    prompt.push_str(&format!("- **Title:** {}\n", task.title));
    prompt.push_str(&format!("- **URL:** {}\n", task.url));
    if let Some(ref branch) = task.branch {
        prompt.push_str(&format!("- **Branch:** `{branch}`\n"));
    }
    prompt.push_str(&format!("- **CI:** {:?}\n", task.ci));
    prompt.push_str(&format!("- **Review:** {:?}\n", task.review));
    if has_conflicts {
        prompt.push_str("- **Merge conflicts:** YES\n");
    }

    // Merge conflict instructions.
    if has_conflicts {
        prompt.push_str("\n## Merge Conflicts\n\n");
        prompt.push_str("This PR has merge conflicts with the base branch.\n");
        prompt.push_str("1. Run `git fetch origin main && git rebase origin/main`\n");
        prompt.push_str("2. Resolve any conflicts\n");
        prompt.push_str("3. Force-push the rebased branch (`git push --force-with-lease`)\n\n");
    }

    // CI failure details.
    if ci_failing || has_failed_checks {
        prompt.push_str("\n## CI Failures\n\n");
        let failed: Vec<_> = task
            .checks
            .iter()
            .filter(|c| c.status == pilot_core::CiStatus::Failure)
            .collect();
        if failed.is_empty() {
            prompt.push_str("CI is failing but no individual check details available.\n");
            prompt.push_str("Run `gh pr checks <num>` to see details.\n");
        } else {
            for check in &failed {
                prompt.push_str(&format!("- **FAILED: {}**", check.name));
                if let Some(ref url) = check.url {
                    prompt.push_str(&format!(" — [view logs]({url})"));
                }
                prompt.push('\n');
            }
        }
        prompt.push_str("\nInvestigate the failing checks, read the logs, and fix the code.\n");
    }

    // Review comments (if any).
    if has_comments {
        prompt.push_str("\n## Review Comments\n\n");
        prompt.push_str(
            "Each inline comment below has a `thread_id`. Reply with a \
             threaded GraphQL reply so it lands on the right file/line — \
             use `gh api graphql -f query='mutation { \
             addPullRequestReviewThreadReply(input: {pullRequestReviewThreadId: \\\"THREAD_ID\\\", body: \\\"...\\\"}) { comment { id } } }'`. \
             Address each comment individually.\n\n",
        );
        for &idx in &indices {
            if let Some(activity) = session.activity.get(idx) {
                let kind_label = match activity.kind {
                    pilot_core::ActivityKind::Comment => "Comment",
                    pilot_core::ActivityKind::Review => "Review",
                    pilot_core::ActivityKind::StatusChange => "Status",
                    pilot_core::ActivityKind::CiUpdate => "CI",
                };
                let quoted_body = activity
                    .body
                    .lines()
                    .map(|line| format!("> {line}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                prompt.push_str(&format!(
                    "### {kind_label} from {} ({})\n\n",
                    activity.author,
                    pilot_core::time::time_ago(&activity.created_at),
                ));
                if let Some(path) = &activity.path {
                    match activity.line {
                        Some(n) => prompt.push_str(&format!("- **File:** `{path}:{n}`\n")),
                        None => prompt.push_str(&format!("- **File:** `{path}`\n")),
                    }
                }
                if let Some(tid) = &activity.thread_id {
                    prompt.push_str(&format!("- **thread_id:** `{tid}`\n"));
                }
                if let Some(hunk) = &activity.diff_hunk
                    && !hunk.is_empty()
                {
                    prompt.push_str("\n```diff\n");
                    prompt.push_str(hunk);
                    if !hunk.ends_with('\n') {
                        prompt.push('\n');
                    }
                    prompt.push_str("```\n");
                }
                prompt.push_str(&format!("\n{quoted_body}\n\n"));
            }
        }
    }

    prompt.push_str("\n## Instructions\n\n");
    prompt.push_str("Use standard `git` and `gh` commands. Env vars in this shell:\n");
    prompt.push_str("- `PILOT_REPO` (owner/repo), `PILOT_PR_NUMBER`, `PILOT_SESSION`\n\n");

    if mode == "fix" {
        prompt.push_str("After making code changes:\n");
        prompt.push_str("1. Make the changes locally (you're already in the worktree)\n");
        prompt.push_str("2. `git push` (or `git push --force-with-lease` after a rebase)\n");
        prompt.push_str("3. `gh pr checks` to confirm CI\n");
        prompt
            .push_str("4. Reply to each comment via `gh api graphql` (threaded) as shown above\n");
    } else {
        prompt.push_str(
            "Draft concise, professional replies. Post each reply as a \
threaded GraphQL reply using the thread_id above so it lands on the right file/line.\n",
        );
    }

    // Write context to file with timestamp to avoid race conditions.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let context_dir = std::path::PathBuf::from(&home)
        .join(".pilot")
        .join("context");
    if let Err(e) = std::fs::create_dir_all(&context_dir) {
        tracing::error!("Failed to create context dir: {e}");
        app.state.status = format!("Failed to create context dir: {e}");
        return;
    }
    let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
    let safe_key = session_key.replace([':', '/'], "_");
    let context_file = context_dir.join(format!("{safe_key}_{timestamp}.md"));
    if let Err(e) = std::fs::write(&context_file, &prompt) {
        app.state.status = format!("Failed to write context file: {e}");
        return;
    }
    // Also write a stable "latest" symlink for pilot_get_context.
    let latest = context_dir.join(format!("{safe_key}.md"));
    let _ = std::fs::remove_file(&latest); // intentional cleanup
    if let Err(e) = std::fs::copy(&context_file, &latest) {
        tracing::warn!("Failed to copy context to latest: {e}");
    }

    // Queue the prompt — it will be injected when Claude is idle.
    tracing::info!("Queued prompt for {session_key} ({} bytes)", prompt.len());
    app.state
        .pending_prompts
        .insert(session_key.clone(), prompt);
    app.state.selected_comments.clear();
    app.state.last_claude_send = Some(now);

    if let Some(idx) = app
        .terminals
        .tab_order()
        .iter()
        .position(|k| k == &session_key)
    {
        app.terminals.set_active_tab(idx);
    }
    // Terminal may exist in the map but have no visible pane (e.g. user
    // closed the pane earlier). ensure_terminal_pane_for puts it back.
    ensure_terminal_pane_for(app, &session_key);

    {
        let n = indices.len();
        app.state.status = format!(
            "Queued {n} comment{} for Claude to {mode}",
            if n == 1 { "" } else { "s" }
        );
    }
}

/// Focus the terminal pane if one exists in the layout. Called after tab
/// switches so the user ends up in Terminal mode consistently.
pub(crate) fn focus_terminal_pane(app: &mut App) {
    if let Some(id) = app
        .state
        .panes
        .find_pane(|c| matches!(c, PaneContent::Terminal(_)))
    {
        app.state.panes.focus(id);
    }
}

/// Guarantee that a `Terminal(session_key)` pane is in the layout and focused.
/// If a terminal pane already exists, retarget it to `session_key`. Otherwise
/// split the detail pane to create one. This is the invariant: whenever a
/// terminal exists in `app.terminals` and we want the user to see it, call
/// this — otherwise you get the silent "prompt sent but no terminal" bug.
pub(crate) fn ensure_terminal_pane_for(app: &mut App, session_key: &str) {
    let existing = app
        .state
        .panes
        .find_pane(|c| matches!(c, PaneContent::Terminal(_)));
    match existing {
        Some(term_id) => {
            app.state
                .panes
                .set_content(term_id, PaneContent::Terminal(session_key.to_string()));
            app.state.panes.focus(term_id);
        }
        None => {
            if let Some(detail_id) = app
                .state
                .panes
                .find_pane(|c| matches!(c, PaneContent::Detail(_)))
            {
                app.state.panes.focused = detail_id;
                app.state
                    .panes
                    .split_vertical_above(PaneContent::Terminal(session_key.to_string()));
            }
        }
    }
    apply_determined_mode(app);
}

/// Update the detail pane content to match the selected session.
///
/// Delegates to `PaneManager::sync_for_selection` (pure, unit-tested
/// in crates/app/src/pane.rs) so the hard cases — leaf re-creation
/// after a kill, focus preservation — are covered by tests.
pub(crate) fn update_detail_pane(app: &mut App) {
    let selected = app.selected_session_key();
    let has_term = selected
        .as_ref()
        .map(|k| app.terminals.contains_key(k))
        .unwrap_or(false);
    app.state
        .panes
        .sync_for_selection(selected.as_deref(), has_term);
}

/// Parse a session key (`"source:key"`) back into a `TaskId` — used when
/// the caller only has the string form but needs to talk to the store.
fn parse_task_id(key: &str) -> Option<pilot_core::TaskId> {
    key.split_once(':').map(|(source, k)| pilot_core::TaskId {
        source: source.to_string(),
        key: k.to_string(),
    })
}

/// Lock the shared monitored-sessions set, recovering from poisoning.
///
/// Helper tasks (worktree checkout, poller) run under tokio and a panic in
/// any of them would poison this mutex forever. Recovering via
/// `parking_lot::Mutex::lock` is infallible — no poisoning, no `PoisonError`
/// handling, faster under contention than `std::sync::Mutex`.
fn lock_monitored(
    m: &crate::state::SharedMonitoredSessions,
) -> parking_lot::MutexGuard<'_, std::collections::HashSet<String>> {
    m.lock()
}

/// Reset detail pane state when switching sessions and auto-update detail.
pub(crate) fn reset_detail_state(app: &mut App) {
    app.state.detail_scroll = 0;
    app.state.detail_cursor = 0;
    app.state.selected_comments.clear();
    update_detail_pane(app);
}

/// Set the input mode, respecting overlay precedence.
/// When an overlay is active (Help, TextInput, Picker),
/// input_mode is left alone -- the overlay owns it.
fn set_mode(app: &mut App, mode: InputMode) {
    if !app.state.input_mode.is_overlay() {
        app.state.input_mode = mode;
    }
}

/// Determine the input mode from pane content and apply it.
pub(crate) fn apply_determined_mode(app: &mut App) {
    let mode = determine_mode(app);
    set_mode(app, mode);
}

/// Determine the input mode based on what the focused pane contains.
///
/// Mode is derived strictly from the focused pane. If the focused pane isn't
/// a terminal, we are NOT in Terminal mode even if a terminal exists in some
/// other tab/pane — otherwise keystrokes get swallowed by a PTY the user
/// can't see.
pub(crate) fn determine_mode(app: &App) -> InputMode {
    match app.state.panes.focused_content() {
        Some(PaneContent::Terminal(key)) => {
            if app.terminals.contains_key(&key) {
                InputMode::Terminal
            } else {
                InputMode::Normal
            }
        }
        Some(PaneContent::Detail(_)) => InputMode::Detail,
        _ => InputMode::Normal,
    }
}

/// Get a PaneContent::Detail for the currently selected session.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_task_id_roundtrip() {
        let tid = parse_task_id("github:owner/repo#123").unwrap();
        assert_eq!(tid.source, "github");
        assert_eq!(tid.key, "owner/repo#123");
        // Round-trip through Display.
        assert_eq!(tid.to_string(), "github:owner/repo#123");
    }

    #[test]
    fn parse_task_id_with_colon_in_key() {
        // Only the FIRST colon separates source/key — the rest is part of key.
        let tid = parse_task_id("linear:ENG-123:extra").unwrap();
        assert_eq!(tid.source, "linear");
        assert_eq!(tid.key, "ENG-123:extra");
    }

    #[test]
    fn parse_task_id_malformed() {
        assert!(parse_task_id("no-colon").is_none());
        assert!(parse_task_id("").is_none());
    }
}
