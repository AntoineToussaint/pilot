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
//! The daemon owns the PTY but the TUI owns the renderer. The daemon
//! broadcasts raw bytes (`Event::TerminalOutput`) so a remote TUI over
//! SSH gets exactly what a local one does — the wire format is "what
//! the agent printed", not "an already-rendered cell grid". Each
//! client runs its own libghostty-vt instance and computes its own
//! viewport. Resizing is per-client (the daemon has its own size,
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

use crate::{PaneId, PaneOutcome};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use libghostty_vt as vt;
use pilot_core::SessionKey;
use pilot_tui_term::GhosttyTerminal;
use pilot_ipc::{Command, Event, TerminalId, TerminalKind};
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
/// 4 KiB is enough to span any prompt the agents have shipped so far.
pub const RECENT_OUTPUT_CAP: usize = 4 * 1024;

pub struct TerminalStack {
    id: PaneId,
    terminals: HashMap<TerminalId, TerminalSlot>,
    /// Which session's terminals are currently visible. `None` =>
    /// render an empty-state message.
    active_session: Option<SessionKey>,
    /// Index into `visible_terminals()`. Clamped on every tab change /
    /// visible-set mutation so it can never point out of range.
    active_tab_idx: usize,
    /// Whether the body is collapsed to its header row. The app's
    /// `build_layout` reads this to give the pane a 1-row slot
    /// instead of its share of the right column. Default: collapsed
    /// when there are no terminals (we show the empty hint inline in
    /// the header rather than wasting the bottom 75% of the screen).
    collapsed: bool,
    /// Once the user explicitly toggles, stop auto-collapsing on
    /// emptiness. Same dance as `RightPane::activity_collapse_user_set`.
    collapse_user_set: bool,
    /// Tile/tab arrangement for the active session. Defaults to
    /// `Tabs` so the legacy single-runner-full-pane UX keeps working
    /// when no split has ever been requested. Mutating this triggers
    /// a `Command::SetSessionLayout` so the daemon persists.
    layout: pilot_core::SessionLayout,
    /// Currently armed `Ctrl-w` prefix? When true, the next keystroke
    /// is interpreted as a tile-management action instead of being
    /// forwarded to the active PTY.
    ctrl_w_armed: bool,
    /// Pending split operation: when the user hits `Ctrl-w |` we
    /// emit `Command::Spawn` for a new shell, then once the
    /// `TerminalSpawned` event arrives we wrap the focused leaf in a
    /// fresh split with the new terminal. `Some(direction)` means
    /// "the next spawn becomes the new sibling on this axis".
    pending_split: Option<PendingSplit>,
    /// Resizes recorded during render and waiting to be drained by
    /// the App loop. Each entry is `(terminal_id, cols, rows)` — the
    /// App turns them into `Command::Resize` and ships them at the
    /// next loop tick. Drained on every `drain_pending_resizes`.
    pending_resizes: Vec<(TerminalId, u16, u16)>,
}

/// Direction of a pending split. `Vertical` = `|` = side-by-side =
/// `HSplit`. `Horizontal` = `-` = stacked = `VSplit`. (Vim
/// vocabulary, which is what most users will type.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingSplit {
    Vertical,
    Horizontal,
}

