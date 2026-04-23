//! Pure reducer: given `&mut State` and an `Action`, mutate the state and
//! return a list of `Command`s that describe the effects the shell should
//! perform. No IO, no tokio, no `&mut App` — only data.
//!
//! During the MVC migration, `handle_action` in `app.rs` routes actions that
//! have been migrated here through `reduce`, and leaves the rest on the old
//! path. As more actions migrate, `handle_action` collapses to a thin
//! `let cmds = reduce(...); for c in cmds { self.execute(c); }`.

use std::path::PathBuf;
use std::time::Instant;

use chrono::{DateTime, Utc};

use crate::action::{Action, PickerKind};
use crate::command::Command;
use crate::input::{InputMode, TextInputKind};
use crate::pane::{Direction, PaneContent};
use crate::picker::{PickerState, build_picker_items};
use crate::state::State;
use pilot_events::EventKind;

/// Max attempts to auto-fix CI before giving up.
pub(crate) const MONITOR_MAX_CI_RETRIES: u32 = 3;

/// Auto-mark-read sweep. Called from the shell's Tick handler every frame.
///
/// Pure: only reads `state.selected_session_key()`-equivalent (derived from
/// nav) and mutates `viewing_since` / the session's read state. Takes a
/// `Clock` so the 2-second threshold is testable.
///
/// Rule: if the cursor has been sitting on the same session for ≥2 seconds,
/// mark it read. Moving the cursor resets the timer.
pub fn auto_mark_read_tick(state: &mut State, clock: &Clock) {
    let Some(key) = selected_key(state) else {
        state.viewing_since = None;
        return;
    };
    match &state.viewing_since {
        Some((viewed_key, since)) if viewed_key == &key => {
            if clock.instant.saturating_duration_since(*since).as_secs() >= 2
                && let Some(session) = state.sessions.get_mut(&key)
                && session.unread_count() > 0
            {
                session.mark_read(clock.chrono);
            }
        }
        _ => {
            state.viewing_since = Some((key, clock.instant));
        }
    }
}

/// Bundle of "ambient" inputs the reducer needs but which would otherwise
/// be IO / non-deterministic. Centralizing them here means tests can pin
/// every time-and-environment-dependent decision to a known value.
#[derive(Debug, Clone)]
pub struct Clock {
    /// Monotonic `Instant` — used for debounces, auto-mark-read timers,
    /// `last_claude_send`.
    pub instant: Instant,
    /// Wall clock — used for snoozed-until, task.updated_at, session keys.
    pub chrono: DateTime<Utc>,
    /// Default working directory when no worktree is available yet.
    /// Injected so reduce doesn't read `$HOME` directly.
    pub default_cwd: PathBuf,
}

impl Clock {
    /// Real-time clock for production. Tests should build one explicitly.
    pub fn now() -> Self {
        Self {
            instant: Instant::now(),
            chrono: Utc::now(),
            default_cwd: std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/tmp")),
        }
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        Self::now()
    }
}

