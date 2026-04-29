//! The live TUI run loop: mount the component tree, drive it with
//! crossterm's `EventStream` and the daemon's IPC event stream, render
//! on every change.
//!
//! This is the piece that turns a pile of unit-tested components into
//! an actual usable binary. It intentionally lives separate from
//! `main.rs` so we can (later) drive it from integration tests with a
//! synthetic `Client` without dragging in argv parsing and
//! `tracing_subscriber` init.

use crate::components::terminal_stack::key_to_bytes;
use crate::components::{Help, RightPane, Root, Sidebar, TerminalStack};
use crate::layout::{self, HSizing, Node, Placeholder, Slot, SplitterId, VSizing};
use crate::setup_flow::{self, SetupOutcome};
use crate::{ComponentId, ComponentTree};
use crossterm::event::{Event as CEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::{execute, terminal};
use futures_util::StreamExt;
use pilot_config::TerminalSection;
use pilot_core::SessionKey;
use pilot_v2_ipc::{Client, Command, Event};
use ratatui::Terminal;
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use std::collections::HashSet;
use std::io;
use std::time::{Duration, Instant};

pub struct Ids {
    pub root: ComponentId,
    pub sidebar: ComponentId,
    pub right: ComponentId,
    pub terminals: ComponentId,
}

/// Transient UI state lifted out of the component tree. Most things
/// belong in components; this is for cross-cutting concerns where a
/// component-level home would be artificial.
pub struct AppState {
    /// Most recent error to surface in the status line. `None` means
    /// "no error to show". The bottom bar renders this in red until
    /// the user takes another action that clears it.
    pub last_error: Option<String>,
    /// First-press time for the q-q double-tap. `q` arms the latch;
    /// a second `q` within `Q_DOUBLE_TAP_WINDOW` quits.
    pub q_armed_at: Option<Instant>,
    /// Pane geometry (sidebar width, right vertical split). Loaded
    /// from the v2 store at startup, persisted on every resize.
    pub layout: pilot_core::PaneLayout,
    /// Whether `layout` has unsaved changes. The event loop checks
    /// this after each tick and writes through to the store if set,
    /// so we don't write on every keystroke.
    pub layout_dirty: bool,
    /// Active mouse drag, if any. While `Some`, mouse events are
    /// interpreted as splitter drags and component focus is cleared
    /// so keystrokes can't sneak through to a half-rendered pane.
    pub dragging: Option<Drag>,
    /// Splitter the mouse is currently hovering over. Drives the
    /// "this is draggable" visual highlight so the user knows the
    /// splitter is interactive without us having to change the OS
    /// mouse cursor (which a TUI can't actually do).
    pub hovering_splitter: Option<Drag>,
    /// Last frame size we drew. Mouse hit-tests use this to compute
    /// where the splitters live in absolute coordinates. (0,0) until
    /// the first frame is drawn, which means mouse events before the
    /// first paint do nothing — that's fine.
    pub last_frame: (u16, u16),
    /// Sessions where we've already issued an auto-spawn shell on
    /// selection. Without this we'd re-spawn on every selection bounce
    /// and pile up shells. The set is per-process — if the user
    /// explicitly closes the auto-spawned terminal we honor that and
    /// never resurrect it during this run.
    pub auto_spawned: HashSet<SessionKey>,
    /// User-configurable escape sequence settings. Default `]]]` —
    /// three closing brackets within 600 ms exits the terminal pane.
    pub escape_cfg: TerminalSection,
    /// In-flight buffer of escape-char keystrokes pending decision.
    /// While the user is mid-sequence we DON'T forward them to the
    /// agent — if they finish the sequence the agent never sees them,
    /// if they break it (different key OR window timeout) we flush
    /// the buffer through and process the new key normally.
    pub escape_buffer: Vec<KeyEvent>,
    /// Timestamp of the most recent escape-char in `escape_buffer`.
    /// Used to expire the buffer when the user pauses typing.
    pub escape_buffer_at: Option<Instant>,
}

/// Which splitter is the user currently dragging.
///
/// Mirrors `layout::SplitterId` — the layout tree is the source of
/// truth for what splitters exist, this enum is just the local
/// "what's-being-dragged" state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drag {
    Sidebar,
    RightSplit,
}

impl Drag {
    fn from_splitter(id: SplitterId) -> Self {
        match id {
            SplitterId::Sidebar => Drag::Sidebar,
            SplitterId::RightVertical => Drag::RightSplit,
        }
    }
    fn to_splitter(self) -> SplitterId {
        match self {
            Drag::Sidebar => SplitterId::Sidebar,
            Drag::RightSplit => SplitterId::RightVertical,
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            last_error: None,
            q_armed_at: None,
            layout: pilot_core::PaneLayout::DEFAULT,
            layout_dirty: false,
            dragging: None,
            hovering_splitter: None,
            last_frame: (0, 0),
            auto_spawned: HashSet::new(),
            escape_cfg: TerminalSection::default(),
            escape_buffer: Vec::new(),
            escape_buffer_at: None,
        }
    }