/// Read-only walk of a `TileTree` along a path. Returns None if the
/// path tries to descend through a leaf.
fn subtree_at_path<'a>(
    root: &'a pilot_core::TileTree,
    path: &[u8],
) -> Option<&'a pilot_core::TileTree> {
    let mut node = root;
    for &step in path {
        node = match node {
            pilot_core::TileTree::HSplit { left, right, .. }
            | pilot_core::TileTree::VSplit {
                top: left,
                bottom: right,
                ..
            } => {
                if step == 0 {
                    left.as_ref()
                } else {
                    right.as_ref()
                }
            }
            pilot_core::TileTree::Leaf { .. } => return None,
        };
    }
    Some(node)
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
    /// Agent state cached from the daemon's `Event::AgentState`
    /// broadcasts. Drives the "needs input" badge in the tab strip.
    /// Default Active so non-agent terminals (shells) carry a
    /// neutral state.
    agent_state: pilot_ipc::AgentState,
    /// Last (cols, rows) we rendered this terminal at. Used to detect
    /// pane resizes — when the rect changes between frames we push a
    /// `Command::Resize` so the backend PTY sees the new size and the
    /// shell process resizes its own view (otherwise output beyond
    /// the original spawn size never gets written and the user sees
    /// a frozen-looking pane).
    last_rendered_size: Option<(u16, u16)>,
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
    pub fn new(id: PaneId) -> Self {
        Self {
            id,
            terminals: HashMap::new(),
            active_session: None,
            active_tab_idx: 0,
            collapsed: true,
            collapse_user_set: false,
            layout: pilot_core::SessionLayout::default(),
            ctrl_w_armed: false,
            pending_split: None,
            pending_resizes: Vec::new(),
        }
    }

    /// Drain queued resize requests from the last frame. The App calls
    /// this after every render and ships each as a `Command::Resize`
    /// so the backend PTY's size tracks the visible rect — without
    /// this, the shell process inside the PTY stays at its initial
    /// spawn size and output past those rows never gets written,
    /// surfacing as "the terminal looks frozen."
    pub fn drain_pending_resizes(&mut self) -> Vec<(TerminalId, u16, u16)> {
        std::mem::take(&mut self.pending_resizes)
    }

    /// Apply a session's persisted layout. Called by the App when the
    /// active workspace + session change so the renderer matches the
    /// user's last arrangement.
    pub fn set_layout(&mut self, layout: pilot_core::SessionLayout) {
        self.layout = layout;
        self.ctrl_w_armed = false;
        self.pending_split = None;
    }

    pub fn layout(&self) -> &pilot_core::SessionLayout {
        &self.layout
    }

    /// Terminal id at the focused leaf (Splits mode), or the active
    /// tab's terminal id (Tabs mode). Returns None when nothing is
    /// renderable.
    pub fn focused_terminal_id(&self) -> Option<TerminalId> {
        match &self.layout {
            pilot_core::SessionLayout::Tabs { .. } => self.active_terminal_id(),
            pilot_core::SessionLayout::Splits { tree, focused } => {
                let leaves = tree.leaves();
                let path = focused.as_slice();
                let id = subtree_at_path(tree, path).and_then(|n| match n {
                    pilot_core::TileTree::Leaf { terminal_id } => Some(*terminal_id),
                    _ => None,
                });
                id.map(TerminalId).or_else(|| leaves.first().map(|i| TerminalId(*i)))
            }
        }
    }

    /// Whether the pane should render only its header row.
    pub fn is_collapsed(&self) -> bool {
        self.collapsed
    }

    /// Toggle the collapse state. Marks the user-override flag so we
    /// stop auto-collapsing on emptiness.
    pub fn set_collapsed(&mut self, collapsed: bool) {
        self.collapsed = collapsed;
        self.collapse_user_set = true;
    }

    /// Re-apply the empty-aware default unless the user has already
    /// expressed a preference. Called from event handlers that change
    /// the visible terminal set (Snapshot, TerminalSpawned,
    /// TerminalExited, set_active_session).
    fn auto_collapse_on_emptiness(&mut self) {
        if self.collapse_user_set {
            return;
        }
        self.collapsed = self.visible_terminals().is_empty();
    }

    /// AppRoot calls this whenever the sidebar selection changes.
    /// Also resets the active tab to 0 so switching sessions doesn't
    /// dump the user on a tab index that happens to still be valid
    /// but represents a totally different terminal.
    pub fn set_active_session(&mut self, session: Option<SessionKey>) {
        let changed = self.active_session != session;
        if changed {
            self.active_tab_idx = 0;
            // Drop the user's explicit collapse override on session
            // change — each session gets its own auto-default.
            self.collapse_user_set = false;
        }
        self.active_session = session;
        if changed {
            self.auto_collapse_on_emptiness();
        }
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

    /// Find an existing runner inside the given session whose kind
    /// has the same singleton-identity as `kind` (e.g. "this session
    /// already has a Claude → don't spawn a second one"). Returns
    /// `None` for non-singleton kinds (Shell) since those are
    /// always new spawns.
    pub fn find_runner(
        &self,
        session_key: &SessionKey,
        kind: &TerminalKind,
    ) -> Option<TerminalId> {
        let key = kind.singleton_key()?;
        self.terminals
            .iter()
            .find(|(_, slot)| {
                slot.session_key == *session_key
                    && slot.kind.singleton_key() == Some(key.clone())
            })
            .map(|(id, _)| *id)
    }

    /// Switch the active tab to the given terminal (must belong to
    /// the active session, otherwise no-op). Used by the singleton
    /// toggle-or-focus path: the user pressed `c`, we already have
    /// a Claude in this session, just bring it forward.
    pub fn focus_terminal(&mut self, target: TerminalId) -> bool {
        let visible = self.visible_terminals();
        if let Some(idx) = visible.iter().position(|id| *id == target) {
            self.active_tab_idx = idx;
            // Expanding the section is part of "focus": collapsed
            // body would otherwise hide the tab the user just asked
            // for.
            self.set_collapsed(false);
            true
        } else {
            false
        }
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

    /// True when the focused terminal's inner program (libghostty
    /// has parsed CSI ?1000h / ?1002h / ?1003h / ?1006h SGR) wants
    /// raw mouse events forwarded. Claude Code, vim, less, etc. all
    /// turn this on while running. The orchestrator's mouse handler
    /// uses this signal to choose between "scroll the scrollback"
    /// and "encode + send to PTY".
    pub fn focused_terminal_tracks_mouse(&self) -> bool {
        let Some(id) = self.focused_terminal_id() else {
            return false;
        };
        self.terminals
            .get(&id)
            .and_then(|s| s.vt.terminal.is_mouse_tracking().ok())
            .unwrap_or(false)
    }

    /// Encode a mouse event for the focused terminal using its
    /// active mouse-tracking mode + format. Returns the bytes to
    /// `Write` to the PTY plus the terminal id. Returns `None` when
    /// the terminal isn't tracking mouse, encoding failed, or the
    /// event doesn't translate to anything (no-op for the protocol).
    /// `cell_col` / `cell_row` are 0-based cell coordinates **within
    /// the terminal's rect**, not the screen.
    pub fn encode_mouse_for_focused(
        &mut self,
        action: vt::mouse::Action,
        button: Option<vt::mouse::Button>,
        cell_col: u32,
        cell_row: u32,
    ) -> Option<(TerminalId, Vec<u8>)> {
        let id = self.focused_terminal_id()?;
        let slot = self.terminals.get_mut(&id)?;
        if !slot.vt.terminal.is_mouse_tracking().unwrap_or(false) {
            return None;
        }
        let mut encoder = vt::mouse::Encoder::new().ok()?;
        encoder.set_options_from_terminal(&slot.vt.terminal);
        // Cell-aligned reporting: width=cols, height=rows, cell=1×1
        // pixel. `Position::{x,y}` in pixels then equals the cell
        // index, which is what the protocol expects in non-pixel
        // formats (the encoder divides x/cell_width to get the cell).
        let cols = slot.vt.cols.max(1) as u32;
        let rows = slot.vt.rows.max(1) as u32;
        encoder.set_size(vt::mouse::EncoderSize {
            screen_width: cols,
            screen_height: rows,
            cell_width: 1,
            cell_height: 1,
            padding_top: 0,
            padding_bottom: 0,
            padding_left: 0,
            padding_right: 0,
        });
        let mut event = vt::mouse::Event::new().ok()?;
        event
            .set_action(action)
            .set_button(button)
            .set_position(vt::mouse::Position {
                x: cell_col as f32,
                y: cell_row as f32,
            });
        let mut buf: Vec<u8> = Vec::with_capacity(32);
        encoder.encode_to_vec(&event, &mut buf).ok()?;
        if buf.is_empty() {
            return None;
        }
        Some((id, buf))
    }

    /// Scroll the focused terminal's viewport by `delta` rows.
    /// Negative scrolls up into the scrollback; positive scrolls
    /// down toward the live content. Called from the app loop's
    /// mouse-wheel handler so trackpad gestures move the viewport
    /// instead of just being eaten. Uses `focused_terminal_id` so
    /// both Tabs and Splits modes route to the right tile.
    pub fn scroll_active(&mut self, delta: isize) {
        if delta == 0 {
            return;
        }
        let Some(id) = self.focused_terminal_id() else {
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
            agent_state: pilot_ipc::AgentState::Active,
            last_rendered_size: None,
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

/// Inherent methods. Lifted from the legacy `tui_kit::Pane` trait.
impl TerminalStack {
    /// Stable pane id.
    pub fn id(&self) -> PaneId {
        self.id
    }

    /// Border title.
    pub fn title(&self) -> &str {
        "Terminals"
    }

    /// Whether this pane can pop into a detached window. Terminals
    /// don't (yet); the legacy trait default returned `None` and we
    /// preserve that here.
    pub fn detachable(&self) -> Option<crate::DetachSpec> {
        None
    }

    /// Bindings shown in the hint bar.
    pub fn keymap(&self) -> &'static [crate::Binding] {
        use crate::Binding;
        &[
            Binding { keys: "all keys", label: "→ PTY" },
            Binding { keys: "]]", label: "exit to sidebar" },
            Binding { keys: "Ctrl-c", label: "interrupt" },
        ]
    }

    pub fn handle_key(&mut self, key: KeyEvent, cmds: &mut Vec<Command>) -> PaneOutcome {
        // Tile-management prefix. Once `Ctrl-w` arms the latch, the
        // next key is a tile action (split, focus move, close);
        // anything unrecognised disarms cleanly. Same vocabulary as
        // tmux/vim windows so existing muscle memory transfers.
        if self.ctrl_w_armed {
            self.ctrl_w_armed = false;
            return self.handle_tile_action(key, cmds);
        }
        if key.code == KeyCode::Char('w') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.ctrl_w_armed = true;
            return PaneOutcome::Consumed;
        }

        // Escape sequence is owned by the app-level dispatcher (see
        // `dispatch_key` in app.rs). It uses a double-Esc latch on
        // `AppState` because that state needs to persist across calls.
        // Here we just route bytes to the focused terminal; everything
        // — q, Tab, Ctrl-C, single Esc — is the agent's.
        let id = self.focused_terminal_id().or_else(|| self.active_terminal_id());
        let Some(id) = id else {
            // No terminal to route to — let the parent handle.
            return PaneOutcome::Pass;
        };
        if !self.terminals.contains_key(&id) {
            return PaneOutcome::Pass;
        }

        let Some(bytes) = key_to_bytes(&key) else {
            return PaneOutcome::Consumed;
        };
        cmds.push(Command::Write {
            terminal_id: id,
            bytes,
        });
        PaneOutcome::Consumed
    }

    pub fn on_event(&mut self, event: &Event) {
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
                self.auto_collapse_on_emptiness();
            }
            Event::TerminalSpawned {
                terminal_id,
                session_key,
                kind,
            } => {
                let slot = Self::make_slot(session_key.clone(), kind.clone(), 0);
                self.terminals.insert(*terminal_id, slot);
                // A fresh terminal arrived for the active session —
                // expand so the user actually sees it. We bypass the
                // user-override here on purpose: spawning is itself an
                // explicit user action, and silently leaving the
                // section collapsed would make the user wonder if
                // anything happened.
                if Some(session_key) == self.active_session.as_ref() {
                    self.collapsed = false;
                    self.collapse_user_set = true;
                }

                // Stage 2 of a Ctrl-w split: wrap the focused leaf
                // in a fresh split with this new terminal as the
                // sibling. Without this, the new shell shows up as a
                // tab but never enters the tile tree.
                if let Some(direction) = self.pending_split.take()
                    && Some(session_key) == self.active_session.as_ref()
                {
                    self.commit_pending_split(*terminal_id, direction);
                } else if Some(session_key) == self.active_session.as_ref()
                    && matches!(self.layout, pilot_core::SessionLayout::Tabs { .. })
                    && self
                        .terminals
                        .iter()
                        .filter(|(_, slot)| Some(&slot.session_key) == self.active_session.as_ref())
                        .count()
                        >= 2
                {
                    // Two-or-more terminals on the same session: the
                    // user wants to see both. The Tabs default hides
                    // everything but the active tab; auto-promote to
                    // a vertical split so the new arrival lands beside
                    // the previous one. Single-terminal sessions stay
                    // in Tabs (cheaper render, no wasted dividers).
                    self.commit_pending_split(*terminal_id, PendingSplit::Vertical);
                }
            }
            Event::TerminalOutput {
                terminal_id,
                bytes,
                seq,
            } => {
                self.append_output(*terminal_id, bytes, *seq);
            }
            Event::TerminalFocusRequested { terminal_id } => {
                // Daemon-driven focus from the singleton guard.
                // Make the matching tab active + bring the pane up.
                self.focus_terminal(*terminal_id);
            }
            Event::AgentState { session_key, state } => {
                // Update every agent slot in this session — the
                // daemon broadcasts a single state per session_key.
                // Today only one agent of each kind runs per session
                // so this is unambiguous.
                for slot in self.terminals.values_mut() {
                    if &slot.session_key == session_key
                        && matches!(slot.kind, TerminalKind::Agent(_))
                    {
                        slot.agent_state = *state;
                    }
                }
            }
            Event::TerminalExited { terminal_id, .. } => {
                // Process exited (`exit`, ^D, segfault, kill from
                // outside) — close the window. Mirrors how every other
                // terminal emulator behaves: the prompt goes away, the
                // pane goes with it. Auto-spawn won't re-fire because
                // it's gated on first selection of the session.
                self.terminals.remove(terminal_id);
                // Prune the tile tree so the kill surfaces visually:
                // a single-leaf split collapses to a Leaf root; an
                // n-way split loses just the dead branch. Tabs mode
                // doesn't carry tile state — no work to do there.
                if let pilot_core::SessionLayout::Splits { tree, focused } =
                    &mut self.layout
                {
                    if let Some(path) = tree.path_to(terminal_id.0) {
                        match tree.remove_at(&path) {
                            Ok(new_focus) => {
                                *focused = new_focus;
                            }
                            Err(()) => {
                                // path was empty (the killed leaf was
                                // the only tile) → drop back to the
                                // tabs default so a future spawn opens
                                // a fresh layout instead of leaving an
                                // orphan tree.
                                self.layout =
                                    pilot_core::SessionLayout::Tabs { active: 0 };
                            }
                        }
                    }
                    // If the post-collapse tree is just a Leaf, drop
                    // back to Tabs — keeping a Splits-with-single-leaf
                    // payload renders fine but means the next spawn
                    // promotes us right back into Splits, which is
                    // confusing UX.
                    if let pilot_core::SessionLayout::Splits { tree, .. } =
                        &self.layout
                        && matches!(tree, pilot_core::TileTree::Leaf { .. })
                    {
                        self.layout = pilot_core::SessionLayout::Tabs { active: 0 };
                    }
                }
                self.clamp_active_tab();
                self.auto_collapse_on_emptiness();
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
                self.auto_collapse_on_emptiness();
            }
            _ => {}
        }
    }

    pub fn render(&mut self, area: Rect, frame: &mut Frame, focused: bool) {
        // Modern minimal: title row + thin divider, no surrounding box.
        let theme = crate::theme::current();

        let visible = self.visible_terminals();
        let title_area = Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(2),
            1.min(area.height),
        );
        // Title row: "Terminals" plus an icon+label per active terminal
        // (e.g. `Terminals    claude   _ shell`). Active is bold-accent;
        // inactive is dim grey. Two-tab common case looks like a tab
        // strip; single-terminal shows just one entry.
        let mut title_spans: Vec<Span<'static>> = vec![
            Span::styled("Terminals", theme.title(focused)),
            Span::raw("  "),
        ];
        for (i, id) in visible.iter().enumerate() {
            let (icon, label, is_asking) = self
                .terminals
                .get(id)
                .map(|s| {
                    let icon: &'static str = match &s.kind {
                        TerminalKind::Shell => crate::components::icons::SHELL,
                        TerminalKind::Agent(agent_id) => {
                            crate::components::icons::agent_icon(agent_id)
                        }
                        // Log-tail terminals reuse the shell glyph for now.
                        _ => crate::components::icons::SHELL,
                    };
                    let asking = matches!(s.agent_state, pilot_ipc::AgentState::Asking);
                    (icon, Self::tab_label(&s.kind), asking)
                })
                .unwrap_or((crate::components::icons::SHELL, "?".into(), false));
            let is_active = i == self.active_tab_idx;
            let style = if is_active && focused {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else if is_active {
                Style::default()
                    .fg(theme.text_strong)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.chrome)
            };
            if i > 0 {
                title_spans.push(Span::raw("  "));
            }
            title_spans.push(Span::styled(format!("{icon} {label}"), style));
            // Bold yellow "!" next to an agent waiting on the user.
            // Stays prominent regardless of which tab is active so
            // the user notices a Claude prompt even while typing in
            // a different shell.
            if is_asking {
                title_spans.push(Span::styled(
                    " ! needs input",
                    Style::default()
                        .fg(theme.warn)
                        .add_modifier(Modifier::BOLD),
                ));
            }
        }
        frame.render_widget(Paragraph::new(Line::from(title_spans)), title_area);

        if area.height >= 2 {
            let div_area = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), 1);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "─".repeat(div_area.width as usize),
                    theme.divider(),
                ))),
                div_area,
            );
        }

        let inner = Rect {
            x: area.x + 1,
            y: area.y + 3,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(3),
        };

        if visible.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "(no terminals — press s for shell, c for claude)",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ))),
                inner,
            );
            return;
        }

        // Branch on layout. Tabs = legacy single-pane render. Splits
        // = walk the tile tree, render each leaf at its rect with
        // dividers between.
        let body = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height,
        };

        match self.layout.clone() {
            pilot_core::SessionLayout::Tabs { .. } => {
                // Render the active tab full-pane (existing behavior).
                if let Some(id) = self.active_terminal_id() {
                    self.render_one_terminal(id, body, frame, focused);
                }
            }
            pilot_core::SessionLayout::Splits { tree, focused: focus_path } => {
                // Recursive tile renderer. Dividers are drawn on the
                // boundary between adjacent leaves; the focused leaf
                // gets a brighter border so the user can tell where
                // typing lands.
                let theme_chrome = theme.chrome;
                let theme_accent = theme.accent;
                self.render_tile_tree(
                    &tree,
                    body,
                    frame,
                    focused,
                    &focus_path,
                    &[],
                    theme_chrome,
                    theme_accent,
                );
            }
        }
    }
}