/// Reduce an Action against a State, returning the commands the shell
/// should execute.
///
/// `clock` bundles monotonic time, wall-clock, and environment defaults.
/// Tests can construct a pinned Clock to assert exact state transitions.
pub fn reduce(state: &mut State, action: Action, clock: &Clock) -> Vec<Command> {
    let mut cmds: Vec<Command> = Vec::new();
    match action {
        // ── Sidebar navigation ──
        Action::SelectNext => {
            let nav_count = crate::nav::nav_items_from_state(state).len();
            if nav_count > 0 {
                state.selected = (state.selected + 1).min(nav_count - 1);
            }
            reset_detail(state);
        }
        Action::SelectPrev => {
            state.selected = state.selected.saturating_sub(1);
            reset_detail(state);
        }

        // ── Status / overlays ──
        Action::StatusMessage(msg) => {
            state.status = msg;
        }
        Action::ToggleHelp => {
            state.show_help = !state.show_help;
            state.input_mode = if state.show_help {
                InputMode::Help
            } else {
                InputMode::Normal
            };
        }

        // ── Search input (pure string editing) ──
        Action::SearchActivate => {
            state.search_active = true;
            state.input_mode = InputMode::TextInput(TextInputKind::Search);
        }
        Action::SearchInput(c) => {
            state.search_query.push(c);
            recompute_filter(state);
        }
        Action::SearchBackspace => {
            state.search_query.pop();
            if state.search_query.is_empty() {
                state.filtered_keys = None;
            } else {
                recompute_filter(state);
            }
        }
        Action::SearchClear => {
            state.search_query.clear();
            state.filtered_keys = None;
            state.search_active = false;
            state.selected = 0;
            if matches!(state.input_mode, InputMode::TextInput(TextInputKind::Search)) {
                state.input_mode = InputMode::Normal;
            }
        }

        // ── Snooze / unsnooze current session ──
        Action::Snooze => {
            if let Some(key) = selected_key(state)
                && let Some(session) = state.sessions.get_mut(&key) {
                    if session.is_snoozed(clock.chrono) {
                        session.snoozed_until = None;
                        state.status = format!("Unsnoozed: {}", session.display_name);
                    } else {
                        session.snoozed_until =
                            Some(clock.chrono + chrono::Duration::hours(4));
                        state.status = format!("Snoozed for 4h: {}", session.display_name);
                    }
                }
        }

        // ── Panes (pure — no terminal lifecycle) ──
        Action::FocusPaneNext => {
            state.panes.focus_next();
            skip_empty_detail(state);
        }
        Action::FocusPanePrev => {
            state.panes.focus_prev();
            skip_empty_detail(state);
        }
        Action::FocusPaneUp | Action::FocusPaneDown
        | Action::FocusPaneLeft | Action::FocusPaneRight => {
            let dir = match action {
                Action::FocusPaneUp => Direction::Up,
                Action::FocusPaneDown => Direction::Down,
                Action::FocusPaneLeft => Direction::Left,
                _ => Direction::Right,
            };
            // We don't know the true screen rect here; use a nominal one.
            // focus_direction only needs relative positions, and the shell
            // recomputes the exact layout on the next render.
            state.panes.focus_direction(dir, ratatui::prelude::Rect::default());
        }
        Action::SplitVertical => {
            let content = selected_key(state)
                .map(PaneContent::Detail)
                .unwrap_or(PaneContent::Empty);
            state.panes.split_vertical(content);
        }
        Action::SplitHorizontal => {
            let content = selected_key(state)
                .map(PaneContent::Detail)
                .unwrap_or(PaneContent::Empty);
            state.panes.split_horizontal(content);
        }
        Action::ClosePane => {
            state.panes.close_focused();
        }
        Action::ResizePane(delta) => {
            state.panes.resize_focused(delta);
        }
        Action::FullscreenToggle => {
            state.panes.fullscreen_toggle();
        }

        // ── Sidebar tree collapse ──
        Action::ToggleRepo(repo) => {
            let repo = if repo.is_empty() {
                match crate::nav::selected_nav_item_from_state(state) {
                    Some(crate::nav::NavItem::Repo(r)) => r,
                    Some(crate::nav::NavItem::Session(k)) => state
                        .sessions
                        .get(&k)
                        .map(|s| s.repo.clone())
                        .unwrap_or_default(),
                    None => String::new(),
                }
            } else {
                repo
            };
            if !repo.is_empty() {
                // Toggle collapse: remove if present, else insert.
                if !state.collapsed_repos.remove(&repo) {
                    state.collapsed_repos.insert(repo.clone());
                }
                // Keep cursor on the repo header.
                let items = crate::nav::nav_items_from_state(state);
                if let Some(idx) = items
                    .iter()
                    .position(|i| matches!(i, crate::nav::NavItem::Repo(r) if r == &repo))
                {
                    state.selected = idx;
                }
            }
        }
        Action::ToggleSession(key) => {
            let key = if key.is_empty() {
                selected_key(state).unwrap_or_default()
            } else {
                key
            };
            if !key.is_empty() && !state.collapsed_sessions.remove(&key) {
                state.collapsed_sessions.insert(key);
            }
        }

        // ── Detail pane cursor ──
        Action::DetailCursorUp => {
            state.detail_cursor = state.detail_cursor.saturating_sub(1);
            mark_cursor_comment_read(state);
        }
        Action::DetailCursorDown => {
            if let Some(key) = selected_key(state)
                && let Some(session) = state.sessions.get(&key) {
                    let max = session.activity.len().saturating_sub(1);
                    state.detail_cursor = (state.detail_cursor + 1).min(max);
                }
            mark_cursor_comment_read(state);
        }
        Action::ToggleCommentSelect => {
            let idx = state.detail_cursor;
            if !state.selected_comments.insert(idx) {
                state.selected_comments.remove(&idx);
            }
            mark_cursor_comment_read(state);
        }
        Action::SelectAllComments => {
            if let Some(key) = selected_key(state)
                && let Some(session) = state.sessions.get(&key) {
                    if state.selected_comments.len() == session.activity.len() {
                        state.selected_comments.clear();
                    } else {
                        state.selected_comments = (0..session.activity.len()).collect();
                    }
                }
        }

        // ── Quit (2-press guard if terminals alive) ──
        Action::Quit => {
            if state.terminal_index.keys.is_empty() || state.quit_pending {
                tracing::info!(
                    "Detaching from {} tmux session(s)",
                    state.terminal_index.keys.len()
                );
                // Persist everything before exit.
                for key in state.sessions.order().to_vec() {
                    cmds.push(Command::StoreSaveSession { session_key: key.into() });
                }
                state.should_quit = true;
            } else {
                state.quit_pending = true;
                let n = state.terminal_index.keys.len();
                state.status = format!(
                    "Quit? {n} terminal{} running. Press q again to confirm.",
                    if n == 1 { "" } else { "s" }
                );
            }
        }

        // ── Refresh (wake the poller) ──
        Action::Refresh => {
            state.status = "Refreshing…".into();
            cmds.push(Command::WakePoller);
        }

        // ── Mark-read current session ──
        Action::MarkRead => {
            if let Some(key) = selected_key(state) {
                let cmd = state.sessions.get_mut(&key).map(|session| {
                    session.mark_read(clock.chrono);
                    session.primary_task.needs_reply = false;
                    Command::StoreMarkRead {
                        task_id: session.task_id.clone(),
                        seen_count: i64::try_from(session.seen_count).unwrap_or(i64::MAX),
                    }
                });
                cmds.extend(cmd);
                state.status = "Marked as read".into();
            }
        }

        // ── OpenInBrowser ──
        Action::OpenInBrowser => {
            if let Some(key) = selected_key(state)
                && let Some(session) = state.sessions.get(&key) {
                    let url = session.primary_task.url.clone();
                    if url.is_empty() {
                        state.status = "No URL for this session".into();
                    } else {
                        state.status = "Opened in browser".into();
                        cmds.push(Command::OpenUrl { url });
                    }
                }
        }

        // ── FocusDetail: jump focus onto the Detail pane ──
        Action::FocusDetail => {
            if let Some(detail_id) = state
                .panes
                .find_pane(|c| matches!(c, PaneContent::Detail(_)))
            {
                state.panes.focus(detail_id);
            }
        }

        // ── JumpToNextAsking: cycle to next session needing input ──
        Action::JumpToNextAsking => {
            use crate::nav::NavItem;
            use crate::agent_state::AgentState;
            let items = crate::nav::nav_items_from_state(state);
            let cur = state.selected;
            let n = items.len();
            // Scan forward from the current position, wrapping. Skip the
            // starting index so repeated presses cycle through all asking
            // sessions rather than getting stuck on the current one.
            let asking: Option<(usize, String)> = (1..=n)
                .find_map(|off| {
                    let idx = (cur + off) % n;
                    match items.get(idx)? {
                        NavItem::Session(k)
                            if state.agent_states.get(k) == Some(&AgentState::Asking) =>
                        {
                            Some((idx, k.clone()))
                        }
                        _ => None,
                    }
                });
            if let Some((idx, key)) = asking {
                state.selected = idx;
                // Also switch the active tab to that session so Tab
                // lands the user on its terminal, and emit
                // FocusTerminalPane so the user can reply immediately.
                if let Some(tab_idx) =
                    state.terminal_index.tab_order.iter().position(|k| k == &key)
                {
                    state.terminal_index.active_tab = Some(tab_idx);
                    cmds.push(Command::SetActiveTab { idx: tab_idx });
                    cmds.push(Command::FocusTerminalPane {
                        session_key: key.into(),
                    });
                }
                state.status = "Jumped to session needing input".into();
            } else {
                state.status = "No sessions waiting for input".into();
            }
        }

        // ── OpenCiChecks: open the PR's /checks tab ──
        Action::OpenCiChecks => {
            if let Some(key) = selected_key(state)
                && let Some(session) = state.sessions.get(&key) {
                    let base = &session.primary_task.url;
                    if base.is_empty() {
                        state.status = "No URL for this session".into();
                    } else {
                        // github.com/o/r/pull/N → github.com/o/r/pull/N/checks
                        let url = format!("{}/checks", base.trim_end_matches('/'));
                        state.status = "Opened CI checks".into();
                        cmds.push(Command::OpenUrl { url });
                    }
                }
        }

        // ── Merge (two-press guard, optimistic) ──
        Action::MergePr => {
            if let Some(key) = selected_key(state) {
                let Some(session) = state.sessions.get(&key) else { return cmds };
                let repo = session.primary_task.repo.clone().unwrap_or_default();
                let pr_number = session
                    .primary_task
                    .id
                    .key
                    .rsplit_once('#')
                    .map(|(_, n)| n.to_string())
                    .unwrap_or_default();
                let review = format!("{:?}", session.primary_task.review);
                if repo.is_empty() || pr_number.is_empty() {
                    state.status = "Cannot merge: no PR info".into();
                    return cmds;
                }
                if state.merge_pending.as_deref() == Some(key.as_str()) {
                    // Second M — execute.
                    state.merge_pending = None;
                    state.status = format!("Merging {repo}#{pr_number}…");
                    if !state.input_mode.is_overlay() {
                        state.input_mode = InputMode::Normal;
                    }
                    if let Some(s) = state.sessions.get_mut(&key) {
                        s.primary_task.state = pilot_core::TaskState::Merged;
                    }
                    cmds.push(Command::RunGhMerge {
                        repo,
                        pr_number,
                        session_key: key.into(),
                    });
                } else {
                    state.merge_pending = Some(key);
                    state.status = format!(
                        "Merge? {repo}#{pr_number} (review: {review}). Press M again."
                    );
                }
            }
        }
        Action::MergeCompleted { session_key } => {
            if let Some(session) = state.sessions.get_mut(&session_key) {
                session.primary_task.state = pilot_core::TaskState::Merged;
                let task_id = session.primary_task.id.clone();
                cmds.push(Command::StoreDeleteSession { task_id });
            }
        }

        // ── Approve (only Reviewer or Assignee may approve) ──
        Action::ApprovePr => {
            let Some(key) = selected_key(state) else { return cmds };
            let Some(session) = state.sessions.get(&key) else { return cmds };
            let role = session.primary_task.role;
            let current_review = session.primary_task.review;
            let repo = session.primary_task.repo.clone().unwrap_or_default();
            let pr_number = session
                .primary_task
                .id
                .key
                .rsplit_once('#')
                .map(|(_, n)| n.to_string())
                .unwrap_or_default();
            let title = session.display_name.clone();
            if !matches!(
                role,
                pilot_core::TaskRole::Reviewer | pilot_core::TaskRole::Assignee
            ) {
                state.status =
                    format!("Approve: you are {role:?} on this PR — need Reviewer or Assignee");
                return cmds;
            }
            if repo.is_empty() || pr_number.is_empty() {
                state.status = "Approve: no PR info".into();
                return cmds;
            }
            if current_review == pilot_core::ReviewStatus::Approved {
                state.status = format!("{title}: already approved");
                return cmds;
            }
            if let Some(s) = state.sessions.get_mut(&key) {
                s.primary_task.review = pilot_core::ReviewStatus::Approved;
            }
            state.status = format!("Approving {repo}#{pr_number}…");
            cmds.push(Command::RunGhApprove { repo, pr_number });
        }

        // ── UpdateBranch (two-press confirmation) ──
        Action::UpdateBranch => {
            let Some(key) = selected_key(state) else { return cmds };
            let Some(session) = state.sessions.get(&key) else { return cmds };
            let task = &session.primary_task;
            let repo = task.repo.clone().unwrap_or_default();
            let pr_number = task
                .id
                .key
                .rsplit_once('#')
                .map(|(_, n)| n.to_string())
                .unwrap_or_default();
            if repo.is_empty() || pr_number.is_empty() {
                state.status = "UpdateBranch: no PR info".into();
                return cmds;
            }
            if !task.is_behind_base {
                state.status = format!("{repo}#{pr_number}: branch is not behind base");
                return cmds;
            }
            if state.update_branch_pending.as_deref() == Some(key.as_str()) {
                state.update_branch_pending = None;
                state.status = format!("Updating {repo}#{pr_number} from base…");
                cmds.push(Command::RunGhUpdateBranch {
                    repo,
                    pr_number,
                    session_key: key.into(),
                });
            } else {
                state.update_branch_pending = Some(key);
                state.status = format!(
                    "Update branch on {repo}#{pr_number}? Press Shift-U again to confirm."
                );
            }
        }

        // ── SlackNudge ──
        Action::SlackNudge => {
            let Some(key) = selected_key(state) else { return cmds };
            let Some(session) = state.sessions.get(&key) else { return cmds };
            let Some(webhook_url) = state.config.slack.webhook_url.clone() else {
                state.status =
                    "No Slack webhook configured — set slack.webhook_url in ~/.pilot/config.yaml"
                        .into();
                return cmds;
            };
            let task = &session.primary_task;
            if task.reviewers.is_empty() {
                state.status = "No reviewers to nudge".into();
                return cmds;
            }
            let reviewer_list = task.reviewers.join(", ");
            let text = format!(
                "Friendly reminder: *{}* is waiting for review.\n<{}|View PR>\nReviewers: {reviewer_list}",
                task.title, task.url
            );
            state.status = format!("Sending Slack nudge to {reviewer_list}…");
            cmds.push(Command::HttpPostJson {
                url: webhook_url,
                body: serde_json::json!({ "text": text }),
            });
        }

        // ── ResetLayout — nuclear escape hatch. Wipe the pane tree back
        // to defaults, put focus on the sidebar, force Normal input mode,
        // then let `sync_for_selection` rebuild Detail/Terminal leaves on
        // the next tick based on the current selection.
        Action::ResetLayout => {
            state.panes = crate::pane::PaneManager::default_layout();
            state.input_mode = crate::input::InputMode::Normal;
            state.status = "Layout reset".into();
        }

        // ── WorktreeFailed — clear the CheckingOut state so the sidebar
        // stops spinning, keep the session row around so the user can kill
        // it with Shift-X or retry once the name is fixed.
        Action::WorktreeFailed { session_key, error } => {
            if let Some(session) = state.sessions.get_mut(&session_key) {
                session.state = pilot_core::SessionState::Active;
                state.status = format!("Worktree failed for {}: {error}", session.display_name);
            } else {
                state.status = format!("Worktree failed: {error}");
            }
        }

        // ── WorktreeReady (async checkout completed) ──
        Action::WorktreeReady { session_key, path } => {
            if let Some(session) = state.sessions.get_mut(&session_key) {
                session.worktree_path = Some(path);
                session.state = pilot_core::SessionState::Active;
                state.status = format!("Worktree ready: {}", session.display_name);
                if session.monitor.is_some() {
                    cmds.push(Command::DispatchAction(Action::MonitorTick {
                        session_key: session_key.clone(),
                    }));
                }
            }
        }

        // ── Open a session terminal ──
        Action::OpenSession(shell_kind) => {
            let Some(key) = selected_key(state) else {
                state.status = "No session selected".into();
                return cmds;
            };
            // Already has terminal → switch to the tab AND focus the pane.
            // Without the explicit focus command, `enforce_terminal_invariant`
            // short-circuits when the session's terminal is already visible
            // (e.g. sidebar nav already retargeted it), so keystrokes keep
            // going to the sidebar.
            if state.terminal_index.contains_key(&key) {
                if let Some(idx) = state.terminal_index.tab_order.iter().position(|k| k == &key) {
                    state.terminal_index.active_tab = Some(idx);
                    cmds.push(Command::SetActiveTab { idx });
                }
                cmds.push(Command::FocusTerminalPane { session_key: key.into() });
                return cmds;
            }
            let worktree_path = state.sessions.get(&key).and_then(|s| s.worktree_path.clone());
            if let Some(path) = worktree_path {
                cmds.push(Command::SpawnTerminal {
                    session_key: key.into(),
                    cwd: path,
                    kind: shell_kind,
                    focus: true, // user-initiated
                });
                return cmds;
            }
            // Need to checkout.
            if let Some(session) = state.sessions.get_mut(&key) {
                let repo = session.primary_task.repo.clone();
                let branch = session.primary_task.branch.clone();
                session.state = pilot_core::SessionState::CheckingOut;
                state.status = format!("Checking out worktree for {}…", session.display_name);
                if let (Some(repo_full), Some(branch)) = (repo, branch) {
                    if let Some((owner, repo)) = repo_full.split_once('/') {
                        cmds.push(Command::CheckoutWorktree {
                            owner: owner.to_string(),
                            repo: repo.to_string(),
                            branch,
                            base: None,
                            session_key: key.clone().into(),
                            then: Some(Box::new(Action::OpenSession(shell_kind))),
                        });
                    }
                } else {
                    cmds.push(Command::SpawnTerminal {
                        session_key: key.into(),
                        cwd: clock.default_cwd.clone(),
                        kind: shell_kind,
                        focus: true,
                    });
                }
            }
        }

        // ── Tabs ──
        Action::NextTab => {
            let n = state.terminal_index.tab_order.len();
            if n > 0 {
                let cur = state.terminal_index.active_tab.unwrap_or(0);
                let next = (cur + 1) % n;
                state.terminal_index.active_tab = Some(next);
                cmds.push(Command::SetActiveTab { idx: next });
                if let Some(key) = state.terminal_index.tab_order.get(next).cloned() {
                    cmds.push(Command::FocusTerminalPane { session_key: key.into() });
                }
                sync_selected_to_active_tab(state);
            }
        }
        Action::PrevTab => {
            let n = state.terminal_index.tab_order.len();
            if n > 0 {
                let cur = state.terminal_index.active_tab.unwrap_or(0);
                let next = (cur + n - 1) % n;
                state.terminal_index.active_tab = Some(next);
                cmds.push(Command::SetActiveTab { idx: next });
                if let Some(key) = state.terminal_index.tab_order.get(next).cloned() {
                    cmds.push(Command::FocusTerminalPane { session_key: key.into() });
                }
                sync_selected_to_active_tab(state);
            }
        }
        Action::GoToTab(n) => {
            let idx = n.saturating_sub(1);
            if idx < state.terminal_index.tab_order.len() {
                state.terminal_index.active_tab = Some(idx);
                cmds.push(Command::SetActiveTab { idx });
                if let Some(key) = state.terminal_index.tab_order.get(idx).cloned() {
                    cmds.push(Command::FocusTerminalPane { session_key: key.into() });
                }
                sync_selected_to_active_tab(state);
            }
        }
        Action::CloseTab => {
            if let Some(key) = state.terminal_index.active_key().cloned() {
                if let Some(session) = state.sessions.get_mut(&key) {
                    session.state = pilot_core::SessionState::Active;
                }
                cmds.push(Command::CloseTerminal { session_key: key.into() });
            }
        }
        Action::KillSession => {
            // Prefer the focused-pane terminal (what the user is actually
            // looking at). Fall back to active tab, then to the selected
            // session — so `X` works even when the tmux session is alive
            // but no pilot PTY is attached to it yet.
            let target = selected_key(state);
            let tab_key = state.terminal_index.active_key().cloned();
            let key = tab_key.or(target);
            if let Some(key) = key {
                let tmux_name = key.replace([':', '/'], "_");
                state.live_tmux_sessions.remove(&tmux_name);

                // For source="local" sessions (created via `N` with no PR
                // backing them), Shift-X means DELETE: they have no PR to
                // track, their tmux might never have existed (stuck
                // checkout), and leaving the row in the sidebar gives the
                // user no way out. For source="github" PRs we only kill
                // the terminal — the PR row stays.
                let is_local = state
                    .sessions
                    .get(&key)
                    .map(|s| s.primary_task.id.source == "local")
                    .unwrap_or(false);

                if let Some(session) = state.sessions.get_mut(&key) {
                    session.state = pilot_core::SessionState::Active;
                }
                // Wipe ALL per-session transient state — if we only called
                // CloseTerminal, those keys would be cleaned, but if the
                // tmux session was alive without a pilot PTY attached, a
                // queued prompt / agent-state / asking-flag would linger.
                state.pending_prompts.remove(&key);
                state.notified_asking.remove(&key);
                state.agent_states.remove(&key);
                cmds.push(Command::KillTmuxSession { tmux_name });
                if state.terminal_index.contains_key(&key) {
                    cmds.push(Command::CloseTerminal { session_key: key.clone().into() });
                }

                if is_local {
                    let task_id_opt = state.sessions.get(&key).map(|s| s.primary_task.id.clone());
                    state.sessions.remove(&key);
                    if let Some(task_id) = task_id_opt {
                        cmds.push(Command::StoreDeleteSession { task_id });
                    }
                    // Step cursor back so it doesn't dangle past end-of-list.
                    state.selected = state.selected.saturating_sub(1);
                    state.status = format!("Removed local session {key}");
                } else {
                    state.status = format!("Killed tmux session for {key}");
                }
            }
        }

        // ── Resize (screen dimensions updated) ──
        Action::Resize { width, height } => {
            state.last_term_area = (width, height);
        }

        // ── Cache default branch ──
        Action::CacheDefaultBranch { repo, branch } => {
            state.default_branch_cache.insert(repo, branch);
        }

        // ── Waiting prefix (Ctrl-w just pressed) ──
        Action::WaitingPrefix => {
            state.input_mode = InputMode::PanePrefix;
        }

        // ── No-op ──
        Action::None => {}

        // ── New session overlay ──
        Action::NewSession => {
            state.new_session_input = Some(String::new());
            state.input_mode = InputMode::TextInput(TextInputKind::NewSession);
            state.status = "New worktree — type branch name, Enter to create + open Claude".into();
        }
        Action::NewSessionCancel => {
            state.new_session_input = None;
            if matches!(state.input_mode, InputMode::TextInput(TextInputKind::NewSession)) {
                state.input_mode = InputMode::Normal;
            }
            state.status = String::new();
        }
        Action::NewSessionConfirm { description } => {
            state.new_session_input = None;
            if matches!(state.input_mode, InputMode::TextInput(TextInputKind::NewSession)) {
                state.input_mode = InputMode::Normal;
            }

            // Strip whitespace; reject empty.
            let raw = description.trim().to_string();
            if raw.is_empty() {
                state.status = "New session: empty branch name".into();
                return cmds;
            }
            // Slugify: git doesn't allow spaces, `..`, `~`, `^`, `:`, `?`,
            // `*`, `[`, `\`, `@{`, or a leading `-`. Convert whitespace to
            // `-` and bounce the input if it still contains illegal chars
            // instead of silently munging something the user wrote.
            let branch_name = raw
                .chars()
                .map(|c| if c.is_whitespace() { '-' } else { c })
                .collect::<String>();
            const BAD_CHARS: &[char] = &['~', '^', ':', '?', '*', '[', '\\'];
            if branch_name.starts_with('-')
                || branch_name.contains("..")
                || branch_name.contains("@{")
                || branch_name.chars().any(|c| BAD_CHARS.contains(&c))
            {
                state.status =
                    format!("New session: `{branch_name}` is not a valid git branch name");
                return cmds;
            }

            // Inherit repo + base from wherever the sidebar cursor is:
            //   - On a session row: use that session's repo + base_branch
            //   - On a repo header: use that repo; base is the first
            //     session's base_branch (they're all off the same trunk) or
            //     the cached default branch, or "main".
            //   - On nothing: bail.
            let (owner, repo, base_branch) = {
                let Some(repo_full) = infer_repo_context(state) else {
                    state.status =
                        "New session: select a PR or a repo header first".into();
                    return cmds;
                };
                let Some((owner, repo)) = repo_full.split_once('/') else {
                    state.status = format!("New session: unrecognized repo `{repo_full}`");
                    return cmds;
                };
                let base = state
                    .sessions
                    .values()
                    .find(|s| s.primary_task.repo.as_deref() == Some(repo_full.as_str()))
                    .and_then(|s| s.primary_task.base_branch.clone())
                    .or_else(|| state.default_branch_cache.get(&repo_full).cloned())
                    .unwrap_or_else(|| "main".to_string());
                (owner.to_string(), repo.to_string(), base)
            };

            let repo_full = format!("{owner}/{repo}");
            let key = format!("local:{repo_full}#{branch_name}");

            // Avoid collision: if a session already exists with this key, bail.
            if state.sessions.contains_key(&key) {
                state.status = format!("Session {key} already exists");
                return cmds;
            }

            let task = pilot_core::Task {
                id: pilot_core::TaskId {
                    source: "local".into(),
                    key: key.clone(),
                },
                title: branch_name.clone(),
                body: None,
                state: pilot_core::TaskState::Open,
                role: pilot_core::TaskRole::Author,
                ci: pilot_core::CiStatus::None,
                review: pilot_core::ReviewStatus::None,
                checks: vec![],
                unread_count: 0,
                url: String::new(),
                repo: Some(repo_full.clone()),
                branch: Some(branch_name.clone()),
                base_branch: Some(base_branch.clone()),
                updated_at: clock.chrono,
                labels: vec![],
                reviewers: vec![],
                assignees: vec![],
                auto_merge_enabled: false,
                is_in_merge_queue: false,
                has_conflicts: false,
                is_behind_base: false,
                node_id: None,
                needs_reply: false,
                last_commenter: None,
                recent_activity: vec![],
                additions: 0,
                deletions: 0,
            };
            let mut session = pilot_core::Session::new_at(task, clock.chrono);
            session.state = pilot_core::SessionState::CheckingOut;
            state.sessions.insert(key.clone(), session);
            state.status = format!(
                "Creating worktree {repo_full}#{branch_name} off {base_branch}…"
            );

            // Move the sidebar cursor onto the new session so when the
            // subsequent `OpenSession(Claude)` fires after WorktreeReady,
            // `selected_key()` returns OUR session (not the one we inherited
            // repo context from).
            let items = crate::nav::nav_items_from_state(state);
            if let Some(idx) = items
                .iter()
                .position(|i| matches!(i, crate::nav::NavItem::Session(k) if k == &key))
            {
                state.selected = idx;
            }

            // Create the worktree off base; when ready, land directly in
            // Claude Code inside it — same flow as clicking `c` on a PR.
            cmds.push(Command::CheckoutWorktree {
                owner,
                repo,
                branch: branch_name,
                base: Some(base_branch),
                session_key: key.clone().into(),
                then: Some(Box::new(Action::OpenSession(crate::action::ShellKind::Claude))),
            });
        }

        // ── Quick reply overlay ──
        Action::QuickReply => {
            if let Some(key) = selected_key(state) {
                let cursor = state.detail_cursor;
                state.quick_reply_input = Some((key, String::new(), cursor));
                state.input_mode = InputMode::TextInput(TextInputKind::QuickReply);
                state.status =
                    "Quick reply — type message, Enter to post, Esc to cancel".into();
            }
        }
        Action::QuickReplyCancel => {
            state.quick_reply_input = None;
            if matches!(state.input_mode, InputMode::TextInput(TextInputKind::QuickReply)) {
                state.input_mode = InputMode::Normal;
            }
            state.status = String::new();
        }
        Action::QuickReplyConfirm { body } => {
            if matches!(state.input_mode, InputMode::TextInput(TextInputKind::QuickReply)) {
                state.input_mode = InputMode::Normal;
            }
            if let Some((session_key, _, comment_idx)) = state.quick_reply_input.take()
                && let Some(session) = state.sessions.get(&session_key) {
                    let repo = session.primary_task.repo.clone().unwrap_or_default();
                    let pr_number = session
                        .primary_task
                        .id
                        .key
                        .rsplit_once('#')
                        .map(|(_, n)| n.to_string())
                        .unwrap_or_default();
                    let reply_to = session
                        .activity
                        .get(comment_idx)
                        .and_then(|a| a.node_id.clone());
                    if !repo.is_empty() && !pr_number.is_empty() && !body.trim().is_empty() {
                        state.status = "Posting reply…".into();
                        cmds.push(Command::RunGhComment {
                            repo,
                            pr_number,
                            body,
                            reply_to_node_id: reply_to,
                        });
                    }
                }
        }

        // ── Picker (reviewer/assignee) ──
        Action::EditReviewers | Action::EditAssignees => {
            let kind = if matches!(action, Action::EditReviewers) {
                PickerKind::Reviewer
            } else {
                PickerKind::Assignee
            };
            let Some(key) = selected_key(state) else { return cmds };
            let Some(session) = state.sessions.get(&key) else { return cmds };
            let task = &session.primary_task;
            let repo = task.repo.as_deref().unwrap_or("").to_string();
            let pr_number = task
                .id
                .key
                .rsplit_once('#')
                .map(|(_, n)| n.to_string())
                .unwrap_or_default();
            if repo.is_empty() || pr_number.is_empty() {
                state.status = "No PR info available".into();
                return cmds;
            }
            let current: Vec<String> = match kind {
                PickerKind::Reviewer => task.reviewers.clone(),
                PickerKind::Assignee => task.assignees.clone(),
            };
            if let Some(collabs) = state.collaborators_cache.get(&repo) {
                let items = build_picker_items(collabs, &current);
                state.picker = Some(PickerState {
                    kind,
                    items,
                    cursor: 0,
                    filter: String::new(),
                    session_key: key,
                    repo,
                    pr_number,
                });
                state.input_mode = InputMode::Picker;
            } else {
                state.status = format!("Loading collaborators for {repo}…");
                cmds.push(Command::FetchCollaborators {
                    repo,
                    kind,
                    session_key: key.into(),
                    pr_number,
                });
            }
        }
        Action::CollaboratorsLoaded(payload) => {
            let crate::action::CollaboratorsLoaded {
                repo,
                kind,
                session_key,
                pr_number,
                collaborators,
                ..
            } = *payload;
            state.collaborators_cache.insert(repo.clone(), collaborators.clone());
            // Determine current selections from the session.
            let current: Vec<String> = state
                .sessions
                .get(session_key.as_str())
                .map(|s| match kind {
                    PickerKind::Reviewer => s.primary_task.reviewers.clone(),
                    PickerKind::Assignee => s.primary_task.assignees.clone(),
                })
                .unwrap_or_default();
            let items = build_picker_items(&collaborators, &current);
            state.picker = Some(PickerState {
                kind,
                items,
                cursor: 0,
                filter: String::new(),
                session_key: session_key.to_string(),
                repo,
                pr_number,
            });
            state.input_mode = InputMode::Picker;
            state.status = String::new();
        }
        Action::PickerCancel => {
            state.picker = None;
            if matches!(state.input_mode, InputMode::Picker) {
                state.input_mode = InputMode::Normal;
            }
        }
        Action::PickerConfirm => {
            if matches!(state.input_mode, InputMode::Picker) {
                state.input_mode = InputMode::Normal;
            }
            if let Some(picker) = state.picker.take() {
                if !state.sessions.contains_key(&picker.session_key) {
                    state.status = "Picker: session no longer available".into();
                    return cmds;
                }
                let added: Vec<String> = picker
                    .items
                    .iter()
                    .filter(|i| i.selected && !i.was_selected)
                    .map(|i| i.login.clone())
                    .collect();
                let removed: Vec<String> = picker
                    .items
                    .iter()
                    .filter(|i| !i.selected && i.was_selected)
                    .map(|i| i.login.clone())
                    .collect();
                if added.is_empty() && removed.is_empty() {
                    return cmds;
                }
                let label = match picker.kind {
                    PickerKind::Reviewer => "reviewer",
                    PickerKind::Assignee => "assignee",
                };
                state.status = format!("Updating {label}s…");
                // Optimistic update in-place.
                if let Some(session) = state.sessions.get_mut(&picker.session_key) {
                    let people = match picker.kind {
                        PickerKind::Reviewer => &mut session.primary_task.reviewers,
                        PickerKind::Assignee => &mut session.primary_task.assignees,
                    };
                    people.retain(|p| !removed.contains(p));
                    for u in &added {
                        if !people.contains(u) {
                            people.push(u.clone());
                        }
                    }
                }
                cmds.push(Command::RunGhEditCollaborators {
                    repo: picker.repo,
                    pr_number: picker.pr_number,
                    kind: picker.kind,
                    added,
                    removed,
                });
            }
        }

        // ── Monitor state machine ──
        //
        // The state transitions here mirror the old `monitor::handle_monitor_tick`
        // but stay pure: the CI-fix spawn becomes a `WriteMonitorContext` +
        // related commands the shell executes.
        Action::MonitorTick { session_key } => {
            let Some(session) = state.sessions.get(&session_key) else {
                return cmds;
            };
            let Some(monitor) = session.monitor.clone() else {
                return cmds;
            };
            let ci = session.primary_task.ci;
            let display_name = session.display_name.clone();

            match monitor {
                pilot_core::MonitorState::Idle => {
                    if ci == pilot_core::CiStatus::Failure {
                        if let Some(s) = state.sessions.get_mut(&session_key) {
                            s.monitor = Some(pilot_core::MonitorState::CiFixing { attempt: 1 });
                        }
                        state.status = format!("Monitor: fixing CI for {display_name}");
                        cmds.extend(queue_monitor_claude_fix(state, &session_key, clock));
                    }
                }
                pilot_core::MonitorState::WaitingCi { after_attempt } => match ci {
                    pilot_core::CiStatus::Success => {
                        if let Some(s) = state.sessions.get_mut(&session_key) {
                            s.monitor = Some(pilot_core::MonitorState::Idle);
                        }
                        state.status = format!("Monitor: CI passed for {display_name}");
                    }
                    pilot_core::CiStatus::Failure => {
                        if after_attempt >= MONITOR_MAX_CI_RETRIES {
                            if let Some(s) = state.sessions.get_mut(&session_key) {
                                s.monitor = Some(pilot_core::MonitorState::Failed {
                                    reason: format!(
                                        "CI still failing after {after_attempt} attempts"
                                    ),
                                });
                            }
                            state.status = format!(
                                "Monitor: gave up on {display_name} after {after_attempt} attempts"
                            );
                        } else {
                            if let Some(s) = state.sessions.get_mut(&session_key) {
                                s.monitor = Some(pilot_core::MonitorState::CiFixing {
                                    attempt: after_attempt + 1,
                                });
                            }
                            state.status = format!(
                                "Monitor: retry #{} for {display_name}",
                                after_attempt + 1
                            );
                            cmds.extend(queue_monitor_claude_fix(state, &session_key, clock));
                        }
                    }
                    _ => {}
                },
                pilot_core::MonitorState::CiFixing { attempt } => {
                    if ci == pilot_core::CiStatus::Success {
                        if let Some(s) = state.sessions.get_mut(&session_key) {
                            s.monitor = Some(pilot_core::MonitorState::Idle);
                        }
                        state.status = format!("Monitor: CI passed for {display_name}");
                    } else if let Some(s) = state.sessions.get_mut(&session_key) {
                        s.monitor =
                            Some(pilot_core::MonitorState::WaitingCi { after_attempt: attempt });
                        state.status = format!("Monitor: waiting for CI on {display_name}");
                    }
                }
                pilot_core::MonitorState::Rebasing => {
                    if let Some(s) = state.sessions.get_mut(&session_key) {
                        s.monitor = Some(pilot_core::MonitorState::WaitingCi { after_attempt: 0 });
                    }
                    state.status = format!("Monitor: rebased, waiting for CI on {display_name}");
                }
                pilot_core::MonitorState::Failed { .. } => {}
            }
        }

        Action::TmuxSessionsRefreshed { sessions } => {
            state.live_tmux_sessions = sessions;
            // Auto-attach: any persisted pilot session whose tmux process
            // is alive but which doesn't have a pilot terminal gets its
            // terminal re-spawned (`tmux -A` reattaches rather than
            // creating new). Covers the "quit + restart" case, sessions
            // learned after startup, and tabs the user closed but wants
            // back when they come into view.
            let shell_kind = crate::action::ShellKind::Claude;
            let candidates: Vec<(String, PathBuf)> = state
                .sessions
                .iter()
                .filter(|(_, s)| {
                    matches!(
                        s.primary_task.state,
                        pilot_core::TaskState::Open
                            | pilot_core::TaskState::Draft
                            | pilot_core::TaskState::InReview
                            | pilot_core::TaskState::InProgress
                    )
                })
                .filter_map(|(key, s)| {
                    let tmux_name = key.replace([':', '/'], "_");
                    if !state.live_tmux_sessions.contains(&tmux_name) {
                        return None;
                    }
                    if state.terminal_index.contains_key(key) {
                        return None; // already attached
                    }
                    let cwd = s
                        .worktree_path
                        .clone()
                        .unwrap_or_else(|| clock.default_cwd.clone());
                    Some((key.clone(), cwd))
                })
                .collect();
            for (key, cwd) in candidates {
                cmds.push(Command::SpawnTerminal {
                    session_key: key.into(),
                    cwd,
                    kind: shell_kind,
                    focus: false, // auto-attach — don't steal focus
                });
            }
        }

        Action::NeedsRebaseResult {
            session_key,
            needs_rebase,
            wt_path,
            default_branch,
        } => {
            if needs_rebase {
                if let Some(s) = state.sessions.get_mut(&session_key) {
                    s.monitor = Some(pilot_core::MonitorState::Rebasing);
                }
                state.status = format!("Monitor: rebasing {session_key}…");
                cmds.push(Command::RunRebase {
                    session_key,
                    wt_path,
                    default_branch,
                });
            }
        }

        // ── Monitor toggle ──
        Action::ToggleMonitor => {
            let Some(key) = selected_key(state) else { return cmds };
            let is_monitored = state.monitored_sessions.contains(&key);
            if is_monitored {
                state.monitored_sessions.remove(&key);
                if let Some(s) = state.sessions.get_mut(&key) {
                    s.monitor = None;
                }
                state.status = "Monitor off".into();
                cmds.push(Command::UpdateMonitoredSet {
                    session_key: key.into(),
                    monitored: false,
                });
            } else {
                state.monitored_sessions.insert(key.clone());
                if let Some(s) = state.sessions.get_mut(&key) {
                    s.monitor = Some(pilot_core::MonitorState::Idle);
                }
                state.status = "Monitor on — pilot will auto-fix CI failures".into();
                cmds.push(Command::UpdateMonitoredSet {
                    session_key: key.clone().into(),
                    monitored: true,
                });
                cmds.push(Command::DispatchAction(Action::MonitorTick { session_key: key.into() }));
            }
        }

        // ── Collapse / Expand by cursor ──
        Action::CollapseSelected => {
            if let Some(item) = crate::nav::selected_nav_item_from_state(state) {
                match item {
                    crate::nav::NavItem::Repo(r) => {
                        state.collapsed_repos.insert(r);
                    }
                    crate::nav::NavItem::Session(k) => {
                        state.collapsed_sessions.insert(k);
                    }
                }
            }
        }
        Action::ExpandSelected => {
            if let Some(item) = crate::nav::selected_nav_item_from_state(state) {
                match item {
                    crate::nav::NavItem::Repo(r) => {
                        state.collapsed_repos.remove(&r);
                    }
                    crate::nav::NavItem::Session(k) => {
                        state.collapsed_sessions.remove(&k);
                    }
                }
            }
        }

        // ── External events from providers ──
        Action::ExternalEvent(event) => {
            // Remember which session the cursor was on so we can restore
            // it after the mutation. Without this, a poll that inserts a
            // new session in an earlier repo group shifts every row below,
            // and the sidebar cursor lands on a different PR.
            let prior_selected_key: Option<String> =
                match crate::nav::selected_nav_item_from_state(state) {
                    Some(crate::nav::NavItem::Session(k)) => Some(k),
                    _ => None,
                };

            let event = *event; // unbox once; event.kind is consumed below
            state.notifications.insert(0, event.summary());
            if state.notifications.len() > 100 {
                state.notifications.truncate(100);
            }
            match event.kind {
                EventKind::TaskUpdated(task) => {
                    if !state.loaded {
                        state.loaded = true;
                    }
                    if !state.purged_stale {
                        state.first_poll_keys.insert(task.id.to_string());
                    }
                    let key = task.id.to_string();

                    // Merged/closed → forget everywhere.
                    if matches!(
                        task.state,
                        pilot_core::TaskState::Merged | pilot_core::TaskState::Closed
                    ) {
                        let was_viewing =
                            selected_key(state).as_deref() == Some(key.as_str());
                        cmds.extend(forget_session_in_state(state, &key));
                        if was_viewing {
                            reset_detail(state);
                        }
                        cmds.push(Command::CloseTerminal {
                            session_key: key.clone().into(),
                        });
                        cmds.push(Command::StoreDeleteSession {
                            task_id: task.id.clone(),
                        });
                        return cmds;
                    }

                    let mut task = task;
                    if let Some(session) = state.sessions.get_mut(&key) {
                        let existing_count = session.activity.len();
                        let fresh_count = task.recent_activity.len();
                        let shift = fresh_count.saturating_sub(existing_count);
                        if shift > 0 {
                            // Drain the new activities instead of cloning — we
                            // own `task` so we can move them straight into
                            // the session.
                            for a in task.recent_activity.drain(..shift) {
                                session.push_activity(a);
                            }
                        }
                        session.primary_task = task;
                        if shift > 0 {
                            shift_detail_indices_if_viewing(state, &key, shift);
                        }
                    } else {
                        // New session — shell will load any persisted seen_count
                        // after save via its own logic. For now, just seed with
                        // fresh data; a subsequent StoreSaveSession writes back.
                        let activities = std::mem::take(&mut task.recent_activity);
                        let mut session = pilot_core::Session::new_at(task, clock.chrono);
                        for activity in activities {
                            session.push_activity(activity);
                        }
                        state.sessions.insert(key.clone(), session);
                    }
                    cmds.push(Command::StoreSaveSession {
                        session_key: key.clone().into(),
                    });
                }
                EventKind::NewActivity {
                    task_id,
                    activity,
                } => {
                    let key = task_id.to_string();
                    let mut pushed = false;
                    if let Some(session) = state.sessions.get_mut(&key) {
                        session.push_activity(activity.clone());
                        pushed = true;
                    }
                    if pushed {
                        shift_detail_indices_if_viewing(state, &key, 1);
                    }
                    // Notify if author needs to reply.
                    if state.loaded
                        && let Some(session) = state.sessions.get(&key)
                            && session.primary_task.needs_reply
                                && session.primary_task.role == pilot_core::TaskRole::Author
                            {
                                cmds.push(Command::Notify {
                                    title: format!(
                                        "{} commented on {}",
                                        activity.author, session.display_name
                                    ),
                                    body: "You may need to reply".into(),
                                });
                            }
                }
                EventKind::TaskStateChanged { task_id, new, .. } => {
                    let key = task_id.to_string();
                    if let Some(session) = state.sessions.get_mut(&key) {
                        session.primary_task.state = new;
                    }
                }
                EventKind::CiStatusChanged { task_id, new, .. } => {
                    let key = task_id.to_string();
                    let mut notify_title = None;
                    if let Some(session) = state.sessions.get_mut(&key) {
                        session.primary_task.ci = new;
                        if session.monitor.is_some() {
                            cmds.push(Command::DispatchAction(Action::MonitorTick {
                                session_key: key.clone().into(),
                            }));
                        }
                        if state.loaded
                            && new == pilot_core::CiStatus::Failure
                            && session.primary_task.role == pilot_core::TaskRole::Author
                        {
                            notify_title = Some(session.display_name.clone());
                        }
                    }
                    if let Some(title) = notify_title {
                        cmds.push(Command::Notify {
                            title: format!("CI failed: {title}"),
                            body: "A CI check failed on your PR".into(),
                        });
                    }
                }
                EventKind::ReviewStatusChanged { task_id, new, .. } => {
                    let key = task_id.to_string();
                    let mut notify_title = None;
                    if let Some(session) = state.sessions.get_mut(&key) {
                        session.primary_task.review = new;
                        if state.loaded
                            && new == pilot_core::ReviewStatus::Approved
                            && session.primary_task.role == pilot_core::TaskRole::Author
                        {
                            notify_title = Some(session.display_name.clone());
                        }
                    }
                    if let Some(title) = notify_title {
                        cmds.push(Command::Notify {
                            title: format!("Approved: {title}"),
                            body: "Your PR was approved! Ready to merge.".into(),
                        });
                    }
                }
                EventKind::TaskRemoved(id) => {
                    let key = id.to_string();
                    let was_viewing = selected_key(state).as_deref() == Some(key.as_str());
                    cmds.extend(forget_session_in_state(state, &key));
                    cmds.push(Command::CloseTerminal {
                        session_key: key.clone().into(),
                    });
                    cmds.push(Command::StoreDeleteSession { task_id: id });
                    if was_viewing {
                        reset_detail(state);
                    }
                }
                EventKind::ProviderError { message } => {
                    tracing::warn!("Provider error: {message}");
                    // Surface to the user — truncate to keep the status bar
                    // readable, but include enough of the message to debug
                    // (GraphQL errors are often >100 chars).
                    let shown: String = message.chars().take(160).collect();
                    let suffix = if message.chars().count() > 160 { "…" } else { "" };
                    state.status = format!("Provider error: {shown}{suffix}");
                    // Poison the purge: an error means the fresh result set
                    // is incomplete, so we can't trust "not in first_poll_keys"
                    // as a signal that a stored session is gone.
                    if !state.purged_stale {
                        state.first_poll_had_errors = true;
                    }
                }
            }

            // Restore the sidebar cursor onto the same session it was on
            // before the event. `state.selected` is an INDEX: after a new
            // session insert shifts rows (new repo group sorts earlier
            // alphabetically, or an is_session_visible flip hides/shows
            // rows), the index points at a different session. Re-resolve
            // by key so the user doesn't see the cursor teleport on every
            // poll.
            if let Some(prior_key) = prior_selected_key {
                let items = crate::nav::nav_items_from_state(state);
                if let Some(idx) = items
                    .iter()
                    .position(|i| matches!(i, crate::nav::NavItem::Session(k) if k == &prior_key))
                {
                    state.selected = idx;
                } else {
                    // Prior session vanished (merged, closed, filtered out).
                    // Clamp to a valid index — don't jump to zero.
                    let n = items.len();
                    if n > 0 && state.selected >= n {
                        state.selected = n - 1;
                    }
                }
            }
        }

        // ── Time-range filter cycle (1d → 3d → 7d → 30d → all → 1d) ──
        Action::CycleTimeFilter => {
            state.activity_days_filter = match state.activity_days_filter {
                1 => 3,
                3 => 7,
                7 => 30,
                30 => 0,
                _ => 1,
            };
            let label = match state.activity_days_filter {
                0 => "all time".to_string(),
                d => format!("last {d}d"),
            };
            state.status = format!("Filter: {label}");
            state.selected = 0;
        }

        // Anything not yet migrated is handled by the old handle_action branch.
        _ => return Vec::new(),
    }
    cmds
}