    /// Build with a layout loaded from the store (if any). Falls back
    /// to `PaneLayout::DEFAULT` for missing / corrupt rows.
    pub fn with_store(store: Option<&dyn pilot_store::Store>) -> Self {
        let mut s = Self::new();
        if let Some(store) = store
            && let Ok(Some(raw)) = store.get_kv(pilot_core::KV_KEY_LAYOUT)
            && let Ok(layout) = serde_json::from_str::<pilot_core::PaneLayout>(&raw)
        {
            s.layout = layout.clamp();
        }
        s
    }
}

/// How long the first `q` stays armed waiting for the second tap.
const Q_DOUBLE_TAP_WINDOW: std::time::Duration = std::time::Duration::from_millis(800);

pub async fn run(client: Client) -> anyhow::Result<()> {
    run_inner(client, false, |_| {}, None).await
}

/// Variant for `pilot --fresh`: forces the setup screen to display
/// even if the detection report is fully green, so the user can
/// review what we picked up before any polling happens.
pub async fn run_force_setup(client: Client) -> anyhow::Result<()> {
    run_inner(client, true, |_| {}, None).await
}

/// `run` plus a hook that fires once setup completes with the user's
/// confirmed integration choices. The binary uses this to spawn the
/// daemon's polling loop with `polling::sources_for(&outcome…)` so
/// disabled integrations don't get polled. The hook runs synchronously
/// on the main task before the TUI loop starts; if it needs to do
/// async work it should `tokio::spawn` from inside the closure.
pub async fn run_with_setup_hook<F>(
    client: Client,
    force_setup: bool,
    on_setup_complete: F,
) -> anyhow::Result<()>
where
    F: FnOnce(&SetupOutcome) + Send,
{
    run_inner(client, force_setup, on_setup_complete, None).await
}

/// `pilot --test` entry point: run the TUI assuming setup is already
/// satisfied (the test fixture seeded the kv row), with no polling
/// hook. The persisted-setup row in the in-memory store is what
/// causes `setup_flow::run_with_persistence` to skip the screen.
pub async fn run_test_mode(
    client: Client,
    store: std::sync::Arc<dyn pilot_store::Store>,
) -> anyhow::Result<()> {
    run_inner(client, false, |_| {}, Some(store)).await
}

async fn run_inner<F>(
    mut client: Client,
    force_setup: bool,
    on_setup_complete: F,
    override_store: Option<std::sync::Arc<dyn pilot_store::Store>>,
) -> anyhow::Result<()>
where
    F: FnOnce(&SetupOutcome) + Send,
{
    // ── Terminal setup ────────────────────────────────────────────────
    // The setup flow and the main loop share the SAME raw-mode + alt
    // screen lifetime so we don't flicker between them. Setup runs
    // first, owns the entire screen, and the main UI never paints
    // until setup hands back an outcome.
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        terminal::EnterAlternateScreen,
        // Bracketed paste: pasted text arrives as CEvent::Paste(String)
        // and we route it as a single Command::Write so the agent PTY
        // sees one chunk, not a stream of keystrokes.
        crossterm::event::EnableBracketedPaste,
        // Mouse capture: clicks and drags arrive as CEvent::Mouse and
        // we use them to drive splitter resize. NATIVE TEXT SELECTION
        // still works in modern terminals (iTerm2, WezTerm, Ghostty,
        // Alacritty) by holding Shift while dragging, which forces
        // local selection regardless of mouse capture state. Help
        // overlay documents this.
        crossterm::event::EnableMouseCapture,
    )?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let result = run_after_terminal(
        &mut term,
        &mut client,
        force_setup,
        on_setup_complete,
        override_store,
    )
    .await;

    // ── Terminal teardown (always runs) ───────────────────────────────
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        term.backend_mut(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
        terminal::LeaveAlternateScreen,
    );
    let _ = term.show_cursor();
    result
}

/// Concrete backend type used everywhere in the run loop.
///
/// We deliberately drop the `<B: Backend>` generic that earlier
/// versions had: ratatui 0.30's `Backend::Error` is associated and
/// the cascade of `Send + Sync + 'static` bounds it would require to
/// stay generic isn't worth the testability win — every caller uses
/// crossterm anyway.
pub type AppBackend = ratatui::backend::CrosstermBackend<std::io::Stdout>;

