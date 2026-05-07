//! Sidebar — the left pane. Lists sessions, owns the cursor, handles
//! the core navigation and session-level keybindings.
//!
//! ## Why one component, not three
//!
//! Decomposing into FilterRow + SessionList + SessionRow components
//! is tempting, but the state is tightly coupled (cursor index depends
//! on visible order, which depends on filter/search/mailbox) and
//! splitting it opens a desync surface across multiple owners. Keeping
//! Sidebar as one component with private state is the simpler correct
//! answer. When a specific part gets independently complicated (custom
//! filter UIs per provider, say), splitting it later is localised.
//!
//! ## State the sidebar owns
//!
//! - `workspaces`: the authoritative map of SessionKey → Workspace.
//!   `SessionKey` is the wire-side selection identifier — we use the
//!   workspace's key string as its value. The daemon is the source
//!   of truth; we mirror what it sends via `Event::WorkspaceUpserted`
//!   / `WorkspaceRemoved` / `Snapshot`.
//! - `visible`: derived — `workspaces` filtered by mailbox and
//!   sorted by primary task's `updated_at` descending. Recomputed
//!   on every change so the user never sees a stale order.
//! - `cursor`: index into `visible`. Preserved by key (not index)
//!   across refreshes — the same row stays under the cursor even
//!   when another workspace gets inserted above it.
//! - `mailbox`: which view we're showing (Inbox vs Snoozed).
//! - `kill_pending`: two-press guard for `Shift-X`.

use crate::{PaneId, PaneOutcome};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pilot_core::{SessionId, SessionKey, Workspace};
use pilot_ipc::{Command, Event, TerminalId, TerminalKind};
use ratatui::Frame;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Which logical mailbox the sidebar is currently showing.
///
/// Three mutually-exclusive buckets, cycled via `Shift-S`:
///
/// - **Inbox** — actionable workspaces: not snoozed, primary task
///   is Open / Draft / In-Progress / In-Review. The default.
/// - **Inactive** — historical workspaces: primary task is Merged
///   or Closed. Useful for "where did I work on that PR last
///   week" — the data is already persisted, this just surfaces it.
/// - **Snoozed** — explicitly snoozed (Z / Shift-Z).
///
/// Future expansion: a fourth "All repo activity" view that surfaces
/// PRs the user isn't involved in. That requires a separate GH fetch
/// (today the poller filters by `role.*`) and lives with the
/// org/repo picker work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mailbox {
    #[default]
    Inbox,
    Inactive,
    Snoozed,
}

/// One row in the rendered sidebar list. The visual model is a
/// three-level tree:
///
/// ```text
/// owner/name              <- RepoHeader
///   ▸ Workspace title     <- Workspace (always present)
///       claude            <- Session (only when workspace has 2+)
///       shell             <- Session
///   ▸ Other workspace
/// ```
///
/// **Sessions are only surfaced when the workspace has more than
/// one.** A workspace with zero or one session collapses to its
/// single Workspace row — the sub-list would just be redundant. As
/// soon as a second session appears (`Event::SessionCreated`), the
/// workspace expands to show all of them.
///
/// Headers are render-only — j/k navigation and key dispatch skip
/// them, so the cursor always rests on a Workspace or Session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisibleRow {
    /// Repo group header. The string is the repo display name
    /// (`"owner/name"` for GitHub, the project key for Linear, or
    /// `"(no repo)"` for unattached workspaces).
    RepoHeader(String),
    /// A workspace under whichever repo header most recently appeared.
    Workspace(SessionKey),
    /// A session sub-row (workspace key + session id). Only emitted
    /// when its parent workspace has 2+ sessions; otherwise the
    /// session is implicit in the workspace row.
    Session {
        workspace: SessionKey,
        session_id: SessionId,
    },
}

/// Per-repo summary line shown in the collapsible header.
#[derive(Debug, Clone, Default)]
pub struct RepoSummary {
    /// Workspaces under this repo that are visible in the current
    /// mailbox. Roughly "active work for this repo".
    pub active: usize,
    /// Workspaces with at least one indicator demanding the user's
    /// attention: unread activity, CI failing, review pending /
    /// changes-requested, agent in `Asking` state. Configurable in
    /// the future; defaults are the indicators pilot already
    /// surfaces as badges on workspace rows.
    pub attention: usize,
}

pub struct Sidebar {
    id: PaneId,
    workspaces: HashMap<SessionKey, Workspace>,
    /// Derived view: workspaces filtered by mailbox, grouped by repo,
    /// each group sorted by updated_at desc. Headers are interleaved
    /// with workspace rows in render order; the cursor navigates
    /// only over workspace rows (headers are skipped).
    visible: Vec<VisibleRow>,
    /// Repos the user has collapsed. Workspace rows under collapsed
    /// repos are still tracked in `workspaces` but are skipped when
    /// `recompute_visible` rebuilds the view.
    collapsed_repos: BTreeSet<String>,
    /// Per-repo counters computed during `recompute_visible`. Keys
    /// are the same display strings used by `VisibleRow::RepoHeader`.
    repo_summaries: BTreeMap<String, RepoSummary>,
    /// Index into `visible`. Always points at a `Workspace` variant
    /// when there is at least one — `recompute_visible` and the
    /// j/k handlers maintain that invariant.
    cursor: usize,
    mailbox: Mailbox,
    /// If set, `Shift-X` has been pressed once on this row. A
    /// second press executes the kill. Any other key clears.
    kill_pending: Option<SessionKey>,
    /// Per-key agent id map. Defaults to `c => "claude", x => "codex",
    /// u => "cursor"`. AppRoot can override via `with_agent_shortcuts`
    /// for users with Aider / custom CLIs configured.
    agent_shortcuts: HashMap<char, String>,
    /// Mirror of the daemon's live-terminals set, scoped to what we
    /// need for the workspace-row runner badges (e.g. ` C  S 2` for
    /// one Claude + two shells running). Populated from `Event::Snapshot`
    /// and kept in sync via `TerminalSpawned` / `TerminalExited`.
    running_terminals: HashMap<TerminalId, (SessionKey, TerminalKind)>,
}

impl Sidebar {
    pub fn new(id: PaneId) -> Self {
        // Lowercase, easy to type, and mirrors the hint-bar:
        //   c → claude, x → codex, u → cursor (`s` is the shell, handled
        //   separately because it isn't an agent registered in the
        //   agent registry).
        let mut agent_shortcuts = HashMap::new();
        agent_shortcuts.insert('c', "claude".to_string());
        agent_shortcuts.insert('x', "codex".to_string());
        agent_shortcuts.insert('u', "cursor".to_string());
        Self {
            id,
            workspaces: HashMap::new(),
            visible: Vec::new(),
            collapsed_repos: BTreeSet::new(),
            repo_summaries: BTreeMap::new(),
            cursor: 0,
            mailbox: Mailbox::Inbox,
            kill_pending: None,
            agent_shortcuts,
            running_terminals: HashMap::new(),
        }
    }