fn reset_detail(state: &mut State) {
    state.detail_scroll = 0;
    state.detail_cursor = 0;
    state.selected_comments.clear();
}

fn recompute_filter(state: &mut State) {
    // Recompute `filtered_keys` from the current search query.
    let query = state.search_query.trim().to_lowercase();
    if query.is_empty() {
        state.filtered_keys = None;
        return;
    }
    let filtered: Vec<String> = state
        .sessions
        .order()
        .iter()
        .filter(|key| {
            state
                .sessions
                .get(key)
                .map(|s| crate::nav::session_matches_query(s, &query))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    state.filtered_keys = Some(filtered);
    state.selected = 0;
}

fn mark_cursor_comment_read(state: &mut State) {
    if let Some(key) = selected_key(state) {
        let idx = state.detail_cursor;
        if let Some(session) = state.sessions.get_mut(&key)
            && session.is_activity_unread(idx) {
                session.mark_activity_read(idx);
            }
    }
}

/// Forget a session at the State level (memory, monitored set, viewing_since).
/// Returns commands the caller MUST emit to keep shell-owned state in sync —
/// particularly the shared `Arc<Mutex<HashSet>>` monitored set. Store +
/// terminal cleanup are emitted explicitly at each call site.
#[must_use]
fn forget_session_in_state(state: &mut State, key: &str) -> Vec<Command> {
    let was_monitored = state.monitored_sessions.remove(key);
    state.sessions.remove(key);
    if state
        .viewing_since
        .as_ref()
        .map(|(k, _)| k.as_str())
        == Some(key)
    {
        state.viewing_since = None;
    }
    if was_monitored {
        vec![Command::UpdateMonitoredSet {
            session_key: key.into(),
            monitored: false,
        }]
    } else {
        Vec::new()
    }
}

/// Shift detail_cursor / selected_comments forward by n when new activities
/// prepend to the currently-viewed session's activity list.
fn shift_detail_indices_if_viewing(state: &mut State, session_key: &str, n: usize) {
    if n == 0 { return; }
    let viewing = selected_key(state).as_deref() == Some(session_key);
    if !viewing { return; }
    let new_len = state
        .sessions
        .get(session_key)
        .map(|s| s.activity.len())
        .unwrap_or(0);
    if new_len == 0 {
        state.detail_cursor = 0;
        state.selected_comments.clear();
        return;
    }
    state.detail_cursor = (state.detail_cursor + n).min(new_len - 1);
    state.selected_comments = state
        .selected_comments
        .iter()
        .filter_map(|i| {
            let shifted = i + n;
            if shifted < new_len { Some(shifted) } else { None }
        })
        .collect();
}

/// Build the CI-fix prompt, queue it for injection, and emit the IO
/// commands the shell needs to run (write context file, ensure terminal).
///
/// Returns the commands the caller should append to its own `cmds` vec.
fn queue_monitor_claude_fix(state: &mut State, session_key: &str, clock: &Clock) -> Vec<Command> {
    let Some(session) = state.sessions.get(session_key) else {
        return Vec::new();
    };
    let task = &session.primary_task;
    let mut prompt = String::new();
    prompt.push_str("# Task: Fix CI failures\n\n");
    prompt.push_str("## PR\n\n");
    prompt.push_str(&format!("- **Title:** {}\n", task.title));
    prompt.push_str(&format!("- **URL:** {}\n", task.url));
    if let Some(ref branch) = task.branch {
        prompt.push_str(&format!("- **Branch:** `{branch}`\n"));
    }
    prompt.push_str(&format!("- **CI:** {:?}\n", task.ci));

    prompt.push_str("\n## Failed CI Checks\n\n");
    let mut has_failures = false;
    for check in &task.checks {
        if check.status == pilot_core::CiStatus::Failure {
            has_failures = true;
            prompt.push_str(&format!("- **{}**", check.name));
            if let Some(ref url) = check.url {
                prompt.push_str(&format!(" — [logs]({url})"));
            }
            prompt.push('\n');
        }
    }
    if !has_failures {
        prompt.push_str(
            "- (no individual check details available — investigate via `gh pr checks`)\n",
        );
    }
    prompt.push_str("\n## Instructions\n\n");
    prompt.push_str("CI is failing on this PR. Please:\n\n");
    prompt.push_str("1. Run `gh pr checks <num>` to see failing checks\n");
    prompt.push_str("2. Investigate the failing checks — read logs, reproduce locally\n");
    prompt.push_str("3. Make the necessary code changes to fix them\n");
    prompt.push_str("4. Run `git push` to push your fix\n\n");

    let mut cmds = vec![Command::WriteMonitorContext {
        session_key: session_key.into(),
        content: prompt.clone(),
    }];

    // If no terminal, spawn one.
    if !state.terminal_index.contains_key(session_key) {
        if let Some(wt) = session.worktree_path.clone() {
            cmds.push(Command::SpawnTerminal {
                session_key: session_key.into(),
                cwd: wt,
                kind: crate::action::ShellKind::Claude,
                focus: false, // monitor auto-spawn, don't grab focus
            });
        } else if let Some(s) = state.sessions.get_mut(session_key) {
            s.monitor = Some(pilot_core::MonitorState::Failed {
                reason: "No worktree available".into(),
            });
            return cmds;
        }
    }

    // Queue the prompt; shell injects it on the next idle detection.
    state.pending_prompts.insert(session_key.to_string(), prompt);
    state.last_claude_send = Some(clock.instant);
    cmds
}

/// After changing the active terminal tab, move the sidebar cursor onto the
/// session that tab represents.
fn sync_selected_to_active_tab(state: &mut State) {
    let Some(tab_key) = state.terminal_index.active_key().cloned() else { return };
    let items = crate::nav::nav_items_from_state(state);
    if let Some(idx) = items
        .iter()
        .position(|i| matches!(i, crate::nav::NavItem::Session(k) if k == &tab_key))
    {
        state.selected = idx;
    }
}

fn selected_key(state: &State) -> Option<String> {
    let items = crate::nav::nav_items_from_state(state);
    match items.get(state.selected) {
        Some(crate::nav::NavItem::Session(k)) => Some(k.clone()),
        _ => None,
    }
}

/// Repo the sidebar cursor is currently "in" — works whether the user is
/// on a session row or a repo header. Returns "owner/repo".
pub(crate) fn infer_repo_context(state: &State) -> Option<String> {
    let items = crate::nav::nav_items_from_state(state);
    match items.get(state.selected)? {
        crate::nav::NavItem::Session(k) => {
            state.sessions.get(k)?.primary_task.repo.clone()
        }
        crate::nav::NavItem::Repo(r) => Some(r.clone()),
    }
}

/// After a focus move, step past panes that aren't meaningful for the
/// CURRENT selection:
///   - Landed on Detail but the selected session has a live terminal →
///     skip to Terminal (the typing surface).
///   - Landed on Terminal but the selected session has NO live terminal
///     → the pane tree's Terminal leaf belongs to some other session
///     and isn't rendered. Tab must cycle past it to avoid "TERM mode
///     with nothing visible".
///   - Landed on Detail and no terminal exists → stay; user needs Detail
///     to select comments.
fn skip_empty_detail(state: &mut State) {
    let selected_has_terminal = match selected_key(state) {
        Some(k) => state.terminal_index.keys.contains(&k),
        None => false,
    };
    match state.panes.focused_content() {
        Some(PaneContent::Detail(_)) if selected_has_terminal => {
            state.panes.focus_next();
        }
        Some(PaneContent::Terminal(_)) if !selected_has_terminal => {
            state.panes.focus_next();
        }
        _ => {}
    }
}

/// Return `true` if `reduce` fully handled the action. This is a compile-time
/// enumeration so the shell knows which actions to route through reduce vs
/// the legacy path during the migration.
pub fn handled_by_reduce(action: &Action) -> bool {
    matches!(
        action,
        Action::SelectNext
            | Action::SelectPrev
            | Action::StatusMessage(_)
            | Action::ToggleHelp
            | Action::SearchActivate
            | Action::SearchInput(_)
            | Action::SearchBackspace
            | Action::SearchClear
            | Action::Snooze
            | Action::FocusPaneNext
            | Action::FocusPanePrev
            | Action::FocusPaneUp
            | Action::FocusPaneDown
            | Action::FocusPaneLeft
            | Action::FocusPaneRight
            | Action::SplitVertical
            | Action::SplitHorizontal
            | Action::ClosePane
            | Action::ResizePane(_)
            | Action::FullscreenToggle
            | Action::ToggleRepo(_)
            | Action::ToggleSession(_)
            | Action::CollapseSelected
            | Action::ExpandSelected
            | Action::DetailCursorUp
            | Action::DetailCursorDown
            | Action::ToggleCommentSelect
            | Action::SelectAllComments
            | Action::CycleTimeFilter
            | Action::Quit
            | Action::Refresh
            | Action::MarkRead
            | Action::OpenInBrowser
            | Action::OpenCiChecks
            | Action::JumpToNextAsking
            | Action::FocusDetail
            | Action::MergePr
            | Action::MergeCompleted { .. }
            | Action::ApprovePr
            | Action::UpdateBranch
            | Action::SlackNudge
            | Action::WorktreeReady { .. }
            | Action::WorktreeFailed { .. }
            | Action::ResetLayout
            | Action::OpenSession(_)
            | Action::NextTab
            | Action::PrevTab
            | Action::GoToTab(_)
            | Action::CloseTab
            | Action::KillSession
            | Action::Resize { .. }
            | Action::CacheDefaultBranch { .. }
            | Action::WaitingPrefix
            | Action::None
            | Action::NewSession
            | Action::NewSessionCancel
            | Action::NewSessionConfirm { .. }
            | Action::QuickReply
            | Action::QuickReplyCancel
            | Action::QuickReplyConfirm { .. }
            | Action::EditReviewers
            | Action::EditAssignees
            | Action::PickerCancel
            | Action::PickerConfirm
            | Action::CollaboratorsLoaded(_)
            | Action::ToggleMonitor
            | Action::MonitorTick { .. }
            | Action::NeedsRebaseResult { .. }
            | Action::TmuxSessionsRefreshed { .. }
            | Action::ExternalEvent(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;

    fn now() -> Clock { Clock::for_test() }

    #[test]
    fn status_message_updates_status() {
        let mut s = State::new_for_test();
        let cmds = reduce(&mut s, Action::StatusMessage("hi".into()), &now());
        assert!(cmds.is_empty());
        assert_eq!(s.status, "hi");
    }

    #[test]
    fn toggle_help_toggles() {
        let mut s = State::new_for_test();
        assert!(!s.show_help);
        reduce(&mut s, Action::ToggleHelp, &now());
        assert!(s.show_help);
        assert_eq!(s.input_mode, InputMode::Help);
        reduce(&mut s, Action::ToggleHelp, &now());
        assert!(!s.show_help);
        assert_eq!(s.input_mode, InputMode::Normal);
    }

    #[test]
    fn search_backspace_pops_char() {
        let mut s = State::new_for_test();
        s.search_query = "hello".into();
        reduce(&mut s, Action::SearchBackspace, &now());
        assert_eq!(s.search_query, "hell");
    }

    #[test]
    fn search_clear_resets_everything() {
        let mut s = State::new_for_test();
        s.search_query = "anything".into();
        s.filtered_keys = Some(vec!["k1".into()]);
        s.selected = 5;
        reduce(&mut s, Action::SearchClear, &now());
        assert_eq!(s.search_query, "");
        assert!(s.filtered_keys.is_none());
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn select_next_clamps_when_no_nav_items() {
        let mut s = State::new_for_test();
        // No sessions, so nav_items is empty; selected stays at 0.
        reduce(&mut s, Action::SelectNext, &now());
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn select_prev_saturates_at_zero() {
        let mut s = State::new_for_test();
        s.selected = 0;
        reduce(&mut s, Action::SelectPrev, &now());
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn select_prev_clears_detail_state() {
        let mut s = State::new_for_test();
        s.selected = 3;
        s.detail_cursor = 5;
        s.detail_scroll = 12;
        s.selected_comments.insert(1);
        reduce(&mut s, Action::SelectPrev, &now());
        assert_eq!(s.selected, 2);
        assert_eq!(s.detail_cursor, 0);
        assert_eq!(s.detail_scroll, 0);
        assert!(s.selected_comments.is_empty());
    }

    #[test]
    fn handled_by_reduce_whitelist() {
        assert!(handled_by_reduce(&Action::SelectNext));
        assert!(handled_by_reduce(&Action::SelectPrev));
        assert!(handled_by_reduce(&Action::ToggleHelp));
        assert!(handled_by_reduce(&Action::Snooze));
        assert!(handled_by_reduce(&Action::FocusPaneNext));
        assert!(handled_by_reduce(&Action::SplitVertical));
        assert!(handled_by_reduce(&Action::DetailCursorDown));
        assert!(handled_by_reduce(&Action::CycleTimeFilter));
        assert!(handled_by_reduce(&Action::Quit));
        assert!(handled_by_reduce(&Action::Refresh));
        assert!(handled_by_reduce(&Action::MergePr));
        assert!(handled_by_reduce(&Action::OpenInBrowser));
        assert!(handled_by_reduce(&Action::ApprovePr));
        assert!(handled_by_reduce(&Action::ToggleMonitor));
        assert!(handled_by_reduce(&Action::PickerConfirm));
        // Only truly-unmigrated actions: Key/Mouse/Paste/Tick/ExternalEvent/
        // MonitorTick/FixWithClaude/ReplyWithClaude.
        assert!(!handled_by_reduce(&Action::Tick));
    }

    #[test]
    fn quit_sets_flag_when_no_terminals() {
        let mut s = State::new_for_test();
        let cmds = reduce(&mut s, Action::Quit, &now());
        assert!(s.should_quit);
        // Saves all sessions (empty, so 0 cmds of that kind) - still ok.
        assert!(cmds.iter().all(|c| matches!(c, Command::StoreSaveSession { .. })));
    }

    #[test]
    fn quit_asks_confirm_when_terminals_alive() {
        let mut s = State::new_for_test();
        s.terminal_index.keys.insert("tab-1".into());
        let _ = reduce(&mut s, Action::Quit, &now());
        assert!(!s.should_quit);
        assert!(s.quit_pending);
        assert!(s.status.contains("Quit?"));
    }

    #[test]
    fn refresh_emits_wake_poller() {
        let mut s = State::new_for_test();
        let cmds = reduce(&mut s, Action::Refresh, &now());
        assert!(matches!(cmds.as_slice(), [Command::WakePoller]));
        assert_eq!(s.status, "Refreshing…");
    }

    #[test]
    fn open_in_browser_emits_open_url() {
        use pilot_core::*;
        let mut s = State::new_for_test();
        let mut task = Task {
            id: TaskId { source: "github".into(), key: "o/r#1".into() },
            title: "t".into(), body: None, state: TaskState::Open,
            role: TaskRole::Author, ci: CiStatus::None, review: ReviewStatus::None,
            checks: vec![], unread_count: 0,
            url: "https://github.com/o/r/pull/1".into(),
            repo: Some("o/r".into()), branch: Some("b".into()),
            base_branch: None,
            updated_at: chrono::Utc::now(), labels: vec![], reviewers: vec![],
            assignees: vec![], auto_merge_enabled: false, is_in_merge_queue: false,
            has_conflicts: false, is_behind_base: false, node_id: None,
            needs_reply: false, last_commenter: None,
            recent_activity: vec![], additions: 0, deletions: 0,
        };
        task.url = "https://github.com/o/r/pull/1".into();
        let session = pilot_core::Session::new_at(task, chrono::Utc::now());
        s.sessions.insert("github:o/r#1".into(), session);
        let items = crate::nav::nav_items_from_state(&s);
        s.selected = items.iter().position(|i| matches!(i, crate::nav::NavItem::Session(_))).unwrap();

        let cmds = reduce(&mut s, Action::OpenInBrowser, &now());
        assert!(matches!(
            cmds.as_slice(),
            [Command::OpenUrl { url }] if url == "https://github.com/o/r/pull/1"
        ));
    }

    #[test]
    fn approve_blocked_for_author() {
        use pilot_core::*;
        let mut s = State::new_for_test();
        let task = Task {
            id: TaskId { source: "github".into(), key: "o/r#1".into() },
            title: "t".into(), body: None, state: TaskState::Open,
            role: TaskRole::Author,  // Author can't self-approve.
            ci: CiStatus::None, review: ReviewStatus::None,
            checks: vec![], unread_count: 0,
            url: "u".into(), repo: Some("o/r".into()), branch: Some("b".into()),
            base_branch: None,
            updated_at: chrono::Utc::now(), labels: vec![], reviewers: vec![],
            assignees: vec![], auto_merge_enabled: false, is_in_merge_queue: false,
            has_conflicts: false, is_behind_base: false, node_id: None,
            needs_reply: false, last_commenter: None,
            recent_activity: vec![], additions: 0, deletions: 0,
        };
        s.sessions.insert("github:o/r#1".into(), pilot_core::Session::new_at(task, chrono::Utc::now()));
        let items = crate::nav::nav_items_from_state(&s);
        s.selected = items.iter().position(|i| matches!(i, crate::nav::NavItem::Session(_))).unwrap();

        let cmds = reduce(&mut s, Action::ApprovePr, &now());
        assert!(cmds.is_empty());
        assert!(s.status.contains("need Reviewer or Assignee"));
    }

    #[test]
    fn approve_works_for_reviewer() {
        use pilot_core::*;
        let mut s = State::new_for_test();
        let mut task = Task {
            id: TaskId { source: "github".into(), key: "o/r#1".into() },
            title: "t".into(), body: None, state: TaskState::Open,
            role: TaskRole::Author, ci: CiStatus::None, review: ReviewStatus::None,
            checks: vec![], unread_count: 0,
            url: "u".into(), repo: Some("o/r".into()), branch: Some("b".into()),
            base_branch: None,
            updated_at: chrono::Utc::now(), labels: vec![], reviewers: vec![],
            assignees: vec![], auto_merge_enabled: false, is_in_merge_queue: false,
            has_conflicts: false, is_behind_base: false, node_id: None,
            needs_reply: false, last_commenter: None,
            recent_activity: vec![], additions: 0, deletions: 0,
        };
        task.role = TaskRole::Reviewer;
        s.sessions.insert("github:o/r#1".into(), pilot_core::Session::new_at(task, chrono::Utc::now()));
        let items = crate::nav::nav_items_from_state(&s);
        s.selected = items.iter().position(|i| matches!(i, crate::nav::NavItem::Session(_))).unwrap();

        let cmds = reduce(&mut s, Action::ApprovePr, &now());
        assert!(matches!(cmds.as_slice(), [Command::RunGhApprove { .. }]));
        assert_eq!(
            s.sessions.get("github:o/r#1").unwrap().primary_task.review,
            ReviewStatus::Approved
        );
    }

    #[test]
    fn toggle_monitor_switches_on_and_off() {
        use pilot_core::*;
        let mut s = State::new_for_test();
        let task = Task {
            id: TaskId { source: "github".into(), key: "o/r#1".into() },
            title: "t".into(), body: None, state: TaskState::Open,
            role: TaskRole::Author, ci: CiStatus::None, review: ReviewStatus::None,
            checks: vec![], unread_count: 0, url: "u".into(),
            repo: Some("o/r".into()), branch: Some("b".into()),
            base_branch: None,
            updated_at: chrono::Utc::now(), labels: vec![], reviewers: vec![],
            assignees: vec![], auto_merge_enabled: false, is_in_merge_queue: false,
            has_conflicts: false, is_behind_base: false, node_id: None,
            needs_reply: false, last_commenter: None,
            recent_activity: vec![], additions: 0, deletions: 0,
        };
        s.sessions.insert("github:o/r#1".into(), pilot_core::Session::new_at(task, chrono::Utc::now()));
        let items = crate::nav::nav_items_from_state(&s);
        s.selected = items.iter().position(|i| matches!(i, crate::nav::NavItem::Session(_))).unwrap();

        let cmds = reduce(&mut s, Action::ToggleMonitor, &now());
        assert!(s.monitored_sessions.contains("github:o/r#1"));
        // Emits: UpdateMonitoredSet + DispatchAction(MonitorTick).
        assert_eq!(cmds.len(), 2);

        let cmds = reduce(&mut s, Action::ToggleMonitor, &now());
        assert!(!s.monitored_sessions.contains("github:o/r#1"));
        assert!(matches!(
            cmds.as_slice(),
            [Command::UpdateMonitoredSet { monitored: false, .. }]
        ));
    }

    #[test]
    fn merge_first_press_pending_second_executes() {
        use pilot_core::*;
        let mut s = State::new_for_test();
        let task = Task {
            id: TaskId { source: "github".into(), key: "o/r#1".into() },
            title: "t".into(), body: None, state: TaskState::Open,
            role: TaskRole::Author, ci: CiStatus::Success,
            review: ReviewStatus::Approved,
            checks: vec![], unread_count: 0, url: "u".into(),
            repo: Some("o/r".into()), branch: Some("b".into()),
            base_branch: None,
            updated_at: chrono::Utc::now(), labels: vec![], reviewers: vec![],
            assignees: vec![], auto_merge_enabled: false, is_in_merge_queue: false,
            has_conflicts: false, is_behind_base: false, node_id: None,
            needs_reply: false, last_commenter: None,
            recent_activity: vec![], additions: 0, deletions: 0,
        };
        s.sessions.insert("github:o/r#1".into(), pilot_core::Session::new_at(task, chrono::Utc::now()));
        let items = crate::nav::nav_items_from_state(&s);
        s.selected = items.iter().position(|i| matches!(i, crate::nav::NavItem::Session(_))).unwrap();

        let cmds = reduce(&mut s, Action::MergePr, &now());
        assert!(cmds.is_empty());
        assert_eq!(s.merge_pending.as_deref(), Some("github:o/r#1"));

        let cmds = reduce(&mut s, Action::MergePr, &now());
        assert!(matches!(cmds.as_slice(), [Command::RunGhMerge { .. }]));
        assert!(s.merge_pending.is_none());
        assert_eq!(
            s.sessions.get("github:o/r#1").unwrap().primary_task.state,
            TaskState::Merged
        );
    }

    #[test]
    fn mark_read_emits_store_command() {
        use pilot_core::*;
        let mut s = State::new_for_test();
        let task = Task {
            id: TaskId { source: "github".into(), key: "o/r#1".into() },
            title: "t".into(), body: None, state: TaskState::Open,
            role: TaskRole::Author, ci: CiStatus::None, review: ReviewStatus::None,
            checks: vec![], unread_count: 0, url: "u".into(),
            repo: Some("o/r".into()), branch: Some("b".into()),
            base_branch: None,
            updated_at: chrono::Utc::now(), labels: vec![], reviewers: vec![],
            assignees: vec![], auto_merge_enabled: false, is_in_merge_queue: false,
            has_conflicts: false, is_behind_base: false, node_id: None,
            needs_reply: true, last_commenter: None,
            recent_activity: vec![], additions: 0, deletions: 0,
        };
        s.sessions.insert("github:o/r#1".into(), pilot_core::Session::new_at(task, chrono::Utc::now()));
        let items = crate::nav::nav_items_from_state(&s);
        s.selected = items.iter().position(|i| matches!(i, crate::nav::NavItem::Session(_))).unwrap();

        let cmds = reduce(&mut s, Action::MarkRead, &now());
        assert!(matches!(cmds.as_slice(), [Command::StoreMarkRead { .. }]));
        assert_eq!(s.status, "Marked as read");
        assert!(!s.sessions.get("github:o/r#1").unwrap().primary_task.needs_reply);
    }

    // ── ExternalEvent ──

    fn sample_task(num: u32) -> pilot_core::Task {
        pilot_core::Task {
            id: pilot_core::TaskId {
                source: "github".into(),
                key: format!("o/r#{num}"),
            },
            title: format!("task {num}"),
            body: None,
            state: pilot_core::TaskState::Open,
            role: pilot_core::TaskRole::Author,
            ci: pilot_core::CiStatus::None,
            review: pilot_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "u".into(),
            repo: Some("o/r".into()),
            branch: Some("b".into()),
            base_branch: None,
            updated_at: chrono::Utc::now(),
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            has_conflicts: false,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
        }
    }

    #[test]
    fn external_event_task_updated_creates_session_and_saves() {
        use pilot_events::{Event, EventKind};
        let mut s = State::new_for_test();
        let task = sample_task(1);
        let event = Event::new("github", EventKind::TaskUpdated(task.clone()));

        let cmds = reduce(&mut s, Action::ExternalEvent(Box::new(event)), &now());
        assert!(s.sessions.contains_key("github:o/r#1"));
        assert!(s.loaded);
        assert!(s.first_poll_keys.contains("github:o/r#1"));
        assert!(cmds
            .iter()
            .any(|c| matches!(c, Command::StoreSaveSession { .. })));
    }

    #[test]
    fn external_event_task_updated_merged_removes_session() {
        use pilot_events::{Event, EventKind};
        let mut s = State::new_for_test();
        s.sessions.insert(
            "github:o/r#1".into(),
            pilot_core::Session::new_at(sample_task(1), chrono::Utc::now()),
        );

        let mut task = sample_task(1);
        task.state = pilot_core::TaskState::Merged;
        let event = Event::new("github", EventKind::TaskUpdated(task));

        let cmds = reduce(&mut s, Action::ExternalEvent(Box::new(event)), &now());
        assert!(!s.sessions.contains_key("github:o/r#1"));
        assert!(cmds.iter().any(|c| matches!(c, Command::CloseTerminal { .. })));
        assert!(cmds
            .iter()
            .any(|c| matches!(c, Command::StoreDeleteSession { .. })));
    }

    #[test]
    fn external_event_task_removed_cleans_up() {
        use pilot_events::{Event, EventKind};
        let mut s = State::new_for_test();
        s.sessions.insert(
            "github:o/r#1".into(),
            pilot_core::Session::new_at(sample_task(1), chrono::Utc::now()),
        );
        s.monitored_sessions.insert("github:o/r#1".into());

        let event = Event::new(
            "github",
            EventKind::TaskRemoved(pilot_core::TaskId {
                source: "github".into(),
                key: "o/r#1".into(),
            }),
        );
        let cmds = reduce(&mut s, Action::ExternalEvent(Box::new(event)), &now());

        assert!(!s.sessions.contains_key("github:o/r#1"));
        assert!(!s.monitored_sessions.contains("github:o/r#1"));
        assert!(cmds
            .iter()
            .any(|c| matches!(c, Command::StoreDeleteSession { .. })));
    }

    #[test]
    fn external_event_new_activity_shifts_detail_cursor() {
        use pilot_core::{Activity, ActivityKind};
        use pilot_events::{Event, EventKind};
        let mut s = State::new_for_test();
        let mut session = pilot_core::Session::new_at(sample_task(1), chrono::Utc::now());
        // Pre-existing activity, user is looking at index 0.
        session.push_activity(Activity {
            author: "alice".into(),
            body: "first".into(),
            created_at: chrono::Utc::now(),
            kind: ActivityKind::Comment,
            node_id: None,
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        });
        s.sessions.insert("github:o/r#1".into(), session);
        let items = crate::nav::nav_items_from_state(&s);
        s.selected = items
            .iter()
            .position(|i| matches!(i, crate::nav::NavItem::Session(_)))
            .unwrap();
        s.detail_cursor = 0;

        // New activity arrives — should shift cursor to stay on "first".
        let event = Event::new(
            "github",
            EventKind::NewActivity {
                task_id: pilot_core::TaskId {
                    source: "github".into(),
                    key: "o/r#1".into(),
                },
                activity: Activity {
                    author: "bob".into(),
                    body: "second".into(),
                    created_at: chrono::Utc::now(),
                    kind: ActivityKind::Comment,
                    node_id: None,
                    path: None,
                    line: None,
                    diff_hunk: None,
                    thread_id: None,
                },
            },
        );
        reduce(&mut s, Action::ExternalEvent(Box::new(event)), &now());
        assert_eq!(s.detail_cursor, 1); // shifted from 0 → 1
    }

    #[test]
    fn edit_reviewers_uses_cache_when_available() {
        let mut s = State::new_for_test();
        s.sessions.insert("github:o/r#1".into(), pilot_core::Session::new_at(sample_task(1), chrono::Utc::now()));
        s.collaborators_cache
            .insert("o/r".to_string(), vec!["alice".into(), "bob".into()]);
        let items = crate::nav::nav_items_from_state(&s);
        s.selected = items.iter().position(|i| matches!(i, crate::nav::NavItem::Session(_))).unwrap();

        let cmds = reduce(&mut s, Action::EditReviewers, &now());
        // No fetch — cache hit. Picker is opened.
        assert!(cmds.is_empty());
        assert!(s.picker.is_some());
        assert_eq!(s.input_mode, InputMode::Picker);
    }

    #[test]
    fn edit_reviewers_fetches_when_not_cached() {
        let mut s = State::new_for_test();
        s.sessions.insert("github:o/r#1".into(), pilot_core::Session::new_at(sample_task(1), chrono::Utc::now()));
        let items = crate::nav::nav_items_from_state(&s);
        s.selected = items.iter().position(|i| matches!(i, crate::nav::NavItem::Session(_))).unwrap();

        let cmds = reduce(&mut s, Action::EditReviewers, &now());
        assert!(matches!(
            cmds.as_slice(),
            [Command::FetchCollaborators { .. }]
        ));
        assert!(s.picker.is_none()); // picker opens only after load
    }

    #[test]
    fn picker_confirm_emits_edit_when_changed() {
        let mut s = State::new_for_test();
        s.sessions
            .insert("github:o/r#1".into(), pilot_core::Session::new_at(sample_task(1), chrono::Utc::now()));
        s.picker = Some(PickerState {
            kind: PickerKind::Reviewer,
            items: vec![
                crate::picker::PickerItem {
                    login: "alice".into(),
                    selected: true,   // newly selected
                    was_selected: false,
                },
                crate::picker::PickerItem {
                    login: "bob".into(),
                    selected: false,  // newly removed
                    was_selected: true,
                },
            ],
            cursor: 0,
            filter: String::new(),
            session_key: "github:o/r#1".into(),
            repo: "o/r".into(),
            pr_number: "1".into(),
        });
        s.input_mode = InputMode::Picker;

        let cmds = reduce(&mut s, Action::PickerConfirm, &now());
        assert!(matches!(
            cmds.as_slice(),
            [Command::RunGhEditCollaborators { .. }]
        ));
        assert!(s.picker.is_none());
    }

    #[test]
    fn worktree_ready_unblocks_monitor() {
        use std::path::PathBuf;
        let mut s = State::new_for_test();
        let mut session = pilot_core::Session::new_at(sample_task(1), chrono::Utc::now());
        session.monitor = Some(pilot_core::MonitorState::Idle);
        s.sessions.insert("github:o/r#1".into(), session);

        let cmds = reduce(
            &mut s,
            Action::WorktreeReady {
                session_key: "github:o/r#1".into(),
                path: PathBuf::from("/tmp/worktree"),
            },
            &now(),
        );
        // Since monitor is Some, should emit a MonitorTick dispatch.
        assert!(matches!(
            cmds.as_slice(),
            [Command::DispatchAction(Action::MonitorTick { .. })]
        ));
        assert_eq!(
            s.sessions
                .get("github:o/r#1")
                .unwrap()
                .state,
            pilot_core::SessionState::Active
        );
    }

    #[test]
    fn external_event_ci_failure_notifies_author() {
        use pilot_events::{Event, EventKind};
        let mut s = State::new_for_test();
        s.loaded = true;
        let mut session = pilot_core::Session::new_at(sample_task(1), chrono::Utc::now());
        session.primary_task.role = pilot_core::TaskRole::Author;
        s.sessions.insert("github:o/r#1".into(), session);

        let event = Event::new(
            "github",
            EventKind::CiStatusChanged {
                task_id: pilot_core::TaskId {
                    source: "github".into(),
                    key: "o/r#1".into(),
                },
                old: pilot_core::CiStatus::Running,
                new: pilot_core::CiStatus::Failure,
            },
        );
        let cmds = reduce(&mut s, Action::ExternalEvent(Box::new(event)), &now());
        assert!(cmds.iter().any(|c| matches!(c, Command::Notify { .. })));
        assert_eq!(
            s.sessions.get("github:o/r#1").unwrap().primary_task.ci,
            pilot_core::CiStatus::Failure
        );
    }

    #[test]
    fn new_session_flow() {
        let mut s = State::new_for_test();

        // Seed one PR so there's a repo context for the new session to
        // inherit from. `N` requires context — with an empty inbox it
        // has nothing to branch off.
        let task = pilot_core::Task {
            id: pilot_core::TaskId { source: "github".into(), key: "o/r#1".into() },
            title: "seed".into(), body: None,
            state: pilot_core::TaskState::Open, role: pilot_core::TaskRole::Author,
            ci: pilot_core::CiStatus::None, review: pilot_core::ReviewStatus::None,
            checks: vec![], unread_count: 0,
            url: "https://github.com/o/r/pull/1".into(),
            repo: Some("o/r".into()), branch: Some("topic".into()),
            base_branch: Some("main".into()), updated_at: chrono::Utc::now(),
            labels: vec![], reviewers: vec![], assignees: vec![],
            auto_merge_enabled: false, is_in_merge_queue: false,
            has_conflicts: false, is_behind_base: false, node_id: None,
            needs_reply: false, last_commenter: None,
            recent_activity: vec![], additions: 0, deletions: 0,
        };
        s.sessions.insert(
            "github:o/r#1".into(),
            pilot_core::Session::new_at(task, chrono::Utc::now()),
        );
        // Move cursor onto the seeded session (nav[0] is usually the repo
        // header; nav[1] is the session row).
        let items = crate::nav::nav_items_from_state(&s);
        s.selected = items
            .iter()
            .position(|i| matches!(i, crate::nav::NavItem::Session(k) if k == "github:o/r#1"))
            .expect("seeded session visible in nav");

        reduce(&mut s, Action::NewSession, &now());
        assert!(s.new_session_input.is_some());
        assert_eq!(
            s.input_mode,
            InputMode::TextInput(TextInputKind::NewSession)
        );

        let cmds = reduce(
            &mut s,
            Action::NewSessionConfirm { description: "feat/foo".into() },
            &now(),
        );
        assert!(s.new_session_input.is_none());
        assert_eq!(s.input_mode, InputMode::Normal);
        // Original PR + new local session.
        assert_eq!(s.sessions.len(), 2);
        let new_key = "local:o/r#feat/foo";
        let session = s.sessions.get(new_key).expect("local session created");
        assert_eq!(session.primary_task.repo.as_deref(), Some("o/r"));
        assert_eq!(session.primary_task.branch.as_deref(), Some("feat/foo"));
        assert_eq!(session.primary_task.base_branch.as_deref(), Some("main"));

        // A checkout-with-base command must be queued so the worktree gets
        // created off main, with OpenSession(Claude) chained after.
        let checkout = cmds
            .iter()
            .find(|c| matches!(c, Command::CheckoutWorktree { .. }))
            .expect("CheckoutWorktree queued");
        if let Command::CheckoutWorktree { base, branch, then, .. } = checkout {
            assert_eq!(base.as_deref(), Some("main"));
            assert_eq!(branch, "feat/foo");
            assert!(matches!(then.as_deref(), Some(Action::OpenSession(_))));
        }
    }

    #[test]
    fn new_session_without_context_fails_gracefully() {
        let mut s = State::new_for_test();
        // Empty inbox — no repo to inherit. NewSessionConfirm must NOT
        // create a dead-end session without repo/branch info.
        let cmds = reduce(
            &mut s,
            Action::NewSessionConfirm { description: "feat/foo".into() },
            &now(),
        );
        assert_eq!(s.sessions.len(), 0);
        assert!(!cmds.iter().any(|c| matches!(c, Command::CheckoutWorktree { .. })));
        assert!(s.status.contains("select a PR or a repo header first"));
    }

    // ── Pane actions ──

    #[test]
    fn focus_pane_next_cycles() {
        let mut s = State::new_for_test();
        // Default layout is Inbox + Detail; focus starts on Inbox.
        let before = s.panes.focused;
        reduce(&mut s, Action::FocusPaneNext, &now());
        assert_ne!(s.panes.focused, before);
    }

    #[test]
    fn split_vertical_adds_pane() {
        let mut s = State::new_for_test();
        let before_count = s.panes.resolve(ratatui::prelude::Rect::default()).len();
        reduce(&mut s, Action::SplitVertical, &now());
        let after_count = s.panes.resolve(ratatui::prelude::Rect::default()).len();
        assert_eq!(after_count, before_count + 1);
    }

    #[test]
    fn fullscreen_toggle_sticks() {
        let mut s = State::new_for_test();
        assert!(!s.panes.is_fullscreen());
        reduce(&mut s, Action::FullscreenToggle, &now());
        assert!(s.panes.is_fullscreen());
        reduce(&mut s, Action::FullscreenToggle, &now());
        assert!(!s.panes.is_fullscreen());
    }

    // ── Sidebar tree collapse ──

    #[test]
    fn toggle_repo_flips_collapse_state() {
        let mut s = State::new_for_test();
        reduce(&mut s, Action::ToggleRepo("owner/repo".into()), &now());
        assert!(s.collapsed_repos.contains("owner/repo"));
        reduce(&mut s, Action::ToggleRepo("owner/repo".into()), &now());
        assert!(!s.collapsed_repos.contains("owner/repo"));
    }

    #[test]
    fn toggle_session_flips_collapse_state() {
        let mut s = State::new_for_test();
        reduce(&mut s, Action::ToggleSession("github:o/r#1".into()), &now());
        assert!(s.collapsed_sessions.contains("github:o/r#1"));
        reduce(&mut s, Action::ToggleSession("github:o/r#1".into()), &now());
        assert!(!s.collapsed_sessions.contains("github:o/r#1"));
    }

    // ── Detail cursor ──

    #[test]
    fn detail_cursor_up_saturates() {
        let mut s = State::new_for_test();
        s.detail_cursor = 0;
        reduce(&mut s, Action::DetailCursorUp, &now());
        assert_eq!(s.detail_cursor, 0);
    }

    // ── Time filter cycle ──

    #[test]
    fn cycle_time_filter_cycles() {
        let mut s = State::new_for_test();
        s.activity_days_filter = 1;
        reduce(&mut s, Action::CycleTimeFilter, &now());
        assert_eq!(s.activity_days_filter, 3);
        reduce(&mut s, Action::CycleTimeFilter, &now());
        assert_eq!(s.activity_days_filter, 7);
        reduce(&mut s, Action::CycleTimeFilter, &now());
        assert_eq!(s.activity_days_filter, 30);
        reduce(&mut s, Action::CycleTimeFilter, &now());
        assert_eq!(s.activity_days_filter, 0); // all
        reduce(&mut s, Action::CycleTimeFilter, &now());
        assert_eq!(s.activity_days_filter, 1);
    }

    #[test]
    fn cycle_time_filter_sets_status() {
        let mut s = State::new_for_test();
        s.activity_days_filter = 7;
        reduce(&mut s, Action::CycleTimeFilter, &now());
        assert_eq!(s.activity_days_filter, 30);
        assert!(s.status.contains("30d"));
    }

    // ── Search ──

    #[test]
    fn search_activate_switches_mode() {
        let mut s = State::new_for_test();
        reduce(&mut s, Action::SearchActivate, &now());
        assert!(s.search_active);
        assert_eq!(
            s.input_mode,
            InputMode::TextInput(TextInputKind::Search)
        );
    }

    #[test]
    fn search_input_appends_char() {
        let mut s = State::new_for_test();
        s.search_query = "ab".into();
        reduce(&mut s, Action::SearchInput('c'), &now());
        assert_eq!(s.search_query, "abc");
    }

    #[test]
    fn select_all_comments_toggles() {
        use pilot_core::{Activity, ActivityKind, CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState};
        let mut s = State::new_for_test();
        // Build a session with 3 activities so SelectAll has something to select.
        let task = Task {
            id: TaskId { source: "github".into(), key: "o/r#1".into() },
            title: "t".into(), body: None, state: TaskState::Open,
            role: TaskRole::Author, ci: CiStatus::None, review: ReviewStatus::None,
            checks: vec![], unread_count: 0, url: "u".into(),
            repo: Some("o/r".into()), branch: Some("b".into()),
            base_branch: None,
            updated_at: chrono::Utc::now(), labels: vec![], reviewers: vec![],
            assignees: vec![], auto_merge_enabled: false, is_in_merge_queue: false,
            has_conflicts: false, is_behind_base: false, node_id: None,
            needs_reply: false, last_commenter: None,
            recent_activity: vec![], additions: 0, deletions: 0,
        };
        let mut session = pilot_core::Session::new_at(task, chrono::Utc::now());
        for i in 0..3 {
            session.activity.push(Activity {
                author: format!("u{i}"), body: "x".into(),
                created_at: chrono::Utc::now(),
                kind: ActivityKind::Comment, node_id: None,
                path: None, line: None, diff_hunk: None, thread_id: None,
            });
        }
        s.sessions.insert("github:o/r#1".into(), session);
        // Make it selected in the nav.
        let items = crate::nav::nav_items_from_state(&s);
        s.selected = items.iter().position(|i| matches!(i, crate::nav::NavItem::Session(_))).unwrap();

        assert!(s.selected_comments.is_empty());
        reduce(&mut s, Action::SelectAllComments, &now());
        assert_eq!(s.selected_comments.len(), 3);
        // Calling again when all selected → clears.
        reduce(&mut s, Action::SelectAllComments, &now());
        assert!(s.selected_comments.is_empty());
    }
}
