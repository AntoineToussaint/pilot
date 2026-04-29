//! TerminalStack — multi-terminal right-pane surface per session.
//!
//! Each session can have several terminals open simultaneously: the
//! agent (Claude / Codex / Cursor), a shell, a log tail. This
//! component owns the per-terminal libghostty-vt parser state, feeds
//! it the bytes the daemon streams, and renders the resulting cell
//! grid via `pilot_tui_term::GhosttyTerminal`.
//!
//! ## Why per-client emulation
//!
//! In v2 the daemon owns the PTY but the TUI owns the renderer. The
//! daemon broadcasts raw bytes (`Event::TerminalOutput`) so a remote
//! TUI over SSH gets exactly what a local one does — the wire format
//! is "what the agent printed", not "an already-rendered cell grid".
//! Each client runs its own libghostty-vt instance and computes its
//! own viewport. Resizing is per-client (the daemon has its own size,
//! used only to size the underlying PTY).
//!
//! ## Key routing
//!
//! When the TerminalStack is focused and a live terminal is active:
//! - `Ctrl-]` / `Ctrl-o` bubble up (exit terminal mode).
//! - `Tab` moves focus to the next sibling via `Outcome::FocusNext`.
//! - Everything else emits `Command::Write` to the active terminal.
//!
//! Without focus, all keys bubble up so the sidebar / overlays pick
//! them up first.

use crate::{Component, ComponentId, Outcome};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use libghostty_vt as vt;
use pilot_core::SessionKey;
use pilot_tui_term::GhosttyTerminal;
use pilot_v2_ipc::{Command, Event, TerminalId, TerminalKind};
use ratatui::Frame;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::collections::HashMap;

/// Default cell grid size for new terminals before the first
/// resize-from-render. Sized to match a typical agent default; the
/// renderer overrides as soon as it knows the actual viewport.
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 32;

/// Cap for the per-terminal recent-output buffer.
///
/// libghostty-vt holds the canonical cell grid for rendering, but
/// agent-state detection (Claude's "Are you sure?" prompts, error
/// markers, etc.) needs to pattern-match raw bytes — re-extracting
/// them from the cell grid loses the escape sequences. So we keep a
/// rolling window of the last ~4 KiB of bytes the daemon streamed in.
/// 4 KiB matches the v1 `TermSession::recent_output` buffer and is
/// enough to span any prompt the agents have shipped so far.
pub const RECENT_OUTPUT_CAP: usize = 4 * 1024;

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
    last_seq: u64,
    /// libghostty-vt parser. Each client owns its own — the daemon
    /// streams raw bytes; this is what turns them into a cell grid.
    /// `Box`ed so moving `TerminalSlot` doesn't move the inner FFI
    /// allocator pointers (they self-reference).
    vt: Box<TerminalVt>,
    /// Cap of recent raw bytes (post-feed). Pure debug aid; tests
    /// inspect it. Not used for rendering.
    recent: Vec<u8>,
}

/// libghostty-vt state for one terminal.
///
/// **`!Send + !Sync`** because libghostty's allocator owns raw
/// pointers. Lives entirely on the main task — the TUI is single-
/// threaded by design (the daemon is what's multi-threaded).
struct TerminalVt {
    terminal: vt::Terminal<'static, 'static>,
    render_state: vt::RenderState<'static>,
    row_iter: vt::render::RowIterator<'static>,
    cell_iter: vt::render::CellIterator<'static>,
    cols: u16,
    rows: u16,
    _not_send: std::marker::PhantomData<*mut ()>,
}

impl TerminalVt {
    fn new() -> Option<Box<Self>> {
        let terminal = vt::Terminal::new(vt::TerminalOptions {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            max_scrollback: 10_000,
        })
        .ok()?;
        let render_state = vt::RenderState::new().ok()?;
        let row_iter = vt::render::RowIterator::new().ok()?;
        let cell_iter = vt::render::CellIterator::new().ok()?;
        Some(Box::new(Self {
            terminal,
            render_state,
            row_iter,
            cell_iter,
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            _not_send: std::marker::PhantomData,
        }))
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.terminal.vt_write(bytes);
    }