/// Resolve which `ScopeSource` impls to hand the setup runner.
/// Best-effort: each provider builds its own credential chain and
/// is silently skipped if auth isn't available — the picker phase
/// then renders empty for that provider rather than blocking the
/// user. Tests bypass this by calling
/// `run_with_persistence_and_scopes` directly with `MockScopeSource`.
async fn build_scope_sources() -> Vec<Box<dyn pilot_core::ScopeSource>> {
    use pilot_auth::{CommandProvider, CredentialChain, EnvProvider};
    let mut sources: Vec<Box<dyn pilot_core::ScopeSource>> = Vec::new();
    let chain = CredentialChain::new()
        .with(EnvProvider::new("GH_TOKEN"))
        .with(EnvProvider::new("GITHUB_TOKEN"))
        .with(CommandProvider::new("gh", &["auth", "token"]));
    if let Ok(cred) = chain.resolve("github").await
        && let Ok(client) = pilot_gh::GhClient::from_credential(cred).await
    {
        sources.push(Box::new(pilot_gh::GhScopes::new(std::sync::Arc::new(
            client,
        ))));
    }
    sources
}

async fn run_after_terminal<F>(
    term: &mut Terminal<AppBackend>,
    client: &mut Client,
    force_setup: bool,
    on_setup_complete: F,
    override_store: Option<std::sync::Arc<dyn pilot_store::Store>>,
) -> anyhow::Result<()>
where
    F: FnOnce(&SetupOutcome),
{
    // 1. Setup phase. Owns the screen until done. The persisted state
    //    in v2's SQLite kv table lets subsequent launches skip the
    //    screen entirely; --fresh deletes the kv row (see main.rs).
    //    `override_store` is used by `--test` mode to point setup at
    //    an in-memory store with the kv row pre-populated, which
    //    makes the setup screen skip without touching disk.
    let store = override_store.or_else(pilot_v2_server::open_v2_store);
    // Build per-provider ScopeSources for the picker phase. Today
    // only GitHub has one — others get an empty list and the picker
    // skips. We try-resolve credentials best-effort: if the user has
    // no GH token the picker still renders with "no repos visible"
    // and the user can confirm to subscribe to nothing-special.
    let scope_sources = build_scope_sources().await;
    let outcome = match setup_flow::run_with_persistence_and_scopes(
        term,
        force_setup,
        store.clone(),
        scope_sources,
    )
    .await?
    {
        Some(o) => o,
        None => return Ok(()), // user quit during setup
    };

    // Fire the hook BEFORE the main loop subscribes, so polling has
    // a chance to spin up its sources (which involves credential
    // resolution, possibly network). The first SessionUpserted event
    // may then arrive while the loop is already drawing.
    on_setup_complete(&outcome);

    // 2. Main TUI phase.
    let (mut tree, ids) = build_tree();
    let mut state = AppState::with_store(store.as_deref());
    let _ = client.send(Command::Subscribe);
    let mut crossterm_events = EventStream::new();
    event_loop(
        term,
        &mut tree,
        &ids,
        &mut state,
        store.as_ref(),
        client,
        &mut crossterm_events,
    )
    .await
}

async fn event_loop(
    term: &mut Terminal<AppBackend>,
    tree: &mut ComponentTree,
    ids: &Ids,
    state: &mut AppState,
    store: Option<&std::sync::Arc<dyn pilot_store::Store>>,
    client: &mut Client,
    crossterm_events: &mut EventStream,
) -> anyhow::Result<()> {
    draw(term, tree, ids, state)?;
    loop {
        tokio::select! {
            ct = crossterm_events.next() => {
                let Some(Ok(evt)) = ct else { break };
                match evt {
                    CEvent::Key(key) => {
                        if dispatch_key(key, tree, ids, state, client) {
                            break;
                        }
                    }
                    CEvent::Paste(text) => {
                        dispatch_paste(text, tree, ids, client);
                    }
                    CEvent::Mouse(me) => {
                        dispatch_mouse(me, tree, ids, state);
                    }
                    _ => {}
                }
                persist_layout_if_dirty(state, store);
                sync_panes(tree, ids, state, client);
                draw(term, tree, ids, state)?;
            }
            ipc = client.recv() => {
                let Some(event) = ipc else {
                    // Server closed the stream — exit cleanly.
                    break;
                };
                if let Event::ProviderError { source, message } = &event {
                    state.last_error = Some(format!("{source}: {message}"));
                }
                tree.broadcast_event(&event);
                handle_ipc_side_effects(tree, ids, &event);
                sync_panes(tree, ids, state, client);
                draw(term, tree, ids, state)?;
            }
        }
    }
    Ok(())
}