impl TerminalStack {
    /// Commit a pending split: take the currently-focused leaf in
    /// the layout (or fabricate one if we're still in Tabs mode) and
    /// wrap it in a fresh `HSplit`/`VSplit` whose other side is the
    /// new terminal. After the mutation the user keeps focus on the
    /// new leaf so they can immediately type into the freshly-
    /// spawned shell.
    fn commit_pending_split(&mut self, new_id: TerminalId, direction: PendingSplit) {
        // Promote Tabs → Splits if needed. The Tabs mode's "focused"
        // leaf is the active terminal id.
        let mut tree = match self.layout.clone() {
            pilot_core::SessionLayout::Splits { tree, .. } => tree,
            pilot_core::SessionLayout::Tabs { .. } => {
                let Some(current_id) = self.active_terminal_id() else {
                    // No terminal at all yet — the new spawn is just
                    // the first tab. Stay in Tabs mode.
                    return;
                };
                pilot_core::TileTree::Leaf {
                    terminal_id: current_id.0,
                }
            }
        };
        let focused_path = match &self.layout {
            pilot_core::SessionLayout::Splits { focused, .. } => focused.clone(),
            pilot_core::SessionLayout::Tabs { .. } => Vec::new(),
        };

        // Read the existing leaf at the focused path, build the new
        // split with [old, new] (so the new tile lands to the right
        // / below — matches tmux defaults), put it back at the path.
        let Some(existing) = subtree_at_path(&tree, &focused_path).cloned() else {
            return;
        };
        let new_leaf = pilot_core::TileTree::Leaf {
            terminal_id: new_id.0,
        };
        let new_split = match direction {
            PendingSplit::Vertical => pilot_core::TileTree::HSplit {
                left: Box::new(existing),
                right: Box::new(new_leaf),
                ratio: 50,
            },
            PendingSplit::Horizontal => pilot_core::TileTree::VSplit {
                top: Box::new(existing),
                bottom: Box::new(new_leaf),
                ratio: 50,
            },
        };
        tree.replace_at(&focused_path, new_split);

        // New focus = the new leaf, which is the second child of the
        // split we just inserted at `focused_path`.
        let mut new_focus = focused_path;
        new_focus.push(1);
        self.layout = pilot_core::SessionLayout::Splits {
            tree,
            focused: new_focus,
        };
    }

