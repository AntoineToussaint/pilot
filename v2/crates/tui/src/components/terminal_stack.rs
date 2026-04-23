//! TerminalStack — multi-terminal right-pane surface per session.
//!
//! Each session can have several terminals open simultaneously: the
//! agent (Claude / Codex), a shell, a log tail. This component owns
//! the local mirror of what the daemon streams for each one and
//! handles the tab bar + active-terminal key routing.
//!
//! ## MVP rendering
//!
//! The display shows ANSI-stripped plain text. This lets us ship the
//! key/event plumbing fully tested while deferring the libghostty-vt
//! integration to v2.1 (task #78). Behavior is the same either way —
//! only the bytes-to-pixels layer changes.
//!
//! ## Key routing
//!
//! When the TerminalStack is focused and a live terminal is active:
//! - `Ctrl-]` / `Ctrl-o` bubble up (exit terminal mode in the parent).
//! - `Tab` moves focus to the next sibling via `Outcome::FocusNext`.
//! - Everything else emits `Command::Write` to the active terminal.
//!
//! Without focus, all keys bubble up so the sidebar / overlays can
//! pick them up.

use crate::{Component, ComponentId, Outcome};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pilot_core::SessionKey;
use pilot_v2_ipc::{Command, Event, TerminalId, TerminalKind};
use ratatui::Frame;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::collections::HashMap;

/// Cap per-terminal stored bytes so long-running terminals don't
/// balloon the TUI's memory. Older bytes are dropped; the full
/// history stays on the daemon, which can replay it on reconnect.
pub const MAX_PER_TERM_BYTES: usize = 16 * 1024;

pub struct TerminalStack {
    id: ComponentId,
    terminals: HashMap<TerminalId, TerminalSlot>,
    /// Which session's terminals are currently visible. `None` =>
    /// render an empty-state message.
    active_session: Option<SessionKey>,
    /// Index into `visible_terminals()`. Clamped on every tab change /
    /// visible-set mutation so it can never point out of range.
    active_tab_idx: usize,
}

struct TerminalSlot {
    session_key: SessionKey,
    kind: TerminalKind,
    /// ANSI-stripped payload. Capped at `MAX_PER_TERM_BYTES`.
    content: Vec<u8>,
    alive: bool,
    last_seq: u64,
}

impl TerminalStack {
    pub fn new(id: ComponentId) -> Self {
        Self {
            id,
            terminals: HashMap::new(),
            active_session: None,
            active_tab_idx: 0,
        }
    }

    /// AppRoot calls this whenever the sidebar selection changes.
    /// Also resets the active tab to 0 so switching sessions doesn't
    /// dump the user on a tab index that happens to still be valid
    /// but represents a totally different terminal.
    pub fn set_active_session(&mut self, session: Option<SessionKey>) {
        if self.active_session != session {
            self.active_tab_idx = 0;
        }
        self.active_session = session;
    }

    pub fn active_session(&self) -> Option<&SessionKey> {
        self.active_session.as_ref()
    }

    /// TerminalIds visible in the current session, in stable order
    /// (by u64 id so tab positions are deterministic).
    pub fn visible_terminals(&self) -> Vec<TerminalId> {
        let Some(sk) = &self.active_session else {
            return vec![];
        };
        let mut ids: Vec<TerminalId> = self
            .terminals
            .iter()
            .filter(|(_, slot)| slot.session_key == *sk)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_by_key(|id| id.0);
        ids
    }

    pub fn active_terminal_id(&self) -> Option<TerminalId> {
        self.visible_terminals().get(self.active_tab_idx).copied()
    }

    pub fn active_tab_idx(&self) -> usize {
        self.active_tab_idx
    }

    pub fn terminal_count(&self) -> usize {
        self.terminals.len()
    }

    /// Content of the active terminal (ANSI-stripped). For tests /
    /// debugging.
    pub fn active_content(&self) -> Option<&[u8]> {
        let id = self.active_terminal_id()?;
        self.terminals.get(&id).map(|s| s.content.as_slice())
    }

    pub fn cycle_tab_forward(&mut self) {
        let n = self.visible_terminals().len();
        if n == 0 {
            self.active_tab_idx = 0;
            return;
        }
        self.active_tab_idx = (self.active_tab_idx + 1) % n;
    }