fn persist_layout_if_dirty(
    state: &mut AppState,
    store: Option<&std::sync::Arc<dyn pilot_store::Store>>,
) {
    if !state.layout_dirty {
        return;
    }
    state.layout_dirty = false;
    let Some(store) = store else { return };
    let Ok(json) = serde_json::to_string(&state.layout) else {
        return;
    };
    if let Err(e) = store.set_kv(pilot_core::KV_KEY_LAYOUT, &json) {
        tracing::warn!("layout persist failed: {e}");
    }
}

/// Build the canonical app tree (Root + Sidebar + RightPane +
/// TerminalStack as siblings, focus on Sidebar). Lifted out of `run`
/// so tests can construct exactly what the live binary does.
pub fn build_tree() -> (ComponentTree, Ids) {
    let root_id = ComponentId::new(0);
    let mut tree = ComponentTree::new(Box::new(Root::new(root_id)));
    let sidebar_id = tree.alloc_id();
    let right_id = tree.alloc_id();
    let terminals_id = tree.alloc_id();
    tree.mount_child(root_id, Box::new(Sidebar::new(sidebar_id)))
        .expect("mount sidebar");
    tree.mount_child(root_id, Box::new(RightPane::new(right_id)))
        .expect("mount right pane");
    tree.mount_child(root_id, Box::new(TerminalStack::new(terminals_id)))
        .expect("mount terminal stack");
    tree.set_focus(sidebar_id);
    let ids = Ids {
        root: root_id,
        sidebar: sidebar_id,
        right: right_id,
        terminals: terminals_id,
    };
    (tree, ids)
}

/// Outcome of feeding one keystroke through the configurable
/// terminal-escape latch. The latch only runs when focus is on the
/// terminal stack — see `dispatch_key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscapeOutcome {
    /// Key was an escape-char press that's still part of an
    /// in-progress sequence. Don't write to the agent yet — we'll
    /// flush or fire later. Caller should return early.
    Buffered,
    /// Latch reached `escape_count` matches. Caller should pull focus
    /// back to the sidebar; the buffer has already been drained.
    Triggered,
    /// Sequence was broken by a different key (or stale by timeout).
    /// The buffered escape-chars are returned so the caller can flush
    /// them to the agent BEFORE processing the new key.
    Flush(Vec<KeyEvent>),
    /// Key didn't interact with the latch at all (e.g. it's not the
    /// escape char and the buffer was already empty). Caller proceeds
    /// to normal handling.
    Pass,
}

/// Drives the terminal-escape latch one step. Logic:
/// - Window expired → existing buffer is now stale; flush it BEFORE
///   processing the current key (the user paused too long for it to
///   count as part of the same run).
/// - Key matches escape_char → push to buffer; if we just hit
///   `escape_count`, fire `Triggered` and clear.
/// - Buffer non-empty + key doesn't match → flush buffered chars and
///   process the new key normally.
/// - Buffer empty + key doesn't match → `Pass`.
pub fn advance_escape_latch(key: &KeyEvent, state: &mut AppState) -> EscapeOutcome {
    let now = Instant::now();
    let window = Duration::from_millis(state.escape_cfg.escape_window_ms);

    // Stale-buffer check: if the user typed `]` then waited too long,
    // those buffered chars are no longer part of the current run.
    // Flush them before the new key. The new key joins a fresh run if
    // it ALSO matches.
    let stale = state
        .escape_buffer_at
        .map(|t| now.duration_since(t) > window)
        .unwrap_or(false);
    if stale && !state.escape_buffer.is_empty() {
        let flushed = std::mem::take(&mut state.escape_buffer);
        state.escape_buffer_at = None;
        // Re-feed the new key against the now-empty latch.
        let outcome = advance_escape_latch(key, state);
        return match outcome {
            EscapeOutcome::Pass | EscapeOutcome::Flush(_) => EscapeOutcome::Flush(flushed),
            EscapeOutcome::Buffered => EscapeOutcome::Flush(flushed),
            EscapeOutcome::Triggered => {
                // Flush the stale ones; the trigger fired on the new
                // key alone (impossible unless escape_count == 1, which
                // we forbid). Defensive: flush + trigger.
                EscapeOutcome::Flush(flushed)
            }
        };
    }

    let is_escape_char = matches!(key.code, KeyCode::Char(c) if c == state.escape_cfg.escape_char)
        && key.modifiers == KeyModifiers::NONE;

    if is_escape_char {
        state.escape_buffer.push(*key);
        state.escape_buffer_at = Some(now);
        if state.escape_buffer.len() >= state.escape_cfg.escape_count.max(2) as usize {
            state.escape_buffer.clear();
            state.escape_buffer_at = None;
            return EscapeOutcome::Triggered;
        }
        return EscapeOutcome::Buffered;
    }

    if state.escape_buffer.is_empty() {
        return EscapeOutcome::Pass;
    }
    let flushed = std::mem::take(&mut state.escape_buffer);
    state.escape_buffer_at = None;
    EscapeOutcome::Flush(flushed)
}