    /// Tile-action dispatch: a key arriving right after `Ctrl-w`.
    /// Splits, focus moves, close, escape. Anything unrecognised is
    /// a clean no-op (the prefix has already been consumed; the user
    /// just has to retry).
    fn handle_tile_action(
        &mut self,
        key: KeyEvent,
        cmds: &mut Vec<Command>,
    ) -> PaneOutcome {
        use pilot_core::TileDirection;

        // Need an active session to know where to spawn into. Without
        // one, splits + new shells have nowhere to land.
        let Some(session_key) = self.active_session.clone() else {
            return PaneOutcome::Consumed;
        };

        match (key.code, key.modifiers) {
            (KeyCode::Char('|'), _) | (KeyCode::Char('\\'), _) => {
                self.begin_split(session_key, PendingSplit::Vertical, cmds);
            }
            (KeyCode::Char('-'), _) => {
                self.begin_split(session_key, PendingSplit::Horizontal, cmds);
            }
            (KeyCode::Char('h'), _) => self.move_focus(TileDirection::Left, cmds),
            (KeyCode::Char('j'), _) => self.move_focus(TileDirection::Down, cmds),
            (KeyCode::Char('k'), _) => self.move_focus(TileDirection::Up, cmds),
            (KeyCode::Char('l'), _) => self.move_focus(TileDirection::Right, cmds),
            (KeyCode::Char('q'), _) => self.close_focused_tile(cmds),
            _ => {}
        }
        PaneOutcome::Consumed
    }

