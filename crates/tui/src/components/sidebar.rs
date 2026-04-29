//! Sidebar — the left pane. Lists sessions, owns the cursor, handles
//! the core navigation and session-level keybindings.
//!
//! ## Why one component, not three
//!
//! v1's equivalent logic was spread across `state.selected`,
//! `nav_items`, filter functions, snooze checks, search state, and a
//! separate render function. v2 could decompose this into
//! FilterRow + SessionList + SessionRow components, but the state is
//! tightly coupled (cursor index depends on visible order, which
//! depends on filter/search/mailbox) and splitting it would just
//! recreate v1's desync surface. Keeping Sidebar as one component
//! with private state is the simpler correct answer. When a specific
//! part gets independently complicated (custom filter UIs per
//! provider, say), splitting it later is localised.
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

use crate::{Component, ComponentId, Outcome};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pilot_core::{SessionId, SessionKey, Workspace};
use pilot_v2_ipc::{Command, Event, TerminalKind};
use ratatui::Frame;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::collections::HashMap;

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

pub struct Sidebar {
    id: ComponentId,
    workspaces: HashMap<SessionKey, Workspace>,
    /// Derived view: workspaces filtered by mailbox, grouped by repo,
    /// each group sorted by updated_at desc. Headers are interleaved
    /// with workspace rows in render order; the cursor navigates
    /// only over workspace rows (headers are skipped).
    visible: Vec<VisibleRow>,
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
}