    /// Override the default c→claude / C→codex mapping. Keys are
    /// single characters; case matters (`c` and `C` are distinct).
    /// AppRoot wires this from the user's config at startup.
    pub fn with_agent_shortcuts(
        mut self,
        shortcuts: impl IntoIterator<Item = (char, String)>,
    ) -> Self {
        self.agent_shortcuts = shortcuts.into_iter().collect();
        self
    }

    /// Which agents are currently keymapped. For overlays / help
    /// rendering that want to show the user what's available.
    pub fn agent_shortcuts(&self) -> &HashMap<char, String> {
        &self.agent_shortcuts
    }

    // ── Observability helpers (for tests + for AppRoot / RightPane) ────

    pub fn selected_session_key(&self) -> Option<&SessionKey> {
        match self.visible.get(self.cursor)? {
            VisibleRow::Workspace(k) => Some(k),
            VisibleRow::Session { workspace, .. } => Some(workspace),
            VisibleRow::RepoHeader(_) => None,
        }
    }

    /// The specific session id under the cursor, if the cursor is on
    /// a Session sub-row. Workspace rows return `None`, leaving the
    /// daemon to pick the workspace's default session.
    pub fn selected_session_id(&self) -> Option<SessionId> {
        match self.visible.get(self.cursor)? {
            VisibleRow::Session { session_id, .. } => Some(*session_id),
            _ => None,
        }
    }

    /// Read-only view of the rendered rows. Tests + the layout helper
    /// use this to assert grouping without poking at internals.
    pub fn visible_rows(&self) -> &[VisibleRow] {
        &self.visible
    }

    /// Translate a mouse click row inside `area` to a visible-row
    /// index, and move the cursor onto it. Returns true if the click
    /// landed on a selectable row (not a repo header / outside the
    /// content area). Header rows + clicks above the content area
    /// are ignored.
    pub fn click_to_select(&mut self, area: Rect, click_row: u16) -> bool {
        // Mirror the constants from `render`.
        const HEADER_HEIGHT: u16 = 5;
        if click_row < area.y + HEADER_HEIGHT {
            return false;
        }
        let idx = (click_row - area.y - HEADER_HEIGHT) as usize;
        match self.visible.get(idx) {
            Some(VisibleRow::Workspace(_)) | Some(VisibleRow::Session { .. }) => {
                self.cursor = idx;
                true
            }
            // RepoHeader / out-of-bounds → no-op so the click doesn't
            // strand the cursor on a non-selectable row.
            _ => false,
        }
    }

    /// Move the cursor onto the workspace row matching `key`. Returns
    /// true on a hit. Used by `--workspace` preselect on startup.
    pub fn focus_workspace_key(&mut self, key: &SessionKey) -> bool {
        for (i, row) in self.visible.iter().enumerate() {
            if let VisibleRow::Workspace(k) = row
                && k == key
            {
                self.cursor = i;
                return true;
            }
        }
        false
    }

    /// Move the cursor onto the session sub-row matching `id`. No-op
    /// when the row isn't visible — caller must already have aligned
    /// the workspace via `focus_workspace_key`.
    pub fn focus_session_id(&mut self, id: SessionId) -> bool {
        for (i, row) in self.visible.iter().enumerate() {
            if let VisibleRow::Session { session_id, .. } = row
                && *session_id == id
            {
                self.cursor = i;
                return true;
            }
        }
        false
    }

    /// The workspace under the cursor, or `None` if the visible list
    /// is empty. The TUI's right pane / terminal stack consume this
    /// so they always reflect the sidebar's selection.
    pub fn selected_workspace(&self) -> Option<&Workspace> {
        self.selected_session_key()
            .and_then(|k| self.workspaces.get(k))
    }