    /// Stage 1 of a split: arm the pending-split flag and emit a
    /// shell-spawn command. The new terminal id arrives on
    /// `Event::TerminalSpawned`; that's where we mutate the layout.
    fn begin_split(
        &mut self,
        session_key: SessionKey,
        direction: PendingSplit,
        cmds: &mut Vec<Command>,
    ) {
        self.pending_split = Some(direction);
        cmds.push(Command::Spawn {
            session_key,
            session_id: None,
            kind: TerminalKind::Shell,
            cwd: None,
        });
    }

    /// Move focus across the tile tree (or cycle through tabs in
    /// Tabs mode). Persists the new layout via `SetSessionLayout`.
    fn move_focus(&mut self, dir: pilot_core::TileDirection, cmds: &mut Vec<Command>) {
        match &mut self.layout {
            pilot_core::SessionLayout::Tabs { active } => {
                // In tabs mode h/l cycle the tab strip; j/k are no-ops
                // since there's only one row of "tabs" stacked vertically.
                let n = self.terminals.len();
                if n == 0 {
                    return;
                }
                match dir {
                    pilot_core::TileDirection::Left => {
                        *active = if *active == 0 { n - 1 } else { *active - 1 };
                    }
                    pilot_core::TileDirection::Right => {
                        *active = (*active + 1) % n;
                    }
                    _ => {}
                }
                self.active_tab_idx = *active;
            }
            pilot_core::SessionLayout::Splits { tree, focused } => {
                if let Some(new_path) = tree.neighbor(focused, dir) {
                    *focused = new_path;
                }
            }
        }
        self.persist_layout(cmds);
    }

