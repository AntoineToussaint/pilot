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
//! - `sessions`: the authoritative map of SessionKey → Session. The
//!   daemon is the source of truth; we mirror what it sends via
//!   `Event::SessionUpserted` / `SessionRemoved` / `Snapshot`.
//! - `visible`: derived — `sessions` filtered by mailbox and sorted
//!   by `updated_at` descending. Recomputed on every change so the
//!   user never sees a stale order.
//! - `cursor`: index into `visible`. Preserved by key (not index)
//!   across refreshes — the same PR stays under the cursor even when
//!   another session gets inserted above it.
//! - `mailbox`: which view we're showing (Inbox vs Snoozed).
//! - `kill_pending`: two-press guard for `Shift-X`.

use crate::{Component, ComponentId, Outcome};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pilot_core::{Session, SessionKey};
use pilot_v2_ipc::{Command, Event, TerminalKind};
use ratatui::Frame;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::collections::HashMap;

/// Which logical mailbox the sidebar is currently showing.
/// Matches v1's `Mailbox` concept — Inbox is the default, Snoozed
/// holds archived / future-revisit sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mailbox {
    #[default]
    Inbox,
    Snoozed,
}

pub struct Sidebar {
    id: ComponentId,
    sessions: HashMap<SessionKey, Session>,
    /// Derived view: filtered by mailbox + sorted by updated_at desc.
    visible: Vec<SessionKey>,
    cursor: usize,
    mailbox: Mailbox,
    /// If set, `Shift-X` has been pressed once on this session. A
    /// second press executes the kill. Any other key clears.
    kill_pending: Option<SessionKey>,
    /// Per-key agent id map. Defaults to `c => "claude", C => "codex"`.
    /// AppRoot can override via `with_agent_shortcuts` for users with
    /// Aider / Cursor / custom CLIs configured.
    agent_shortcuts: HashMap<char, String>,
}

impl Sidebar {
    pub fn new(id: ComponentId) -> Self {
        let mut agent_shortcuts = HashMap::new();
        agent_shortcuts.insert('c', "claude".to_string());
        agent_shortcuts.insert('C', "codex".to_string());
        Self {
            id,
            sessions: HashMap::new(),
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
        self.visible.get(self.cursor)
    }

    pub fn selected_session(&self) -> Option<&Session> {
        self.selected_session_key().and_then(|k| self.sessions.get(k))
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

    pub fn kill_armed(&self) -> Option<&SessionKey> {
        self.kill_pending.as_ref()
    }

    fn recompute_visible(&mut self) {
        let now = chrono::Utc::now();
        let mailbox = self.mailbox;
        let mut list: Vec<(&SessionKey, &Session)> = self
            .sessions
            .iter()
            .filter(|(_, s)| match mailbox {
                Mailbox::Inbox => !s.is_snoozed(now),
                Mailbox::Snoozed => s.is_snoozed(now),
            })
            .filter(|(_, s)| {
                // Merged/closed never show up. The daemon SHOULD send
                // SessionRemoved for these, but we belt-and-suspenders.
                !matches!(
                    s.primary_task.state,
                    pilot_core::TaskState::Merged | pilot_core::TaskState::Closed
                )
            })
            .collect();
        list.sort_by(|(_, a), (_, b)| {
            b.primary_task
                .updated_at
                .cmp(&a.primary_task.updated_at)
                .then_with(|| a.task_id.to_string().cmp(&b.task_id.to_string()))
        });

        let prior_key = self.visible.get(self.cursor).cloned();
        self.visible = list.iter().map(|(k, _)| (*k).clone()).collect();

        // Preserve cursor-on-key across reorderings.
        if let Some(key) = prior_key
            && let Some(pos) = self.visible.iter().position(|k| *k == key)
        {
            self.cursor = pos;
            return;
        }
        // Prior selection vanished or didn't exist. Clamp.
        if self.visible.is_empty() {
            self.cursor = 0;
        } else {
            self.cursor = self.cursor.min(self.visible.len() - 1);
        }
    }
}

impl Component for Sidebar {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn handle_key(&mut self, key: KeyEvent, cmds: &mut Vec<Command>) -> Outcome {
        // Any key other than Shift-X disarms the kill confirmation.
        let is_shift_x = key.code == KeyCode::Char('X')
            && key.modifiers.contains(KeyModifiers::SHIFT);
        if self.kill_pending.is_some() && !is_shift_x {
            self.kill_pending = None;
        }

        match (key.code, key.modifiers) {
            // ── Navigation ────────────────────────────────────────────
            (KeyCode::Char('j'), m) | (KeyCode::Down, m) if !m.contains(KeyModifiers::SHIFT) => {
                if !self.visible.is_empty() && self.cursor + 1 < self.visible.len() {
                    self.cursor += 1;
                }
                Outcome::Consumed
            }
            (KeyCode::Char('k'), m) | (KeyCode::Up, m) if !m.contains(KeyModifiers::SHIFT) => {
                self.cursor = self.cursor.saturating_sub(1);
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
                        kind: TerminalKind::Agent(agent_id),
                        cwd: None,
                    });
                }
                Outcome::Consumed
            }
            (KeyCode::Char('b'), KeyModifiers::NONE) => {
                if let Some(session_key) = self.selected_session_key().cloned() {
                    cmds.push(Command::Spawn {
                        session_key,
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
                    .sessions
                    .get(&session_key)
                    .map(|s| s.is_snoozed(now))
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

            // ── Mailbox toggle ────────────────────────────────────────
            (KeyCode::Char('S'), m) if m.contains(KeyModifiers::SHIFT) => {
                self.mailbox = match self.mailbox {
                    Mailbox::Inbox => Mailbox::Snoozed,
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
            Event::Snapshot { sessions, .. } => {
                self.sessions.clear();
                for s in sessions {
                    let key: SessionKey = (&s.task_id).into();
                    self.sessions.insert(key, s.clone());
                }
                self.cursor = 0;
                self.recompute_visible();
            }
            Event::SessionUpserted(session) => {
                let key: SessionKey = (&session.task_id).into();
                self.sessions.insert(key, session.clone());
                self.recompute_visible();
            }
            Event::SessionRemoved(key) => {
                self.sessions.remove(key);
                self.recompute_visible();
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
            Mailbox::Snoozed => "SNOOZED",
        };
        let title = format!(" {mailbox_label} ({}) ", self.visible.len());
        let block = Block::bordered()
            .border_style(Style::default().fg(border_color))
            .title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines: Vec<Line> = self
            .visible
            .iter()
            .enumerate()
            .map(|(i, key)| {
                let session = self.sessions.get(key);
                let title = session.map(|s| s.display_name.as_str()).unwrap_or("?");
                let is_cursor = i == self.cursor;
                let style = if is_cursor && focused {
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold()
                } else if is_cursor {
                    Style::default().bg(Color::Reset).fg(Color::White)
                } else {
                    Style::default()
                };
                let prefix = if is_cursor { "▸ " } else { "  " };
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
            })
            .collect();

        let para = Paragraph::new(lines);
        frame.render_widget(para, inner);
    }
}