    pub fn mailbox(&self) -> Mailbox {
        self.mailbox
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn visible_count(&self) -> usize {
        self.visible.len()
    }

    /// How many *workspace* rows are visible (excluding repo headers).
    /// Title bar uses this — counting headers would be confusing
    /// because they're navigation chrome, not items.
    pub fn workspace_count(&self) -> usize {
        self.visible
            .iter()
            .filter(|r| matches!(r, VisibleRow::Workspace(_)))
            .count()
    }

    /// Step the cursor `delta` selectable rows from its current
    /// position, skipping repo headers. Workspace rows AND session
    /// sub-rows are selectable; only headers are not. Clamps at the
    /// first/last selectable row.
    /// True if the cursor sits on a repo header row.
    pub fn cursor_on_repo_header(&self) -> bool {
        matches!(self.visible.get(self.cursor), Some(VisibleRow::RepoHeader(_)))
    }

    fn move_cursor_by(&mut self, delta: isize) {
        if delta == 0 || self.visible.is_empty() {
            return;
        }
        // Navigate over EVERYTHING — workspaces, sessions, and repo
        // headers. Stopping on headers is what lets the user expand
        // a collapsed repo (Space toggles whatever the cursor's on).
        let selectable: Vec<usize> = (0..self.visible.len()).collect();
        if selectable.is_empty() {
            return;
        }
        let pos = selectable
            .iter()
            .position(|i| *i == self.cursor)
            .unwrap_or(0);
        let target = (pos as isize + delta).clamp(0, selectable.len() as isize - 1) as usize;
        self.cursor = selectable[target];
    }

    pub fn kill_armed(&self) -> Option<&SessionKey> {
        self.kill_pending.as_ref()
    }

    /// Total unread activity items across all VISIBLE workspaces. Used
    /// by the top header's `N new` badge — only the current mailbox's
    /// unread is counted, so cycling Inbox→Snoozed shows different
    /// totals (snoozed PRs aren't "in your face" by definition).
    fn total_unread_count(&self) -> usize {
        self.visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => self.workspaces.get(k),
                _ => None,
            })
            .map(|w| w.unread_count())
            .sum()
    }

    /// Number of visible workspaces with at least one session whose
    /// agent is currently waiting on the user (`SessionRunState::Asking`).
    /// Drives the `? N input` indicator in the top header — a quick
    /// "agents stuck on prompts" tally.
    fn input_pending_count(&self) -> usize {
        self.visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => self.workspaces.get(k),
                _ => None,
            })
            .filter(|w| {
                w.sessions
                    .iter()
                    .any(|s| matches!(s.state, pilot_core::SessionRunState::Asking))
            })
            .count()
    }

    /// Visible workspaces with a failing CI on the primary task.
    /// Drives the `N CI` summary — at-a-glance "how many of my PRs
    /// are broken right now."
    fn ci_failing_count(&self) -> usize {
        self.visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => self.workspaces.get(k),
                _ => None,
            })
            .filter(|w| {
                w.primary_task()
                    .map(|t| {
                        matches!(
                            t.ci,
                            pilot_core::CiStatus::Failure | pilot_core::CiStatus::Mixed
                        )
                    })
                    .unwrap_or(false)
            })
            .count()
    }

    /// Visible workspaces where a reviewer is requested or a review
    /// is pending — the "N review" half of the stats row.
    fn review_pending_count(&self) -> usize {
        self.visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => self.workspaces.get(k),
                _ => None,
            })
            .filter(|w| {
                w.primary_task()
                    .map(|t| {
                        matches!(
                            t.review,
                            pilot_core::ReviewStatus::Pending
                                | pilot_core::ReviewStatus::ChangesRequested
                        ) || !t.reviewers.is_empty()
                    })
                    .unwrap_or(false)
            })
            .count()
    }

    /// Stable single-letter key for a runner kind. Drives the workspace
    /// row badge — `claude` → `C`, `codex` → `X`, `cursor` → `U`,
    /// `shell` → `S`, log tail → `L`, generic agent → `A`.
    fn badge_letter(kind: &TerminalKind) -> char {
        match kind {
            TerminalKind::Agent(id) => match id.as_str() {
                "claude" => 'C',
                "codex" => 'X',
                "cursor" => 'U',
                _ => id.chars().next().map(|c| c.to_ascii_uppercase()).unwrap_or('A'),
            },
            TerminalKind::Shell => 'S',
            TerminalKind::LogTail { .. } => 'L',
        }
    }

    /// Aggregate live terminals on `key` into a list of `(letter, count)`
    /// pairs for the sidebar's runner badge. Stable order: agents first
    /// (alphabetical by letter), shells last so the eye lands on the
    /// agent state first. Returns `[]` when the workspace has no live
    /// terminals.
    fn runner_badges(&self, key: &SessionKey) -> Vec<(char, usize)> {
        let mut counts: HashMap<char, usize> = HashMap::new();
        for (sk, kind) in self.running_terminals.values() {
            if sk == key {
                *counts.entry(Self::badge_letter(kind)).or_default() += 1;
            }
        }
        let mut entries: Vec<(char, usize)> = counts.into_iter().collect();
        entries.sort_by_key(|(c, _)| match *c {
            'S' => (1, 'S'),
            other => (0, other),
        });
        entries
    }

    /// Toggle the collapsed flag for the repo at or above the
    /// cursor. Used by `Space`. Resolution:
    ///   - cursor on a `RepoHeader` → toggle that header.
    ///   - cursor on a workspace / session → walk back to find the
    ///     nearest header (the cursor's group) and toggle that.
    /// On collapse, cursor snaps to the now-collapsed header so
    /// j/k from there land on adjacent headers cleanly.
    pub fn toggle_repo_at_cursor(&mut self) -> bool {
        let repo = match self.visible.get(self.cursor).cloned() {
            Some(VisibleRow::RepoHeader(name)) => Some(name),
            Some(VisibleRow::Workspace(_)) | Some(VisibleRow::Session { .. }) => self
                .visible
                .iter()
                .take(self.cursor + 1)
                .rev()
                .find_map(|r| match r {
                    VisibleRow::RepoHeader(name) => Some(name.clone()),
                    _ => None,
                }),
            None => None,
        };
        let Some(repo) = repo else { return false };
        let was_collapsed = self.collapsed_repos.contains(&repo);
        if was_collapsed {
            self.collapsed_repos.remove(&repo);
        } else {
            self.collapsed_repos.insert(repo.clone());
        }
        self.recompute_visible();
        // Always park the cursor on the toggled header so
        // collapse + immediately re-expand works (Space twice in a
        // row toggles the same repo).
        if let Some(idx) = self
            .visible
            .iter()
            .position(|r| matches!(r, VisibleRow::RepoHeader(n) if n == &repo))
        {
            self.cursor = idx;
        }
        true
    }

    /// Read-only view of the per-repo summary for render. Headers
    /// look up by their display name.
    pub fn repo_summary(&self, name: &str) -> Option<&RepoSummary> {
        self.repo_summaries.get(name)
    }

    /// True when the repo is currently collapsed (used by the
    /// header render to pick `▾` vs `▸`).
    pub fn is_repo_collapsed(&self, name: &str) -> bool {
        self.collapsed_repos.contains(name)
    }

    fn recompute_visible(&mut self) {
        let now = chrono::Utc::now();
        let mailbox = self.mailbox;

        // Filter to the current mailbox. Each branch encodes ONE
        // semantic bucket — see Mailbox docs for the full picture.
        let mut filtered: Vec<(&SessionKey, &Workspace)> = self
            .workspaces
            .iter()
            .filter(|(_, w)| match mailbox {
                Mailbox::Inbox => {
                    // Actionable: not snoozed AND primary task is
                    // alive (open/draft/in-progress/in-review).
                    if w.is_snoozed(now) {
                        return false;
                    }
                    match w.primary_task() {
                        Some(t) => !matches!(
                            t.state,
                            pilot_core::TaskState::Merged | pilot_core::TaskState::Closed
                        ),
                        // Empty workspaces show up in Inbox so the
                        // user can act on them.
                        None => true,
                    }
                }
                Mailbox::Inactive => {
                    // Historical: primary task already merged/closed.
                    // Snoozed-and-inactive lands in Snoozed (snooze
                    // wins) so no overlap.
                    if w.is_snoozed(now) {
                        return false;
                    }
                    matches!(
                        w.primary_task().map(|t| t.state),
                        Some(pilot_core::TaskState::Merged) | Some(pilot_core::TaskState::Closed)
                    )
                }
                Mailbox::Snoozed => w.is_snoozed(now),
            })
            .collect();

        // Group by repo. Workspaces with no primary task or no repo
        // string land under a synthetic group so they're still visible.
        const NO_REPO: &str = "(no repo)";
        let repo_of = |w: &Workspace| -> String {
            w.primary_task()
                .and_then(|t| t.repo.clone())
                .unwrap_or_else(|| NO_REPO.to_string())
        };
        // Sort within each group by updated_at desc; group ordering is
        // alphabetical so repo headers don't reshuffle on every poll.
        filtered.sort_by(|(ka, a), (kb, b)| {
            let ra = repo_of(a);
            let rb = repo_of(b);
            ra.cmp(&rb).then_with(|| {
                let a_ts = a
                    .primary_task()
                    .map(|t| t.updated_at)
                    .unwrap_or(a.created_at);
                let b_ts = b
                    .primary_task()
                    .map(|t| t.updated_at)
                    .unwrap_or(b.created_at);
                b_ts.cmp(&a_ts).then_with(|| ka.as_str().cmp(kb.as_str()))
            })
        });

        let prior_key = self.selected_session_key().cloned();
        let prior_session = self.selected_session_id();
        let mut visible: Vec<VisibleRow> = Vec::with_capacity(filtered.len() + 4);
        let mut summaries: BTreeMap<String, RepoSummary> = BTreeMap::new();
        let mut current_repo: Option<String> = None;
        for (k, w) in &filtered {
            let repo = repo_of(w);
            if current_repo.as_deref() != Some(&repo) {
                visible.push(VisibleRow::RepoHeader(repo.clone()));
                current_repo = Some(repo.clone());
                summaries.entry(repo.clone()).or_default();
            }
            // Update this repo's summary whether or not its rows are
            // shown — the counters reflect the underlying data, not
            // the collapse state.
            let summary = summaries.entry(repo.clone()).or_default();
            summary.active += 1;
            if workspace_needs_attention(w) {
                summary.attention += 1;
            }
            // Skip workspace + session rows if the repo is collapsed.
            // The header is still emitted so the user can re-expand.
            if self.collapsed_repos.contains(&repo) {
                continue;
            }
            visible.push(VisibleRow::Workspace((*k).clone()));
            // Session sub-rows appear only when there are 2+ sessions
            // on a workspace.
            if w.session_count() >= 2 {
                let mut sessions: Vec<&pilot_core::WorkspaceSession> = w.sessions.iter().collect();
                sessions.sort_by_key(|s| s.created_at);
                for s in sessions {
                    visible.push(VisibleRow::Session {
                        workspace: (*k).clone(),
                        session_id: s.id,
                    });
                }
            }
        }
        self.visible = visible;
        self.repo_summaries = summaries;

        // Preserve cursor across reorderings. Match by (workspace
        // key, session id) tuple so a cursor sitting on a session
        // sub-row stays on that exact row when sibling sessions
        // come and go. Workspace-row cursors match by key alone.
        if let Some(key) = prior_key {
            for (i, row) in self.visible.iter().enumerate() {
                let matched = match row {
                    VisibleRow::Workspace(k) => *k == key && prior_session.is_none(),
                    VisibleRow::Session {
                        workspace,
                        session_id,
                    } => *workspace == key && Some(*session_id) == prior_session,
                    VisibleRow::RepoHeader(_) => false,
                };
                if matched {
                    self.cursor = i;
                    return;
                }
            }
            // Session vanished but workspace still here — fall back
            // to the workspace row.
            for (i, row) in self.visible.iter().enumerate() {
                if matches!(row, VisibleRow::Workspace(k) if *k == key) {
                    self.cursor = i;
                    return;
                }
            }
        }
        // Prior selection vanished entirely. Land on the first
        // selectable row (workspace or session), or 0 if nothing left.
        self.cursor = self
            .visible
            .iter()
            .position(|r| matches!(r, VisibleRow::Workspace(_) | VisibleRow::Session { .. }))
            .unwrap_or(0);
    }
}