    /// Close the focused leaf, collapsing its parent split into the
    /// surviving sibling. Single-leaf trees are refused (would leave
    /// the session with nothing visible).
    fn close_focused_tile(&mut self, cmds: &mut Vec<Command>) {
        let pilot_core::SessionLayout::Splits { tree, focused } = &mut self.layout else {
            return;
        };
        // Capture the terminal that's about to disappear before we
        // mutate the tree — we'll close its PTY too.
        let target_id = subtree_at_path(tree, focused).and_then(|n| match n {
            pilot_core::TileTree::Leaf { terminal_id } => Some(*terminal_id),
            _ => None,
        });
        if tree.remove_at(focused).is_ok() {
            // After collapse, descend into a leaf so focus lands on
            // a real tile (not a now-stale split path).
            let leaves = tree.leaves();
            if let Some(first) = leaves.first()
                && let Some(p) = tree.path_to(*first)
            {
                *focused = p;
            } else {
                *focused = Vec::new();
            }
            // If the close left us with a single leaf, downgrade to
            // Tabs so the rest of the UI (tab strip, focus models)
            // doesn't see a degenerate splits tree.
            if leaves.len() <= 1 {
                self.layout = pilot_core::SessionLayout::Tabs { active: 0 };
                self.active_tab_idx = 0;
            }
            if let Some(id) = target_id {
                cmds.push(Command::Close {
                    terminal_id: TerminalId(id),
                });
            }
            self.persist_layout(cmds);
        }
    }