/// Returns `true` if the user asked to quit.
pub fn dispatch_key(
    key: KeyEvent,
    tree: &mut ComponentTree,
    ids: &Ids,
    state: &mut AppState,
    client: &mut Client,
) -> bool {
    // Any keystroke clears a stale error from the status line.
    state.last_error = None;

    let focus_in_terminal = tree.focused() == Some(ids.terminals);

    // ── Terminal-escape latch (configurable; default `]]]`) ──────────
    // The ONLY way to leave the terminal pane back to the sidebar.
    // Everything else — q, Tab, Ctrl-C, Ctrl-], Esc — flows to the
    // agent. We buffer pending escape chars so a real `]]]` while
    // typing code never arrives at the agent half-typed: either the
    // user finishes the sequence (we exit, agent sees nothing), or
    // they break it (we flush the buffer THEN write the new key).
    if focus_in_terminal {
        match advance_escape_latch(&key, state) {
            EscapeOutcome::Buffered => return false,
            EscapeOutcome::Triggered => {
                tree.set_focus(ids.sidebar);
                return false;
            }
            EscapeOutcome::Flush(flushed) => {
                if let Some(ts) = tree.get::<TerminalStack>(ids.terminals)
                    && let Some(terminal_id) = ts.active_terminal_id()
                {
                    let mut bytes: Vec<u8> = Vec::with_capacity(flushed.len());
                    for k in &flushed {
                        if let Some(b) = key_to_bytes(k) {
                            bytes.extend_from_slice(&b);
                        }
                    }
                    if !bytes.is_empty() {
                        let _ = client.send(Command::Write { terminal_id, bytes });
                    }
                }
                // Fall through to process the current key normally.
            }
            EscapeOutcome::Pass => {}
        }
    }

    // Ctrl-C: outside a terminal we treat it as "quit" (terminal
    // convention). Inside a terminal, the running process owns it —
    // interrupting `cat`, killing a build, escaping a Claude prompt
    // all need Ctrl-C to flow through. We route it to the active
    // terminal as a keystroke and DON'T quit pilot.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if focus_in_terminal {
            // Fall through to the terminal-stack handler below.
        } else {
            return true;
        }
    }

    // q-q double-tap: first `q` outside a terminal arms the latch;
    // a second `q` within Q_DOUBLE_TAP_WINDOW quits. Anything else
    // disarms.  Inside a terminal, `q` is just a key the shell
    // wants, so we don't intercept.
    if !focus_in_terminal && key.code == KeyCode::Char('q') && key.modifiers == KeyModifiers::NONE {
        let now = Instant::now();
        if let Some(armed_at) = state.q_armed_at {
            if now.duration_since(armed_at) <= Q_DOUBLE_TAP_WINDOW {
                return true;
            }
        }
        state.q_armed_at = Some(now);
        state.last_error = Some("press q again to quit".into());
        return false;
    } else {
        // Any other key disarms.
        state.q_armed_at = None;
    }

    if !focus_in_terminal && key.code == KeyCode::Char('?') && key.modifiers == KeyModifiers::NONE {
        mount_help(tree, ids);
        return false;
    }

    // Pane resize: Shift+arrows move the splitters. Sized so a few
    // taps actually moves things — 2 cols / 5% per tap. Works in any
    // pane focus EXCEPT inside a terminal (the shell may bind these).
    if !focus_in_terminal && key.modifiers.contains(KeyModifiers::SHIFT) {
        let next = match key.code {
            KeyCode::Left => Some(state.layout.nudge(-2, 0)),
            KeyCode::Right => Some(state.layout.nudge(2, 0)),
            KeyCode::Up => Some(state.layout.nudge(0, -5)),
            KeyCode::Down => Some(state.layout.nudge(0, 5)),
            _ => None,
        };
        if let Some(layout) = next {
            if layout != state.layout {
                state.layout = layout;
                state.layout_dirty = true;
            }
            return false;
        }
    }

    // `Tab` cycles focus among top-level panes, unless we're in a
    // terminal (where Tab is a key the shell wants).
    if !focus_in_terminal && key.code == KeyCode::Tab && key.modifiers == KeyModifiers::NONE {
        tree.focus_next_sibling();
        return false;
    }

    let cmds = tree.handle_key(key);
    for cmd in cmds {
        let _ = client.send(cmd);
    }
    false
}