/// Inherent methods. Names match what the legacy `tui_kit::Pane`
/// trait used to require, so the old `app::run` path's concrete-type
/// calls (`app.sidebar.handle_key(...)`) still resolve here without
/// the trait being in scope.
impl Sidebar {
    /// Stable pane id assigned at construction.
    pub fn id(&self) -> PaneId {
        self.id
    }

    /// Title rendered in the pane border.
    pub fn title(&self) -> &str {
        "Inbox"
    }

    /// Bindings advertised in the hint bar.
    pub fn keymap(&self) -> &'static [crate::Binding] {
        use crate::Binding;
        &[
            Binding { keys: "j/k", label: "navigate" },
            Binding { keys: "Tab", label: "next pane" },
            Binding { keys: "Enter", label: "open" },
            Binding { keys: "n", label: "new workspace" },
            Binding { keys: "E", label: "open editor" },
            Binding { keys: "Space", label: "fold repo" },
            Binding { keys: "s", label: "shell" },
            Binding { keys: "c", label: "claude" },
            Binding { keys: "x", label: "codex" },
            Binding { keys: "u", label: "cursor" },
            Binding { keys: "m", label: "mark all read" },
            Binding { keys: "/", label: "search" },
            Binding { keys: "e", label: "last error" },
            Binding { keys: "?", label: "help" },
            Binding { keys: "q q", label: "quit" },
        ]
    }

    pub fn detachable(&self) -> Option<crate::DetachSpec> {
        // Cursor on a session sub-row → detach that specific session.
        // Cursor on a workspace row → detach the whole workspace
        // (both spawn the same kind of child pilot — different
        // arg shape).
        match self.visible.get(self.cursor)? {
            VisibleRow::Session {
                workspace,
                session_id,
            } => Some(crate::DetachSpec {
                layout: "session",
                args: vec![
                    "--workspace".to_string(),
                    workspace.as_str().to_string(),
                    "--session".to_string(),
                    session_id.0.to_string(),
                ],
            }),
            VisibleRow::Workspace(k) => Some(crate::DetachSpec {
                layout: "workspace",
                args: vec!["--workspace".to_string(), k.as_str().to_string()],
            }),
            VisibleRow::RepoHeader(_) => None,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, cmds: &mut Vec<Command>) -> PaneOutcome {
        // Any key other than Shift-X disarms the kill confirmation.
        let is_shift_x =
            key.code == KeyCode::Char('X') && key.modifiers.contains(KeyModifiers::SHIFT);
        if self.kill_pending.is_some() && !is_shift_x {
            self.kill_pending = None;
        }

        match (key.code, key.modifiers) {
            // ── Navigation ────────────────────────────────────────────
            (KeyCode::Char('j'), m) | (KeyCode::Down, m) if !m.contains(KeyModifiers::SHIFT) => {
                self.move_cursor_by(1);
                PaneOutcome::Consumed
            }
            (KeyCode::Char('k'), m) | (KeyCode::Up, m) if !m.contains(KeyModifiers::SHIFT) => {
                self.move_cursor_by(-1);
                PaneOutcome::Consumed
            }
            // ── Collapse / expand the cursor's repo group ─────────────
            // Space toggles the parent repo of whatever workspace /
            // session row the cursor is on. Mimics file-tree TUIs
            // (yazi, nnn, lf) where Space folds a directory.
            (KeyCode::Char(' '), KeyModifiers::NONE) => {
                self.toggle_repo_at_cursor();
                PaneOutcome::Consumed
            }

            // ── Spawn / open ──────────────────────────────────────────
            // Any Char key listed in `agent_shortcuts` spawns that
            // agent for the selected session. Defaults: `c` → Claude,
            // `C` → Codex. AppRoot can remap at startup via
            // `with_agent_shortcuts`. Keys NOT in the map bubble up,
            // so overlays / other components get a fair shot.
            (KeyCode::Char(c), m)
                if self.agent_shortcuts.contains_key(&c)
                    && !m.contains(KeyModifiers::CONTROL)
                    && !m.contains(KeyModifiers::ALT) =>
            {
                if let Some(session_key) = self.selected_session_key().cloned() {
                    let agent_id = self.agent_shortcuts.get(&c).cloned().unwrap_or_default();
                    tracing::info!(
                        key = %c, %session_key, agent_id = %agent_id,
                        "sidebar: emitting Spawn(Agent)"
                    );
                    cmds.push(Command::Spawn {
                        session_key,
                        // The selected session sub-row, if any, scopes
                        // the spawn into a specific worktree. None →
                        // the daemon picks/auto-creates the workspace's
                        // default session.
                        session_id: self.selected_session_id(),
                        kind: TerminalKind::Agent(agent_id),
                        cwd: None,
                    });
                } else {
                    tracing::warn!(
                        key = %c,
                        "sidebar: agent shortcut pressed but no session selected — spawn dropped"
                    );
                }
                PaneOutcome::Consumed
            }
            // `s` for shell — used to be `b` (for "bash") but the
            // hint bar reads better as "S shell / C claude / X codex /
            // U cursor" all-lowercase, and `s` is mnemonic.
            (KeyCode::Char('s'), KeyModifiers::NONE) => {
                if let Some(session_key) = self.selected_session_key().cloned() {
                    tracing::info!(%session_key, "sidebar: emitting Spawn(Shell)");
                    cmds.push(Command::Spawn {
                        session_key,
                        session_id: self.selected_session_id(),
                        kind: TerminalKind::Shell,
                        cwd: None,
                    });
                } else {
                    tracing::warn!(
                        "sidebar: shell shortcut pressed but no session selected — spawn dropped"
                    );
                }
                PaneOutcome::Consumed
            }

            // ── Session actions ───────────────────────────────────────
            (KeyCode::Char('m'), KeyModifiers::NONE) => {
                if let Some(session_key) = self.selected_session_key().cloned() {
                    cmds.push(Command::MarkRead { session_key });
                }
                PaneOutcome::Consumed
            }
            (KeyCode::Char('g'), KeyModifiers::NONE) => {
                cmds.push(Command::Refresh);
                PaneOutcome::Consumed
            }
            (KeyCode::Char('z'), KeyModifiers::NONE) => {
                let Some(session_key) = self.selected_session_key().cloned() else {
                    return PaneOutcome::Consumed;
                };
                let now = chrono::Utc::now();
                let already = self
                    .workspaces
                    .get(&session_key)
                    .map(|w| w.is_snoozed(now))
                    .unwrap_or(false);
                if already {
                    cmds.push(Command::Unsnooze { session_key });
                } else {
                    cmds.push(Command::Snooze {
                        session_key,
                        until: now + chrono::Duration::hours(4),
                    });
                }
                PaneOutcome::Consumed
            }
            (KeyCode::Char('Z'), m) if m.contains(KeyModifiers::SHIFT) => {
                if let Some(session_key) = self.selected_session_key().cloned() {
                    let now = chrono::Utc::now();
                    cmds.push(Command::Snooze {
                        session_key,
                        until: now + chrono::Duration::days(365),
                    });
                }
                PaneOutcome::Consumed
            }

            // ── Mailbox cycle (Inbox → Inactive → Snoozed → Inbox)
            (KeyCode::Char('S'), m) if m.contains(KeyModifiers::SHIFT) => {
                self.mailbox = match self.mailbox {
                    Mailbox::Inbox => Mailbox::Inactive,
                    Mailbox::Inactive => Mailbox::Snoozed,
                    Mailbox::Snoozed => Mailbox::Inbox,
                };
                // New mailbox → reset cursor to top; old cursor key is
                // almost certainly not visible in the other mailbox.
                self.cursor = 0;
                self.recompute_visible();
                PaneOutcome::Consumed
            }

            // ── Kill session (two-press confirmation) ─────────────────
            (KeyCode::Char('X'), m) if m.contains(KeyModifiers::SHIFT) => {
                let Some(session_key) = self.selected_session_key().cloned() else {
                    return PaneOutcome::Consumed;
                };
                if self.kill_pending.as_ref() == Some(&session_key) {
                    self.kill_pending = None;
                    cmds.push(Command::Kill { session_key });
                } else {
                    self.kill_pending = Some(session_key);
                }
                PaneOutcome::Consumed
            }

// Anything else: bubble up. Tab / Help / `?` / overlays /
            // quit are handled by parent components.
            _ => PaneOutcome::Pass,
        }
    }

    pub fn on_event(&mut self, event: &Event) {
        match event {
            Event::Snapshot {
                workspaces,
                terminals,
                ..
            } => {
                self.workspaces.clear();
                for w in workspaces {
                    let key: SessionKey = (&w.key).into();
                    self.workspaces.insert(key, w.clone());
                }
                self.running_terminals.clear();
                for t in terminals {
                    self.running_terminals
                        .insert(t.terminal_id, (t.session_key.clone(), t.kind.clone()));
                }
                self.cursor = 0;
                self.recompute_visible();
            }
            Event::TerminalSpawned {
                terminal_id,
                session_key,
                kind,
            } => {
                self.running_terminals
                    .insert(*terminal_id, (session_key.clone(), kind.clone()));
            }
            Event::TerminalExited { terminal_id, .. } => {
                self.running_terminals.remove(terminal_id);
            }
            Event::WorkspaceUpserted(workspace) => {
                let key: SessionKey = (&workspace.key).into();
                self.workspaces.insert(key, (**workspace).clone());
                self.recompute_visible();
            }
            Event::WorkspaceRemoved(key) => {
                let session_key: SessionKey = key.into();
                self.workspaces.remove(&session_key);
                self.recompute_visible();
            }
            Event::SessionCreated(session) => {
                let key: SessionKey = (&session.workspace_key).into();
                if let Some(w) = self.workspaces.get_mut(&key) {
                    // Idempotent — the canonical add_session will
                    // refuse to duplicate if the daemon resends.
                    if w.find_session(session.id).is_none() {
                        w.sessions.push((**session).clone());
                    }
                    self.recompute_visible();
                }
            }
            Event::SessionEnded {
                workspace_key,
                session_id,
            } => {
                let key: SessionKey = workspace_key.into();
                if let Some(w) = self.workspaces.get_mut(&key) {
                    w.remove_session(*session_id);
                    self.recompute_visible();
                }
            }
            _ => {}
        }
    }

    pub fn render(&mut self, area: Rect, frame: &mut Frame, focused: bool) {
        // V1-style header strip:
        //   row 0: PILOT  N  ● N new  ? N input  [7d]
        //   row 1: s  filter (needs:reply ci:failed ...)
        //   row 2: N CI  N review               (omitted when both 0)
        //   row 3: ── divider ────────────────
        //   row 4: blank
        //   row 5+: content
        let theme = crate::theme::current();
        let now = chrono::Utc::now();
        let mailbox_label = match self.mailbox {
            Mailbox::Inbox => "PILOT",
            Mailbox::Inactive => "INACTIVE",
            Mailbox::Snoozed => "SNOOZED",
        };
        let count = self.workspace_count();
        let unread = self.total_unread_count();
        let input_pending = self.input_pending_count();
        let ci_failing = self.ci_failing_count();
        let review_pending = self.review_pending_count();

        let l_pad: u16 = 1;
        let r_pad: u16 = 3;
        let inner_width = area.width.saturating_sub(l_pad + r_pad);

        // Row 0 — app title + counts.
        let mut header_spans: Vec<Span> = Vec::with_capacity(12);
        header_spans.push(Span::styled(mailbox_label, theme.title(focused)));
        header_spans.push(Span::raw("  "));
        header_spans.push(Span::styled(
            count.to_string(),
            Style::default()
                .fg(theme.warn)
                .add_modifier(Modifier::BOLD),
        ));
        if unread > 0 {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled(
                "● ",
                Style::default().fg(theme.hover).add_modifier(Modifier::BOLD),
            ));
            header_spans.push(Span::styled(
                format!("{unread} new"),
                Style::default().fg(theme.hover).add_modifier(Modifier::BOLD),
            ));
        }
        if input_pending > 0 {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled(
                "? ",
                Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            ));
            header_spans.push(Span::styled(
                format!("{input_pending} input"),
                Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            ));
        }
        header_spans.push(Span::raw("  "));
        header_spans.push(Span::styled(
            "[7d]",
            Style::default().fg(theme.text_dim),
        ));

        let row0 = Rect::new(area.x + l_pad, area.y, inner_width, 1.min(area.height));
        frame.render_widget(Paragraph::new(Line::from(header_spans)), row0);

        // Row 1 — filter hint.
        if area.height >= 2 {
            let row1 = Rect::new(area.x + l_pad, area.y + 1, inner_width, 1);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        "/ ",
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "filter (needs:reply ci:failed …)",
                        Style::default().fg(theme.text_dim),
                    ),
                ])),
                row1,
            );
        }

        // Row 2 — stats summary, only when there's something to summarize.
        let mut stats_spans: Vec<Span> = Vec::new();
        if ci_failing > 0 {
            stats_spans.push(Span::styled(
                ci_failing.to_string(),
                Style::default().fg(theme.error).add_modifier(Modifier::BOLD),
            ));
            stats_spans.push(Span::styled(" CI", Style::default().fg(theme.text_dim)));
        }
        if review_pending > 0 {
            if !stats_spans.is_empty() {
                stats_spans.push(Span::raw("  "));
            }
            stats_spans.push(Span::styled(
                review_pending.to_string(),
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
            ));
            stats_spans.push(Span::styled(
                " review",
                Style::default().fg(theme.text_dim),
            ));
        }
        if !stats_spans.is_empty() && area.height >= 3 {
            let row2 = Rect::new(area.x + l_pad, area.y + 2, inner_width, 1);
            frame.render_widget(Paragraph::new(Line::from(stats_spans)), row2);
        }

        // Row 3 — thin grey divider.
        if area.height >= 4 {
            let div_area = Rect::new(area.x + l_pad, area.y + 3, inner_width, 1);
            let divider = "─".repeat(div_area.width as usize);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(divider, theme.divider()))),
                div_area,
            );
        }

        // Content starts at row 5 (skipping a blank row for breathing
        // room above the first item).
        const HEADER_HEIGHT: u16 = 5;
        let inner = Rect {
            x: area.x + l_pad,
            y: area.y + HEADER_HEIGHT,
            width: inner_width,
            height: area.height.saturating_sub(HEADER_HEIGHT),
        };

        let row_budget = inner_width as usize;
        let lines: Vec<Line> = self
            .visible
            .iter()
            .enumerate()
            .map(|(i, row)| match row {
                VisibleRow::RepoHeader(name) => {
                    use crate::components::icons;
                    let collapsed = self.collapsed_repos.contains(name);
                    let glyph = if collapsed { "▸" } else { "▾" };
                    let is_cursor = i == self.cursor;
                    let row_bg = if is_cursor && focused {
                        Some(theme.row_focused())
                    } else if is_cursor {
                        Some(theme.row_unfocused())
                    } else {
                        None
                    };
                    // Cursor caret on the left mirrors workspace rows so
                    // the user can see the cursor parked on a header
                    // (otherwise navigating onto a header looks like a
                    // dropped key — Space-to-toggle wouldn't be
                    // discoverable).
                    let caret = if is_cursor { "▸ " } else { "  " };
                    let glyph_style = match row_bg {
                        Some(bg) => bg,
                        None => Style::default().fg(theme.text_dim),
                    };
                    let mut spans: Vec<Span> = vec![
                        Span::styled(caret.to_string(), glyph_style),
                        Span::styled(format!("{glyph} "), glyph_style),
                        Span::styled(
                            format!("{} {}", icons::REPO, name),
                            row_bg
                                .unwrap_or_default()
                                .fg(theme.warn)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ];
                    if let Some(s) = self.repo_summaries.get(name) {
                        spans.push(Span::styled(
                            format!("  {}", s.active),
                            row_bg.unwrap_or_default().fg(theme.text_dim),
                        ));
                        if s.attention > 0 {
                            spans.push(Span::styled(
                                format!("  ● {}", s.attention),
                                row_bg
                                    .unwrap_or_default()
                                    .fg(theme.hover)
                                    .add_modifier(Modifier::BOLD),
                            ));
                        }
                    }
                    let _ = row_budget;
                    Line::from(spans)
                }
                VisibleRow::Workspace(key) => {
                    let workspace = self.workspaces.get(key);
                    let task = workspace.and_then(|w| w.primary_task());
                    let raw_title = task
                        .map(|t| t.title.as_str())
                        .unwrap_or_else(|| workspace.map(|w| w.name.as_str()).unwrap_or("?"));
                    let is_cursor = i == self.cursor;
                    // Cursor wins over per-element coloring — the
                    // highlight needs to be unambiguous, even at the
                    // cost of hiding the PR-number / kind colors.
                    let row_style = if is_cursor && focused {
                        theme.row_focused()
                    } else if is_cursor {
                        theme.row_unfocused()
                    } else {
                        Style::default()
                    };
                    let prefix = if is_cursor { "  ▸ " } else { "    " };
                    let kill_mark = if self.kill_pending.as_ref() == Some(key) {
                        " [kill?]"
                    } else {
                        ""
                    };

                    // Decompose the title into [#NNN] [KIND] rest, and
                    // pre-compute the visual width of each chunk so
                    // truncation only ever clips the rest-of-title.
                    let pr_num = task.and_then(crate::components::task_label::pr_number);
                    let parsed = crate::components::task_label::parse_conventional_prefix(
                        raw_title,
                    );
                    let (kind, body_title) = match parsed {
                        Some((k, rest)) => (Some(k), rest),
                        None => (None, raw_title),
                    };

                    let mut spans: Vec<Span> = Vec::with_capacity(8);
                    spans.push(Span::styled(prefix, row_style));

                    let mut used = visual_width(prefix);
                    let push = |span: Span<'static>, used: &mut usize, spans: &mut Vec<Span<'static>>| {
                        let w = visual_width(span.content.as_ref());
                        *used = used.saturating_add(w);
                        spans.push(span);
                    };

                    if let Some(n) = pr_num {
                        let label = format!("#{n}");
                        let style = if is_cursor {
                            row_style
                        } else {
                            Style::default()
                                .fg(crate::components::task_label::pr_number_color(n))
                                .add_modifier(Modifier::BOLD)
                        };
                        push(
                            Span::styled(label.clone(), style),
                            &mut used,
                            &mut spans,
                        );
                        // Role marker suffix: `#7204R` reads as "PR
                        // 7204, your role: Reviewer". Dim/colored char
                        // immediately after the number — keeps the
                        // visual weight on the PR# while still
                        // signalling at-a-glance.
                        if let Some(role) = task.map(|t| t.role) {
                            let (letter, color) = role_badge(theme, role);
                            let role_style = if is_cursor {
                                row_style
                            } else {
                                Style::default().fg(color).add_modifier(Modifier::BOLD)
                            };
                            push(
                                Span::styled(letter.to_string(), role_style),
                                &mut used,
                                &mut spans,
                            );
                        }
                        // Inline unread dot — `●` glyph in the hover
                        // accent when this workspace has any unread
                        // activity. Sits right after the role suffix
                        // so the eye lands on it during the natural
                        // PR-number scan.
                        if workspace.is_some_and(|w| w.unread_count() > 0) {
                            let dot_style = if is_cursor {
                                row_style
                            } else {
                                Style::default()
                                    .fg(theme.hover)
                                    .add_modifier(Modifier::BOLD)
                            };
                            push(Span::styled(" ●", dot_style), &mut used, &mut spans);
                        }
                        push(Span::styled(" ", row_style), &mut used, &mut spans);
                    }

                    if let Some(k) = kind {
                        let label = format!("[{}]", k.label());
                        let style = if is_cursor {
                            row_style
                        } else {
                            Style::default()
                                .fg(crate::components::task_label::kind_color(k))
                                .add_modifier(Modifier::BOLD)
                        };
                        push(Span::styled(label, style), &mut used, &mut spans);
                        push(Span::styled(" ", row_style), &mut used, &mut spans);
                    }

                    // Right-side trailer (right-to-left order in the
                    // visible output):
                    //
                    //     ...title...  [badges]   [status:6]  [time:4]
                    //
                    // **Time is always the rightmost column at a fixed
                    // offset.** Status sits to its left at another
                    // fixed offset. Badges (live runners) float between
                    // the title and the fixed columns — when present,
                    // the title shrinks, but status/time never move.
                    // This keeps the eye scanning down a stable time
                    // column even on rows with running shells/agents.
                    let badges = self.runner_badges(key);
                    let badges_len = if badges.is_empty() {
                        0
                    } else {
                        // Pill width: ` X ` = 3, ` X×2 ` = 5 (×
                        // separator + count digit). Multi-digit counts
                        // (10+ shells) bump width by another cell —
                        // accept the over-budget rather than truncate.
                        let pills: usize = badges
                            .iter()
                            .map(|(_, n)| {
                                if *n > 1 {
                                    4 + n.to_string().chars().count()
                                } else {
                                    3
                                }
                            })
                            .sum();
                        let between = badges.len().saturating_sub(1);
                        pills + between + 1 // +1 leading space before pills
                    };
                    let status = task.and_then(status_pill);
                    let time_text = task.map(|t| relative_time(t.updated_at, now));
                    let fixed_cols_len = if task.is_some() {
                        STATUS_COL_W + 1 + TIME_COL_W
                    } else {
                        0
                    };
                    let trailer_len = badges_len + fixed_cols_len;

                    let title_budget = row_budget
                        .saturating_sub(used)
                        .saturating_sub(visual_width(kill_mark))
                        .saturating_sub(trailer_len);
                    let title_text = truncate_ellipsis(body_title, title_budget);
                    let title_w = visual_width(&title_text);
                    used = used.saturating_add(title_w);
                    spans.push(Span::styled(title_text, row_style));
                    if !kill_mark.is_empty() {
                        used = used.saturating_add(visual_width(kill_mark));
                        spans.push(Span::styled(kill_mark, Style::default().fg(theme.error)));
                    }
                    if trailer_len > 0 {
                        // Pad between title and the trailer.
                        let pad = row_budget.saturating_sub(used).saturating_sub(trailer_len);
                        if pad > 0 {
                            spans.push(Span::styled(" ".repeat(pad), row_style));
                            used = used.saturating_add(pad);
                        }
                        // Runner badges first (between title + fixed
                        // cols) so the fixed cols always sit at the
                        // same x.
                        if !badges.is_empty() {
                            spans.push(Span::styled(" ", row_style));
                            used = used.saturating_add(1);
                            for (idx, (letter, n)) in badges.iter().enumerate() {
                                if idx > 0 {
                                    spans.push(Span::styled(" ", row_style));
                                    used = used.saturating_add(1);
                                }
                                // Single instance → ` C `. Multiple
                                // (rare — only when 2+ shells are
                                // running in one workspace) → ` C×2 `.
                                // The `×` separator makes the count
                                // unambiguous; "S2" got read as
                                // "session ID 2" by users.
                                let label = if *n > 1 {
                                    format!(" {letter}×{n} ")
                                } else {
                                    format!(" {letter} ")
                                };
                                let w = visual_width(&label);
                                spans
                                    .push(Span::styled(label, badge_pill_style(theme, *letter)));
                                used = used.saturating_add(w);
                            }
                        }
                        if fixed_cols_len > 0 {
                            // Status column — single colored pill that
                            // already includes its own padding so it
                            // hits the fixed STATUS_COL_W exactly. When
                            // there's no pill we render that many
                            // row-styled spaces to keep alignment.
                            if let Some(p) = status.as_ref() {
                                spans.push(Span::styled(p.label, p.style));
                                used = used.saturating_add(visual_width(p.label));
                            } else {
                                spans.push(Span::styled(" ".repeat(STATUS_COL_W), row_style));
                                used = used.saturating_add(STATUS_COL_W);
                            }
                            // Single-cell gutter between status + time.
                            spans.push(Span::styled(" ", row_style));
                            used = used.saturating_add(1);
                            // Time column — right-aligned in 4 cells.
                            let t = time_text.unwrap_or_default();
                            let t_w = visual_width(&t).min(TIME_COL_W);
                            let t_pad = TIME_COL_W - t_w;
                            if t_pad > 0 {
                                spans.push(Span::styled(" ".repeat(t_pad), row_style));
                                used = used.saturating_add(t_pad);
                            }
                            let time_style = if is_cursor {
                                row_style
                            } else {
                                Style::default().fg(theme.text_dim)
                            };
                            spans.push(Span::styled(t, time_style));
                            used = used.saturating_add(t_w);
                        }
                    }
                    // Cursor highlight reads as a "filled" row only
                    // when the bg colour extends past the last char to
                    // the right edge of the pane. Without this padding
                    // the highlight ends mid-row and looks broken.
                    if is_cursor && used < row_budget {
                        spans.push(Span::styled(
                            " ".repeat(row_budget - used),
                            row_style,
                        ));
                    }
                    Line::from(spans)
                }
                VisibleRow::Session {
                    workspace,
                    session_id,
                } => {
                    // Per-session sub-row, only emitted when the
                    // workspace has 2+ sessions. Indent further under
                    // the workspace row and show the session name.
                    let name = self
                        .workspaces
                        .get(workspace)
                        .and_then(|w| w.find_session(*session_id))
                        .map(|s| s.name.as_str())
                        .unwrap_or("?");
                    let is_cursor = i == self.cursor;
                    let style = if is_cursor && focused {
                        theme.row_focused()
                    } else if is_cursor {
                        theme.row_unfocused()
                    } else {
                        Style::default().fg(theme.text_dim)
                    };
                    let prefix = if is_cursor { "      ▸ " } else { "        " };
                    let name_budget = row_budget.saturating_sub(visual_width(prefix));
                    let name_text = truncate_ellipsis(name, name_budget);
                    let used = visual_width(prefix) + visual_width(&name_text);
                    let mut spans = vec![
                        Span::styled(prefix, style),
                        Span::styled(name_text, style),
                    ];
                    if is_cursor && used < row_budget {
                        spans.push(Span::styled(" ".repeat(row_budget - used), style));
                    }
                    Line::from(spans)
                }
            })
            .collect();

        let para = Paragraph::new(lines);
        frame.render_widget(para, inner);
    }
}