    /// Push a `Command::SetSessionLayout` for the currently-active
    /// session if we know which one we're on. The daemon writes the
    /// new layout to the workspace record + rebroadcasts.
    fn persist_layout(&self, cmds: &mut Vec<Command>) {
        let Some(session_key) = &self.active_session else {
            return;
        };
        let Ok(layout_json) = serde_json::to_string(&self.layout) else {
            return;
        };
        // Find the session id we belong to. With one session per
        // workspace today, the active_session string IS the workspace
        // key; the user picks the first session by default. A future
        // multi-session sidebar would override this with an explicit
        // session id from selected_session_id. For now, leave the id
        // empty and the daemon's handler tolerates it (no-op).
        cmds.push(Command::SetSessionLayout {
            session_key: session_key.clone(),
            session_id_raw: String::new(),
            layout_json,
        });
    }

    /// Render a single terminal slot full-rect. Used by both the
    /// tabs path and the splits path's leaf case.
    fn render_one_terminal(
        &mut self,
        id: TerminalId,
        rect: Rect,
        frame: &mut Frame,
        focused: bool,
    ) {
        let _ = focused; // ghostty-vt doesn't render focus chrome itself
        if let Some(slot) = self.terminals.get_mut(&id) {
            slot.vt.ensure_size(rect.width, rect.height);
            // Backend PTY also needs to know the new size — otherwise
            // the shell process keeps writing at its spawn dimensions
            // and the bottom rows go blank as soon as the user scrolls
            // past them. Queue a resize for the App to ship.
            let new_size = (rect.width, rect.height);
            if rect.width > 0
                && rect.height > 0
                && slot.last_rendered_size != Some(new_size)
            {
                slot.last_rendered_size = Some(new_size);
                self.pending_resizes.push((id, rect.width, rect.height));
            }
            if let Ok(snapshot) = slot.vt.render_state.update(&slot.vt.terminal) {
                let widget = GhosttyTerminal::new(
                    &snapshot,
                    &mut slot.vt.row_iter,
                    &mut slot.vt.cell_iter,
                );
                frame.render_widget(widget, rect);
            }
        }
    }