/// Build the layout tree for the current pilot state.
///
/// Pure function from `(layout, has_session, has_terminal)` to a
/// `Node`. Both `draw` and `dispatch_mouse` build their own copy so
/// they always agree about what's where; the cached
/// `last_layout_outcome` short-circuits a re-resolve in
/// `dispatch_mouse`.
pub fn build_layout(
    ids: &Ids,
    layout: pilot_core::PaneLayout,
    has_session: bool,
    has_terminal: bool,
) -> Node {
    let right = if has_session || has_terminal {
        Node::v_split(
            Some(SplitterId::RightVertical),
            VSizing::TopPct(layout.right_top_pct.min(100)),
            Node::Component(ids.right),
            Node::Component(ids.terminals),
        )
    } else {
        Node::Placeholder(Placeholder::EmptyRight)
    };
    Node::h_split(
        Some(SplitterId::Sidebar),
        HSizing::LeftFixed(layout.sidebar_width),
        Node::Component(ids.sidebar),
        right,
    )
}

/// Route a mouse event. Mostly we use it for splitter resize:
///
/// - Down on a splitter → start drag (and unfocus components, so the
///   user's mid-drag keystrokes don't leak into a pane).
/// - Drag while dragging → update PaneLayout, mark dirty.
/// - Up → release.
///
/// We deliberately don't forward clicks to components yet — clicking
/// the sidebar to select a row is a future enhancement; today the
/// sidebar uses j/k. Mouse-wheel scroll on the terminal will be
/// wired in the scrollback commit (#91).
pub fn dispatch_mouse(
    me: crossterm::event::MouseEvent,
    tree: &mut ComponentTree,
    ids: &Ids,
    state: &mut AppState,
) {
    use crossterm::event::{MouseButton, MouseEventKind};

    let (frame_w, frame_h) = state.last_frame;
    if frame_w == 0 || frame_h == 0 {
        return; // no frame drawn yet
    }

    // Recompute the same main-area carve-out `draw` did. The hint
    // bar is always 1 row, so subtract that unconditionally to keep
    // hit-test geometry aligned with what `draw` actually rendered.
    let main_h = frame_h.saturating_sub(1);
    let main_area = ratatui::layout::Rect {
        x: 0,
        y: 0,
        width: frame_w,
        height: main_h,
    };

    // Resolve the layout against the same area `draw` rendered into.
    // Hit-test queries against this — single source of truth.
    let has_session = tree
        .get::<Sidebar>(ids.sidebar)
        .and_then(|s| s.selected_session_key().cloned())
        .is_some();
    let has_terminal = tree
        .get::<TerminalStack>(ids.terminals)
        .map(|t| !t.visible_terminals().is_empty())
        .unwrap_or(false);
    let outcome = build_layout(ids, state.layout, has_session, has_terminal).resolve(main_area);

    match me.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            tracing::debug!(col = me.column, row = me.row, "mouse down");
            if let Some(sid) = layout::hit_test(&outcome, me.column, me.row, 1) {
                let drag = Drag::from_splitter(sid);
                state.dragging = Some(drag);
                tree.unfocus();
                state.last_error = Some(match drag {
                    Drag::Sidebar => "dragging sidebar splitter".into(),
                    Drag::RightSplit => "dragging activity / terminal splitter".into(),
                });
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let Some(d) = state.dragging else { return };
            let new_layout = match d {
                Drag::Sidebar => pilot_core::PaneLayout {
                    sidebar_width: me.column,
                    right_top_pct: state.layout.right_top_pct,
                }
                .clamp(),
                Drag::RightSplit => {
                    let pct = if main_h > 0 {
                        ((me.row as u32 * 100) / main_h as u32) as u16
                    } else {
                        state.layout.right_top_pct
                    };
                    pilot_core::PaneLayout {
                        sidebar_width: state.layout.sidebar_width,
                        right_top_pct: pct,
                    }
                    .clamp()
                }
            };
            if new_layout != state.layout {
                state.layout = new_layout;
                state.layout_dirty = true;
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            state.dragging = None;
            state.last_error = None;
        }
        MouseEventKind::Moved => {
            if state.dragging.is_some() {
                return;
            }
            state.hovering_splitter =
                layout::hit_test(&outcome, me.column, me.row, 1).map(Drag::from_splitter);
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            // Trackpad / wheel scroll. Forward to the terminal stack
            // when the cursor is over the terminal pane so users can
            // scroll back through the agent's output. We deliberately
            // gate on hover (not focus) because trackpad scroll is a
            // pointing-device gesture — focus shouldn't have to follow.
            let over_terminal = outcome.slots.iter().any(|slot| {
                let Slot::Component(cid, rect) = slot else {
                    return false;
                };
                *cid == ids.terminals && point_in_rect(me.column, me.row, *rect)
            });
            if !over_terminal {
                return;
            }
            // 3-row notch per gesture is what `less` and most terminals
            // use; libghostty's Delta is signed (negative = scroll up
            // into history).
            const STEP: isize = 3;
            let delta = if matches!(me.kind, MouseEventKind::ScrollUp) {
                -STEP
            } else {
                STEP
            };
            if let Some(ts) = tree.get_mut::<TerminalStack>(ids.terminals) {
                ts.scroll_active(delta);
            }
        }
        _ => {}
    }
    let _ = ids;
}