/// Visible width of the status column. Sized for the longest pill
/// (` CONFLICT ` = 10 cells) so the time column always lands at the
/// same offset.
const STATUS_COL_W: usize = 10;
/// Visible width of the time column. `now`/`Xm`/`Xh`/`Xd`/`Xmo` all
/// fit in 4 cells (max is `12mo`).
const TIME_COL_W: usize = 4;

/// Right-side status pill showing the most actionable problem on the
/// PR. One pill at a time, ordered by severity: merge conflict beats
/// CI failure beats CI-pending beats "behind base" beats nothing. The
/// pill is a colored block (` CONFLICT ` / ` CI FAIL ` / etc.) with
/// strong fg + colored bg — the v1 design that the user actually
/// likes; subtle text-only failed visually.
struct StatusPill {
    label: &'static str,
    style: Style,
}

/// Does this workspace want the user's attention right now? Drives
/// the "needs attention" counter on the collapsed repo header.
///
/// Returns true when ANY of:
///   - unread activity (new comments, CI events, etc. since last viewed)
///   - CI failure on the primary task
///   - review pending (or changes-requested) on the primary task
///   - any session has an agent in `Asking` state (waiting on prompt)
///   - mentioned role on the primary task
///
/// Future: surface as a config so users can pick their own
/// definition (e.g. "only count CI failures").
fn workspace_needs_attention(w: &Workspace) -> bool {
    if w.unread_count() > 0 {
        return true;
    }
    if w.sessions
        .iter()
        .any(|s| matches!(s.state, pilot_core::SessionRunState::Asking))
    {
        return true;
    }
    if let Some(t) = w.primary_task() {
        if matches!(
            t.ci,
            pilot_core::CiStatus::Failure | pilot_core::CiStatus::Mixed
        ) {
            return true;
        }
        if matches!(
            t.review,
            pilot_core::ReviewStatus::ChangesRequested
                | pilot_core::ReviewStatus::Pending
        ) {
            return true;
        }
        if matches!(t.role, pilot_core::TaskRole::Mentioned) {
            return true;
        }
    }
    false
}