    /// Recursive walk of the tile tree. Each Leaf gets its own rect
    /// rendered via the existing per-terminal pipeline; each Split
    /// divides its rect according to `ratio` and recurses, drawing a
    /// thin divider line between the two children.
    #[allow(clippy::too_many_arguments)]
    fn render_tile_tree(
        &mut self,
        node: &pilot_core::TileTree,
        rect: Rect,
        frame: &mut Frame,
        pane_focused: bool,
        focus_path: &[u8],
        current_path: &[u8],
        chrome: Color,
        accent: Color,
    ) {
        match node {
            pilot_core::TileTree::Leaf { terminal_id } => {
                let is_focused_leaf = pane_focused && current_path == focus_path;
                self.render_one_terminal(TerminalId(*terminal_id), rect, frame, is_focused_leaf);
                // Highlight the focused leaf with a one-cell top
                // accent line. Subtle but enough to disambiguate
                // when two shells look identical.
                if is_focused_leaf && rect.height > 0 && rect.width > 0 {
                    let bar = Rect {
                        x: rect.x,
                        y: rect.y,
                        width: rect.width,
                        height: 1,
                    };
                    frame.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            "─".repeat(bar.width as usize),
                            Style::default().fg(accent),
                        ))),
                        bar,
                    );
                }
            }
            pilot_core::TileTree::HSplit { left, right, ratio } => {
                let split_at = (rect.width as u32 * (*ratio).min(100) as u32 / 100) as u16;
                let left_w = split_at.min(rect.width.saturating_sub(1));
                let right_x = rect.x + left_w + 1;
                let right_w = rect.width.saturating_sub(left_w + 1);
                let left_rect = Rect {
                    x: rect.x,
                    y: rect.y,
                    width: left_w,
                    height: rect.height,
                };
                let right_rect = Rect {
                    x: right_x,
                    y: rect.y,
                    width: right_w,
                    height: rect.height,
                };
                let mut p_left = current_path.to_vec();
                p_left.push(0);
                let mut p_right = current_path.to_vec();
                p_right.push(1);
                self.render_tile_tree(
                    left, left_rect, frame, pane_focused, focus_path, &p_left, chrome, accent,
                );
                self.render_tile_tree(
                    right, right_rect, frame, pane_focused, focus_path, &p_right, chrome, accent,
                );
                // Vertical divider between the two halves.
                if rect.height > 0 {
                    let div = Rect {
                        x: rect.x + left_w,
                        y: rect.y,
                        width: 1,
                        height: rect.height,
                    };
                    let lines: Vec<Line> = (0..rect.height)
                        .map(|_| Line::from(Span::styled("│", Style::default().fg(chrome))))
                        .collect();
                    frame.render_widget(Paragraph::new(lines), div);
                }
            }
            pilot_core::TileTree::VSplit { top, bottom, ratio } => {
                let split_at = (rect.height as u32 * (*ratio).min(100) as u32 / 100) as u16;
                let top_h = split_at.min(rect.height.saturating_sub(1));
                let bottom_y = rect.y + top_h + 1;
                let bottom_h = rect.height.saturating_sub(top_h + 1);
                let top_rect = Rect {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: top_h,
                };
                let bottom_rect = Rect {
                    x: rect.x,
                    y: bottom_y,
                    width: rect.width,
                    height: bottom_h,
                };
                let mut p_top = current_path.to_vec();
                p_top.push(0);
                let mut p_bot = current_path.to_vec();
                p_bot.push(1);
                self.render_tile_tree(
                    top, top_rect, frame, pane_focused, focus_path, &p_top, chrome, accent,
                );
                self.render_tile_tree(
                    bottom,
                    bottom_rect,
                    frame,
                    pane_focused,
                    focus_path,
                    &p_bot,
                    chrome,
                    accent,
                );
                // Horizontal divider.
                if rect.width > 0 {
                    let div = Rect {
                        x: rect.x,
                        y: rect.y + top_h,
                        width: rect.width,
                        height: 1,
                    };
                    frame.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            "─".repeat(div.width as usize),
                            Style::default().fg(chrome),
                        ))),
                        div,
                    );
                }
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

/// Encode a key event as the bytes we'd write to a PTY. Returns None
/// for keys we don't know how to encode yet. Public so the app-level
/// escape-latch can flush buffered keystrokes through the same
/// encoding path the live key dispatch uses.
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