impl Sidebar {
    pub fn new(id: ComponentId) -> Self {
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
            cursor: 0,
            mailbox: Mailbox::Inbox,
            kill_pending: None,
            agent_shortcuts,
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
    fn move_cursor_by(&mut self, delta: isize) {
        if delta == 0 || self.visible.is_empty() {
            return;
        }
        let selectable: Vec<usize> = self
            .visible
            .iter()
            .enumerate()
            .filter_map(|(i, r)| match r {
                VisibleRow::Workspace(_) | VisibleRow::Session { .. } => Some(i),
                VisibleRow::RepoHeader(_) => None,
            })
            .collect();
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
        let mut current_repo: Option<String> = None;
        for (k, w) in &filtered {
            let repo = repo_of(w);
            if current_repo.as_deref() != Some(&repo) {
                visible.push(VisibleRow::RepoHeader(repo.clone()));
                current_repo = Some(repo);
            }
            visible.push(VisibleRow::Workspace((*k).clone()));
            // Only expand into per-session sub-rows when there are
            // 2+ sessions — single-session workspaces collapse to
            // just the workspace row (the user wants the visual
            // hierarchy to *earn* its way onto the screen).
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

impl Component for Sidebar {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn handle_key(&mut self, key: KeyEvent, cmds: &mut Vec<Command>) -> Outcome {
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
                Outcome::Consumed
            }
            (KeyCode::Char('k'), m) | (KeyCode::Up, m) if !m.contains(KeyModifiers::SHIFT) => {
                self.move_cursor_by(-1);
                Outcome::Consumed
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
                }
                Outcome::Consumed
            }
            // `s` for shell — used to be `b` (for "bash") but the
            // hint bar reads better as "S shell / C claude / X codex /
            // U cursor" all-lowercase, and `s` is mnemonic.
            (KeyCode::Char('s'), KeyModifiers::NONE) => {
                if let Some(session_key) = self.selected_session_key().cloned() {
                    cmds.push(Command::Spawn {
                        session_key,
                        session_id: self.selected_session_id(),
                        kind: TerminalKind::Shell,
                        cwd: None,
                    });
                }
                Outcome::Consumed
            }

            // ── Session actions ───────────────────────────────────────
            (KeyCode::Char('m'), KeyModifiers::NONE) => {
                if let Some(session_key) = self.selected_session_key().cloned() {
                    cmds.push(Command::MarkRead { session_key });
                }
                Outcome::Consumed
            }
            (KeyCode::Char('g'), KeyModifiers::NONE) => {
                cmds.push(Command::Refresh);
                Outcome::Consumed
            }
            (KeyCode::Char('z'), KeyModifiers::NONE) => {
                let Some(session_key) = self.selected_session_key().cloned() else {
                    return Outcome::Consumed;
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
                Outcome::Consumed
            }
            (KeyCode::Char('Z'), m) if m.contains(KeyModifiers::SHIFT) => {
                if let Some(session_key) = self.selected_session_key().cloned() {
                    let now = chrono::Utc::now();
                    cmds.push(Command::Snooze {
                        session_key,
                        until: now + chrono::Duration::days(365),
                    });
                }
                Outcome::Consumed
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
                Outcome::Consumed
            }

            // ── Kill session (two-press confirmation) ─────────────────
            (KeyCode::Char('X'), m) if m.contains(KeyModifiers::SHIFT) => {
                let Some(session_key) = self.selected_session_key().cloned() else {
                    return Outcome::Consumed;
                };
                if self.kill_pending.as_ref() == Some(&session_key) {
                    self.kill_pending = None;
                    cmds.push(Command::Kill { session_key });
                } else {
                    self.kill_pending = Some(session_key);
                }
                Outcome::Consumed
            }

            // ── Merge / approve / update-branch ───────────────────────
            (KeyCode::Char('M'), m) if m.contains(KeyModifiers::SHIFT) => {
                if let Some(session_key) = self.selected_session_key().cloned() {
                    cmds.push(Command::Merge { session_key });
                }
                Outcome::Consumed
            }
            (KeyCode::Char('V'), m) if m.contains(KeyModifiers::SHIFT) => {
                if let Some(session_key) = self.selected_session_key().cloned() {
                    cmds.push(Command::Approve { session_key });
                }
                Outcome::Consumed
            }
            (KeyCode::Char('U'), m) if m.contains(KeyModifiers::SHIFT) => {
                if let Some(session_key) = self.selected_session_key().cloned() {
                    cmds.push(Command::UpdateBranch { session_key });
                }
                Outcome::Consumed
            }

            // Anything else: bubble up. Tab / Help / `?` / overlays /
            // quit are handled by parent components.
            _ => Outcome::BubbleUp,
        }
    }

    fn on_event(&mut self, event: &Event) {
        match event {
            Event::Snapshot { workspaces, .. } => {
                self.workspaces.clear();
                for w in workspaces {
                    let key: SessionKey = (&w.key).into();
                    self.workspaces.insert(key, w.clone());
                }
                self.cursor = 0;
                self.recompute_visible();
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

    fn render(&mut self, area: Rect, frame: &mut Frame, focused: bool) {
        let border_color = if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        };
        let mailbox_label = match self.mailbox {
            Mailbox::Inbox => "INBOX",
            Mailbox::Inactive => "INACTIVE",
            Mailbox::Snoozed => "SNOOZED",
        };
        let title = format!(" {mailbox_label} ({}) ", self.workspace_count());
        let block = Block::bordered()
            .border_style(Style::default().fg(border_color))
            .title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines: Vec<Line> = self
            .visible
            .iter()
            .enumerate()
            .map(|(i, row)| match row {
                VisibleRow::RepoHeader(name) => Line::from(Span::styled(
                    name.clone(),
                    Style::default().fg(Color::Yellow).bold(),
                )),
                VisibleRow::Workspace(key) => {
                    let workspace = self.workspaces.get(key);
                    let title = workspace
                        .and_then(|w| w.primary_task().map(|t| t.title.as_str()))
                        .unwrap_or_else(|| workspace.map(|w| w.name.as_str()).unwrap_or("?"));
                    let is_cursor = i == self.cursor;
                    let style = if is_cursor && focused {
                        Style::default().bg(Color::DarkGray).fg(Color::White).bold()
                    } else if is_cursor {
                        Style::default().bg(Color::Reset).fg(Color::White)
                    } else {
                        Style::default()
                    };
                    // Two-space indent under the repo header, plus the
                    // selection caret. Visually:
                    //   owner/repo
                    //   ▸ Workspace title
                    //     Other workspace title
                    let prefix = if is_cursor { "  ▸ " } else { "    " };
                    let kill_mark = if self.kill_pending.as_ref() == Some(key) {
                        " [kill?]"
                    } else {
                        ""
                    };
                    Line::from(vec![
                        Span::styled(prefix, style),
                        Span::styled(title.to_string(), style),
                        Span::styled(kill_mark, Style::default().fg(Color::Red)),
                    ])
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
                        Style::default().bg(Color::DarkGray).fg(Color::White).bold()
                    } else if is_cursor {
                        Style::default().bg(Color::Reset).fg(Color::White)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    let prefix = if is_cursor { "      ▸ " } else { "        " };
                    Line::from(vec![
                        Span::styled(prefix, style),
                        Span::styled(name.to_string(), style),
                    ])
                }
            })
            .collect();

        let para = Paragraph::new(lines);
        frame.render_widget(para, inner);
    }
}