fn status_pill(task: &pilot_core::Task) -> Option<StatusPill> {
    let theme = crate::theme::current();
    // Merged/closed tasks have nothing actionable — skip the pill so
    // historical rows in the Inactive mailbox stay quiet.
    if matches!(
        task.state,
        pilot_core::TaskState::Merged | pilot_core::TaskState::Closed
    ) {
        return None;
    }
    // Black-on-color reads cleaner than white-on-color for the same
    // pill styles V1 used. Indexed palette colors render as the
    // terminal's "bright" red/yellow on most setups — punchy without
    // the muddy mid-red `Color::Red` produces on dark themes.
    let pill_red = Style::default()
        .bg(Color::Indexed(196)) // bright red
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    let pill_amber = Style::default()
        .bg(Color::Indexed(214)) // warm amber, less neon than 11
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    if task.has_conflicts {
        return Some(StatusPill {
            label: " CONFLICT ",
            style: pill_red,
        });
    }
    match task.ci {
        pilot_core::CiStatus::Failure => {
            return Some(StatusPill {
                label: " CI FAIL  ",
                style: pill_red,
            });
        }
        pilot_core::CiStatus::Mixed => {
            return Some(StatusPill {
                label: " CI MIX   ",
                style: pill_amber,
            });
        }
        pilot_core::CiStatus::Pending | pilot_core::CiStatus::Running => {
            return Some(StatusPill {
                label: "       CI ",
                style: Style::default()
                    .fg(Color::Indexed(214))
                    .add_modifier(Modifier::BOLD),
            });
        }
        _ => {}
    }
    if task.is_behind_base {
        return Some(StatusPill {
            label: " BEHIND   ",
            style: Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        });
    }
    None
}