fn point_in_rect(x: u16, y: u16, rect: ratatui::layout::Rect) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

/// Route a bracketed-paste payload to the focused terminal as a
/// single `Command::Write`. If the focus isn't on the terminal stack,
/// or there's no active terminal, the paste is dropped — pasting
/// into the sidebar should never accidentally send keystrokes to a
/// background agent.
pub fn dispatch_paste(text: String, tree: &mut ComponentTree, ids: &Ids, client: &mut Client) {
    if tree.focused() != Some(ids.terminals) {
        return;
    }
    let Some(stack) = tree.get::<TerminalStack>(ids.terminals) else {
        return;
    };
    let Some(terminal_id) = stack.active_terminal_id() else {
        return;
    };
    let _ = client.send(Command::Write {
        terminal_id,
        bytes: text.into_bytes(),
    });
}

pub fn mount_help(tree: &mut ComponentTree, ids: &Ids) {
    // Don't stack multiple Help overlays.
    for child in tree.children_of(ids.root).to_vec() {
        if tree.get::<Help>(child).is_some() {
            return;
        }
    }
    let help_id = tree.alloc_id();
    let _ = tree.mount_child(ids.root, Box::new(Help::default_help(help_id)));
    tree.set_focus(help_id);
}

/// Some daemon events need side-effects beyond broadcasting: a newly
/// spawned terminal should pull focus so the user can type into it.
pub fn handle_ipc_side_effects(tree: &mut ComponentTree, ids: &Ids, event: &Event) {
    if let Event::TerminalSpawned { .. } = event {
        tree.set_focus(ids.terminals);
    }
}

/// Keep the right pane + terminal stack aligned with whatever the
/// sidebar has selected. Cheap to call every tick; the setters are
/// idempotent and bail early if nothing changed.
///
/// If the newly-selected session has no terminals yet AND we haven't
/// auto-spawned for it before, fire one shell automatically. The user
/// flow we're optimizing for: open a session → start working. Making
/// the user press `s` first when there's nothing else to do is just
/// extra friction. They can still close it; we won't resurrect.
pub fn sync_panes(tree: &mut ComponentTree, ids: &Ids, state: &mut AppState, client: &mut Client) {
    let (selected_workspace, selected_key) = {
        let sb = tree
            .get::<Sidebar>(ids.sidebar)
            .expect("sidebar mounted at start");
        (
            sb.selected_workspace().cloned(),
            sb.selected_session_key().cloned(),
        )
    };
    if let Some(rp) = tree.get_mut::<RightPane>(ids.right) {
        rp.set_workspace(selected_workspace);
    }
    if let Some(ts) = tree.get_mut::<TerminalStack>(ids.terminals) {
        ts.set_active_session(selected_key.clone());
    }

    // Auto-spawn a shell for empty sessions on first selection.
    if let Some(key) = selected_key
        && !state.auto_spawned.contains(&key)
    {
        let has_any = tree
            .get::<TerminalStack>(ids.terminals)
            .map(|t| !t.visible_terminals().is_empty())
            .unwrap_or(false);
        if !has_any {
            state.auto_spawned.insert(key.clone());
            // session_id None → daemon picks/auto-creates the
            // workspace's default session and roots the spawn there.
            let _ = client.send(Command::Spawn {
                session_key: key,
                session_id: None,
                kind: pilot_v2_ipc::TerminalKind::Shell,
                cwd: None,
            });
        }
    }
}

/// Build the bottom hint row. Errors / q-quit prompts win; otherwise
/// shortcuts that are *actually applicable right now* given which
/// pane has focus and what's on screen. The goal is "the user can
/// look down once and learn what they can do next" without overlays.
fn build_hint_bar(
    tree: &ComponentTree,
    ids: &Ids,
    state: &AppState,
    has_session: bool,
) -> Line<'static> {
    if let Some(err) = state.last_error.as_deref() {
        return Line::from(vec![Span::styled(
            format!(" {err}"),
            Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        )]);
    }
    if state.q_armed_at.is_some() {
        return Line::from(vec![Span::styled(
            " press q again to quit",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]);
    }

    let focused = tree.focused();
    let in_terminal = focused == Some(ids.terminals);
    let visible_terms = tree
        .get::<TerminalStack>(ids.terminals)
        .map(|t| t.visible_terminals().len())
        .unwrap_or(0);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let push = |spans: &mut Vec<Span<'static>>, key: &str, label: &str| {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            key.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            label.to_string(),
            Style::default().fg(Color::Gray),
        ));
    };

    spans.push(Span::raw(" "));
    if in_terminal {
        // Inside a terminal there is exactly ONE meta-shortcut: the
        // escape sequence (default `]]]`). Everything else flows to
        // the agent. `?` and quit are reachable by escaping first.
        let label: String = std::iter::repeat_n(
            state.escape_cfg.escape_char,
            state.escape_cfg.escape_count.max(2) as usize,
        )
        .collect();
        push(&mut spans, &label, "back to inbox");
        let _ = visible_terms;
    } else if has_session {
        push(&mut spans, "s", "shell");
        push(&mut spans, "c", "claude");
        push(&mut spans, "x", "codex");
        push(&mut spans, "u", "cursor");
        push(&mut spans, "j/k", "navigate");
        push(&mut spans, "?", "help");
        push(&mut spans, "q q", "quit");
    } else {
        push(&mut spans, "j/k", "navigate");
        push(&mut spans, "?", "help");
        push(&mut spans, "q q", "quit");
    }

    Line::from(spans)
}