    pub fn cycle_tab_backward(&mut self) {
        let n = self.visible_terminals().len();
        if n == 0 {
            self.active_tab_idx = 0;
            return;
        }
        self.active_tab_idx = if self.active_tab_idx == 0 {
            n - 1
        } else {
            self.active_tab_idx - 1
        };
    }

    fn clamp_active_tab(&mut self) {
        let n = self.visible_terminals().len();
        if n == 0 {
            self.active_tab_idx = 0;
        } else if self.active_tab_idx >= n {
            self.active_tab_idx = n - 1;
        }
    }

    fn append_output(&mut self, id: TerminalId, bytes: &[u8], seq: u64) {
        let Some(slot) = self.terminals.get_mut(&id) else {
            return;
        };
        let stripped = strip_ansi(bytes);
        slot.content.extend_from_slice(&stripped);
        if slot.content.len() > MAX_PER_TERM_BYTES {
            let excess = slot.content.len() - MAX_PER_TERM_BYTES;
            slot.content.drain(..excess);
        }
        slot.last_seq = seq;
    }

    fn tab_label(kind: &TerminalKind) -> String {
        match kind {
            TerminalKind::Agent(name) => name.clone(),
            TerminalKind::Shell => "shell".into(),
            TerminalKind::LogTail { path } => {
                // Short label: last path segment.
                path.rsplit('/').next().unwrap_or(path).to_string()
            }
        }
    }
}

impl Component for TerminalStack {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn handle_key(&mut self, key: KeyEvent, cmds: &mut Vec<Command>) -> Outcome {
        // Terminal escape: bubble up for parent-level handling.
        let is_escape = matches!(
            (key.code, key.modifiers),
            (KeyCode::Char(']'), m) | (KeyCode::Char('o'), m)
                if m.contains(KeyModifiers::CONTROL)
        );
        if is_escape {
            return Outcome::BubbleUp;
        }
        // Tab cycles pane focus (not tab in the terminal).
        if key.code == KeyCode::Tab && key.modifiers == KeyModifiers::NONE {
            return Outcome::FocusNext;
        }

        let Some(id) = self.active_terminal_id() else {
            // No terminal to route to — let the parent handle.
            return Outcome::BubbleUp;
        };
        let Some(slot) = self.terminals.get(&id) else {
            return Outcome::BubbleUp;
        };
        if !slot.alive {
            // Dead terminal — don't write, let parent handle.
            return Outcome::BubbleUp;
        }

        let Some(bytes) = key_to_bytes(&key) else {
            return Outcome::Consumed;
        };
        cmds.push(Command::Write {
            terminal_id: id,
            bytes,
        });
        Outcome::Consumed
    }