/// Compact relative time for the right-side trailer. `now` < 1m → "now",
/// < 1h → `Xm`, < 24h → `Xh`, < 30d → `Xd`, else `Xmo`. Always 2-3
/// cells so the column lines up.
fn relative_time(then: chrono::DateTime<chrono::Utc>, now: chrono::DateTime<chrono::Utc>) -> String {
    let delta = now.signed_duration_since(then);
    let secs = delta.num_seconds().max(0);
    if secs < 60 {
        return "now".into();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h");
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{days}d");
    }
    let months = days / 30;
    format!("{months}mo")
}

/// Single-letter role marker + color for the workspace row. Renders
/// as a leading badge (`A` for author, `R` for reviewer, `@` for
/// assignee, dim `·` for mentioned/done) so the user can scan the
/// inbox and pick out "PRs I have to act on" without reading titles.
///
/// All colors come from the active theme — no hardcoded RGB — so
/// the badges sit on the same palette as the rest of the UI and
/// don't fight for attention.
fn role_badge(theme: &crate::theme::Theme, role: pilot_core::TaskRole) -> (char, Color) {
    match role {
        pilot_core::TaskRole::Author => ('A', theme.success),
        pilot_core::TaskRole::Reviewer => ('R', theme.accent),
        pilot_core::TaskRole::Assignee => ('@', theme.warn),
        pilot_core::TaskRole::Mentioned => ('·', theme.text_dim),
    }
}