    fn ensure_size(&mut self, cols: u16, rows: u16) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        let _ = self.terminal.resize(cols, rows, 0, 0);
        self.cols = cols;
        self.rows = rows;
    }
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

    /// Recent raw output for the active terminal. Used for tests +
    /// pattern-matching (e.g. detecting "Are you sure?" prompts).
    /// NOT the rendering source — that's libghostty-vt. Includes
    /// escape sequences as they came off the wire.
    pub fn active_content(&self) -> Option<&[u8]> {
        let id = self.active_terminal_id()?;
        self.terminals.get(&id).map(|s| s.recent.as_slice())
    }

    /// Scroll the active terminal's viewport by `delta` rows. Negative
    /// scrolls up into the scrollback; positive scrolls down toward
    /// the live content. Called from the app loop's mouse-wheel
    /// handler so trackpad gestures move the viewport instead of just
    /// being eaten.
    pub fn scroll_active(&mut self, delta: isize) {
        if delta == 0 {
            return;
        }
        let Some(id) = self.active_terminal_id() else {
            return;
        };
        let Some(slot) = self.terminals.get_mut(&id) else {
            return;
        };
        slot.vt
            .terminal
            .scroll_viewport(vt::terminal::ScrollViewport::Delta(delta));
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
        slot.vt.feed(bytes);
        slot.recent.extend_from_slice(bytes);
        if slot.recent.len() > RECENT_OUTPUT_CAP {
            let excess = slot.recent.len() - RECENT_OUTPUT_CAP;
            slot.recent.drain(..excess);
        }
        slot.last_seq = seq;
    }

    fn make_slot(session_key: SessionKey, kind: TerminalKind, last_seq: u64) -> TerminalSlot {
        let vt = TerminalVt::new().expect("libghostty-vt init");
        TerminalSlot {
            session_key,
            kind,
            last_seq,
            vt,
            recent: Vec::new(),
        }
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
        // Escape sequence is owned by the app-level dispatcher (see
        // `dispatch_key` in app.rs). It uses a double-Esc latch on
        // `AppState` because that state needs to persist across calls.
        // Here we just route bytes to the active terminal; everything
        // — q, Tab, Ctrl-C, single Esc — is the agent's.
        let Some(id) = self.active_terminal_id() else {
            // No terminal to route to — let the parent handle.
            return Outcome::BubbleUp;
        };
        let Some(_slot) = self.terminals.get(&id) else {
            return Outcome::BubbleUp;
        };

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
                    let mut slot =
                        Self::make_slot(snap.session_key.clone(), snap.kind.clone(), snap.last_seq);
                    // Replay the daemon-side ring through the VT so
                    // the cell grid reflects what was on screen
                    // before this client connected.
                    slot.vt.feed(&snap.replay);
                    self.terminals.insert(snap.terminal_id, slot);
                }
                self.clamp_active_tab();
            }
            Event::TerminalSpawned {
                terminal_id,
                session_key,
                kind,
            } => {
                let slot = Self::make_slot(session_key.clone(), kind.clone(), 0);
                self.terminals.insert(*terminal_id, slot);
            }
            Event::TerminalOutput {
                terminal_id,
                bytes,
                seq,
            } => {
                self.append_output(*terminal_id, bytes, *seq);
            }
            Event::TerminalExited { terminal_id, .. } => {
                // Process exited (`exit`, ^D, segfault, kill from
                // outside) — close the window. Mirrors how every other
                // terminal emulator behaves: the prompt goes away, the
                // pane goes with it. Auto-spawn won't re-fire because
                // it's gated on first selection of the session.
                self.terminals.remove(terminal_id);
                self.clamp_active_tab();
            }
            Event::WorkspaceRemoved(workspace_key) => {
                // Drop every terminal that belonged to the removed
                // workspace. Wire-side the slot's session_key carries
                // the workspace's key string, so a literal compare
                // is enough.
                let key_str = workspace_key.as_str();
                self.terminals
                    .retain(|_, slot| slot.session_key.as_str() != key_str);
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

        // With a single terminal there's no choice to make — the tab
        // strip is just label noise that steals a row from the
        // viewport. Hide it; the terminal's own prompt makes it
        // obvious what's running. Show the strip only when there are
        // multiple tabs to switch between.
        let show_tabs = visible.len() > 1;
        let chunks = if show_tabs {
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner)
        } else {
            Layout::vertical([Constraint::Length(0), Constraint::Min(0)]).split(inner)
        };

        if show_tabs {
            let tab_bar: Vec<Span> = visible
                .iter()
                .enumerate()
                .flat_map(|(i, id)| {
                    let label = self
                        .terminals
                        .get(id)
                        .map(|s| Self::tab_label(&s.kind))
                        .unwrap_or_else(|| "?".into());
                    let is_active = i == self.active_tab_idx;
                    let fg = if is_active && focused {
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
                        Span::styled(format!(" {label} "), Style::default().fg(fg).bg(bg).bold()),
                        Span::raw(" "),
                    ]
                })
                .collect();
            frame.render_widget(Paragraph::new(Line::from(tab_bar)), chunks[0]);
        }

        // Content of the active terminal — rendered through
        // libghostty-vt so colors / cursor / clears / cursor moves
        // all paint correctly.
        if let Some(id) = self.active_terminal_id()
            && let Some(slot) = self.terminals.get_mut(&id)
        {
            // Resize the VT grid to match the area actually being
            // rendered into. This is what tells the agent how wide
            // its viewport is (libghostty echoes resize back as
            // SIGWINCH-equivalent on the next read; the daemon also
            // resizes the PTY via Command::Resize).
            slot.vt.ensure_size(chunks[1].width, chunks[1].height);
            // Build a snapshot from the current terminal state and
            // hand it to the GhosttyTerminal widget, which paints
            // each cell with full color + style.
            if let Ok(snapshot) = slot.vt.render_state.update(&slot.vt.terminal) {
                let widget =
                    GhosttyTerminal::new(&snapshot, &mut slot.vt.row_iter, &mut slot.vt.cell_iter);
                frame.render_widget(widget, chunks[1]);
            }
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
                        if input[i] == 0x1b && i + 1 < input.len() && input[i + 1] == b'\\' {
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
/// None for keys we don't know how to encode yet. Public so the
/// app-level escape-latch can flush buffered keystrokes through the
/// same encoding path the live key dispatch uses.
pub fn key_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
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