    fn on_event(&mut self, event: &Event) {
        match event {
            Event::Snapshot { terminals, .. } => {
                self.terminals.clear();
                for snap in terminals {
                    self.terminals.insert(
                        snap.terminal_id,
                        TerminalSlot {
                            session_key: snap.session_key.clone(),
                            kind: snap.kind.clone(),
                            content: strip_ansi(&snap.replay),
                            alive: true,
                            last_seq: snap.last_seq,
                        },
                    );
                }
                self.clamp_active_tab();
            }
            Event::TerminalSpawned {
                terminal_id,
                session_key,
                kind,
            } => {
                self.terminals.insert(
                    *terminal_id,
                    TerminalSlot {
                        session_key: session_key.clone(),
                        kind: kind.clone(),
                        content: Vec::new(),
                        alive: true,
                        last_seq: 0,
                    },
                );
            }
            Event::TerminalOutput {
                terminal_id,
                bytes,
                seq,
            } => {
                self.append_output(*terminal_id, bytes, *seq);
            }
            Event::TerminalExited { terminal_id, .. } => {
                if let Some(slot) = self.terminals.get_mut(terminal_id) {
                    slot.alive = false;
                }
            }
            Event::SessionRemoved(session_key) => {
                self.terminals.retain(|_, slot| slot.session_key != *session_key);
                self.clamp_active_tab();
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
        let block = Block::bordered()
            .title(" Terminals ")
            .border_style(Style::default().fg(border_color));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let visible = self.visible_terminals();
        if visible.is_empty() {
            let empty = Paragraph::new(Line::from(Span::styled(
                "  (no terminals — press `c` on a session)",
                Style::default().fg(Color::DarkGray).italic(),
            )));
            frame.render_widget(empty, inner);
            return;
        }

        // Row 0: tab bar; row 1..: active terminal content.
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

        let tab_bar: Vec<Span> = visible
            .iter()
            .enumerate()
            .flat_map(|(i, id)| {
                let slot = self.terminals.get(id);
                let (label, alive) = slot
                    .map(|s| (Self::tab_label(&s.kind), s.alive))
                    .unwrap_or_else(|| ("?".into(), false));
                let is_active = i == self.active_tab_idx;
                let fg = if !alive {
                    Color::DarkGray
                } else if is_active && focused {
                    Color::Black
                } else {
                    Color::White
                };
                let bg = if is_active && focused {
                    Color::Cyan
                } else {
                    Color::Reset
                };
                vec![
                    Span::styled(
                        format!(" {label} "),
                        Style::default().fg(fg).bg(bg).bold(),
                    ),
                    Span::raw(" "),
                ]
            })
            .collect();
        frame.render_widget(Paragraph::new(Line::from(tab_bar)), chunks[0]);

        // Content of the active terminal. Show the last `h` lines
        // where h = inner content height.
        if let Some(id) = self.active_terminal_id()
            && let Some(slot) = self.terminals.get(&id)
        {
            let h = chunks[1].height as usize;
            let content = String::from_utf8_lossy(&slot.content);
            let lines: Vec<Line> = content
                .lines()
                .rev()
                .take(h)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|l| Line::from(Span::raw(l.to_string())))
                .collect();
            frame.render_widget(Paragraph::new(lines), chunks[1]);
        }
    }
}

// ── ANSI strip helper ──────────────────────────────────────────────────

/// Strip ANSI escape sequences from a byte buffer, returning plain
/// text bytes. Handles CSI (`ESC [ ... <final>`), OSC (`ESC ] ... BEL`
/// or `ESC \`), and generic ESC-char sequences. Leaves printable bytes
/// (including UTF-8 multi-byte) alone.
///
/// This is a pragmatic MVP parser — it's not a full VT spec compliance
/// layer. Real terminal rendering via libghostty-vt replaces this
/// entirely in task #78.
pub fn strip_ansi(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == 0x1b {
            // ESC introducer.
            if i + 1 >= input.len() {
                break;
            }
            match input[i + 1] {
                b'[' => {
                    // CSI: skip through the final byte (0x40..=0x7E).
                    i += 2;
                    while i < input.len() {
                        let c = input[i];
                        if (0x40..=0x7e).contains(&c) {
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                }
                b']' => {
                    // OSC: terminates on BEL (0x07) or ST (ESC \).
                    i += 2;
                    while i < input.len() {
                        if input[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if input[i] == 0x1b
                            && i + 1 < input.len()
                            && input[i + 1] == b'\\'
                        {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                _ => {
                    // Two-byte ESC sequences (ESC c, ESC (B, etc.).
                    i += 2;
                }
            }
            continue;
        }
        // Drop the single BEL byte — it's not printable.
        if input[i] == 0x07 {
            i += 1;
            continue;
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

// ── Key → PTY bytes ────────────────────────────────────────────────────

/// Encode a key event as the bytes we'd write to a PTY. Mirrors the
/// v1 table (`crates/app/src/keys.rs`) for the common cases. Returns
/// None for keys we don't know how to encode yet.
fn key_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    use KeyCode::*;
    match key.code {
        Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl-<letter>: low control byte.
            Some(vec![(c as u8) & 0x1f])
        }
        Char(c) => {
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        Enter => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                // Shift-Enter → ESC + CR. Claude Code accepts this as
                // "newline in prompt without submit".
                Some(vec![0x1b, b'\r'])
            } else {
                Some(vec![b'\r'])
            }
        }
        Backspace => Some(vec![0x7f]),
        Esc => Some(vec![0x1b]),
        Tab => Some(vec![b'\t']),
        BackTab => Some(b"\x1b[Z".to_vec()),
        Up => Some(b"\x1b[A".to_vec()),
        Down => Some(b"\x1b[B".to_vec()),
        Right => Some(b"\x1b[C".to_vec()),
        Left => Some(b"\x1b[D".to_vec()),
        Home => Some(b"\x1b[H".to_vec()),
        End => Some(b"\x1b[F".to_vec()),
        Delete => Some(b"\x1b[3~".to_vec()),
        _ => None,
    }
}