/// Per-letter pill style for the runner badge. Subtle bg tint (chrome
/// grey, not a saturated accent) + colored bold fg so the pills read
/// clearly without competing with status colors elsewhere on the row.
fn badge_pill_style(theme: &crate::theme::Theme, letter: char) -> Style {
    let fg = match letter {
        'C' => theme.accent,
        'X' => theme.warn,
        'U' => theme.success,
        'S' => theme.text_strong,
        _ => theme.text_strong,
    };
    Style::default()
        .bg(theme.fill)
        .fg(fg)
        .add_modifier(Modifier::BOLD)
}

/// Visual width in cells of a string, treating each char as one cell.
/// PR titles in the sidebar are ASCII in practice; the prefixes use
/// `▸` (1-cell box-drawing) which `chars().count()` gets right.
fn visual_width(s: &str) -> usize {
    s.chars().count()
}

/// Truncate `s` so it fits in `budget` cells, adding `…` when clipped.
/// Returns `s` unchanged when it already fits (cheap fast-path).
pub(crate) fn truncate_ellipsis(s: &str, budget: usize) -> String {
    let w = visual_width(s);
    if w <= budget {
        return s.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    if budget == 1 {
        return "…".to_string();
    }
    // Take `budget - 1` chars, append the ellipsis. Iterating chars
    // (not bytes) keeps multi-byte UTF-8 intact.
    let mut out: String = s.chars().take(budget - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod truncate_tests {
    use super::truncate_ellipsis;

    #[test]
    fn fits_unchanged() {
        assert_eq!(truncate_ellipsis("hello", 10), "hello");
        assert_eq!(truncate_ellipsis("hello", 5), "hello");
    }

    #[test]
    fn clips_with_ellipsis() {
        assert_eq!(truncate_ellipsis("hello world", 8), "hello w…");
    }

    #[test]
    fn zero_and_one_budgets() {
        assert_eq!(truncate_ellipsis("hello", 0), "");
        assert_eq!(truncate_ellipsis("hello", 1), "…");
    }

    #[test]
    fn handles_multibyte() {
        // Characters are kept whole (no byte-slicing into UTF-8).
        let s = "naïve résumé";
        let out = truncate_ellipsis(s, 6);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 6);
    }
}