fn draw(
    term: &mut Terminal<AppBackend>,
    tree: &mut ComponentTree,
    ids: &Ids,
    state: &mut AppState,
) -> anyhow::Result<()> {
    // Compute layout decisions BEFORE the closure so we don't borrow
    // tree both immutably (for `get`) and mutably (for `render_one`)
    // at the same time inside `term.draw`.
    let has_session = tree
        .get::<Sidebar>(ids.sidebar)
        .map(|s| s.selected_workspace().is_some())
        .unwrap_or(false);
    let has_terminal = tree
        .get::<TerminalStack>(ids.terminals)
        .map(|t| !t.visible_terminals().is_empty())
        .unwrap_or(false);

    term.draw(|frame| {
        let area = frame.area();
        state.last_frame = (area.width, area.height);

        // Bottom hint bar (1 row). Errors and the q-quit prompt take
        // priority; otherwise we render context-aware shortcuts that
        // tell the user what `s`, `c`, etc. do RIGHT NOW. No rebound:
        // the row is always reserved so the layout doesn't jump.
        let hint = build_hint_bar(tree, ids, state, has_session);
        let split = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
        let (main_area, status_area) = (split[0], Some(split[1]));

        // Build the layout tree for this frame's state and resolve
        // it into concrete rects. Same outcome `dispatch_mouse`
        // computes for hit-testing.
        let outcome = build_layout(ids, state.layout, has_session, has_terminal).resolve(main_area);

        // Walk the slot list in order and render each leaf.
        for slot in &outcome.slots {
            match slot {
                Slot::Component(id, rect) => tree.render_one(*id, *rect, frame),
                Slot::Placeholder(Placeholder::EmptyRight, rect) => {
                    render_empty_right(frame, *rect)
                }
            }
        }

        // ── Splitter highlight overlay ────────────────────────────
        // Paint a 1-cell-thick band on whichever splitter is hovered
        // or being dragged. We use a SUBTLE background tint (instead
        // of bold foreground, which can read as text selection) so
        // the highlight is visible without being mistaken for
        // selected text.
        let active = state.dragging.or(state.hovering_splitter);
        if let Some(drag) = active {
            if let Some(rect) = outcome.splitter_rect(drag.to_splitter()) {
                let style = if state.dragging.is_some() {
                    Style::default().bg(Color::Cyan).fg(Color::Black)
                } else {
                    Style::default().bg(Color::DarkGray).fg(Color::Gray)
                };
                let glyph = if rect.width == 1 {
                    "│".repeat(rect.height as usize)
                } else {
                    "─".repeat(rect.width as usize)
                };
                let lines: Vec<Line> = if rect.width == 1 {
                    glyph.chars().map(|c| Line::raw(c.to_string())).collect()
                } else {
                    vec![Line::raw(glyph)]
                };
                frame.render_widget(Paragraph::new(lines).style(style), rect);
            }
        }

        if let Some(rect) = status_area {
            frame.render_widget(Paragraph::new(hint), rect);
        }

        // Overlays mounted as root children render last (full screen,
        // overlay handles its own modal sizing).
        for child in tree.children_of(ids.root).to_vec() {
            if child == ids.sidebar || child == ids.right || child == ids.terminals {
                continue;
            }
            tree.render_one(child, area, frame);
            break;
        }
    })?;
    Ok(())
}

/// Empty-state placeholder for the right column when no session is
/// selected and no terminal is open. Helps the user orient on
/// first launch (or in `--test`).
fn render_empty_right(frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "  Pilot is ready.",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  · Select a workspace in the sidebar to see its activity.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "  · Press c / C / b on a workspace to spawn an agent or shell.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "  · Press ? for help, q q to quit.",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}
