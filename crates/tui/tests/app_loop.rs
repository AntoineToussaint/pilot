//! Tests for the live render loop's pure logic. These exercise the
//! key-dispatch / focus-cycle / overlay-mount paths that `app::run`
//! glues to a real terminal — without ever touching `enable_raw_mode`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pilot_v2_ipc::{Event, TerminalId, TerminalKind, channel};
use pilot_v2_tui::app;
use pilot_v2_tui::components::{Help, RightPane, Sidebar, TerminalStack};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

#[test]
fn build_tree_mounts_three_top_level_children() {
    let (tree, ids) = app::build_tree();
    let kids = tree.children_of(ids.root);
    assert_eq!(kids.len(), 3);
    assert!(kids.contains(&ids.sidebar));
    assert!(kids.contains(&ids.right));
    assert!(kids.contains(&ids.terminals));
    assert_eq!(tree.focused(), Some(ids.sidebar));
    assert!(tree.get::<Sidebar>(ids.sidebar).is_some());
    assert!(tree.get::<RightPane>(ids.right).is_some());
    assert!(tree.get::<TerminalStack>(ids.terminals).is_some());
}

#[test]
fn single_q_arms_but_does_not_quit() {
    // First `q` arms the latch and surfaces a status hint;
    // a second `q` within the window quits.
    let (mut tree, ids) = app::build_tree();
    let (mut client, _server) = channel::pair();
    let mut state = app::AppState::new();
    let quit = app::dispatch_key(
        key(KeyCode::Char('q')),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );
    assert!(!quit, "single q is armed, not quit");
    assert!(state.q_armed_at.is_some(), "latch is armed");
    assert_eq!(
        state.last_error.as_deref(),
        Some("press q again to quit"),
        "user gets a hint"
    );
}

#[test]
fn double_q_quits_when_focus_is_on_sidebar() {
    let (mut tree, ids) = app::build_tree();
    let (mut client, _server) = channel::pair();
    let mut state = app::AppState::new();
    app::dispatch_key(
        key(KeyCode::Char('q')),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );
    let quit = app::dispatch_key(
        key(KeyCode::Char('q')),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );
    assert!(quit, "second q within window quits");
}

#[test]
fn other_key_between_q_presses_disarms_the_latch() {
    let (mut tree, ids) = app::build_tree();
    let (mut client, _server) = channel::pair();
    let mut state = app::AppState::new();
    app::dispatch_key(
        key(KeyCode::Char('q')),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );
    // An unrelated key disarms.
    app::dispatch_key(
        key(KeyCode::Char('j')),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );
    assert!(state.q_armed_at.is_none(), "intermediate key disarmed");
    let quit = app::dispatch_key(
        key(KeyCode::Char('q')),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );
    assert!(!quit, "second q is a fresh first-press, not a confirm");
}

#[test]
fn q_does_not_quit_when_focus_is_on_terminal() {
    // When the user has tab'd into the terminal stack, `q` is just a
    // character the shell should receive — not a quit. This is the bug
    // class v1 hit constantly: global keys leaking into terminal mode.
    let (mut tree, ids) = app::build_tree();
    tree.set_focus(ids.terminals);
    let (mut client, _server) = channel::pair();
    let mut state = app::AppState::new();
    let quit = app::dispatch_key(
        key(KeyCode::Char('q')),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );
    assert!(!quit, "q in terminal must not quit");
}

#[test]
fn ctrl_c_outside_terminal_quits() {
    // Sidebar / right pane don't need Ctrl-C — it's a quit shortcut
    // matching standard terminal convention.
    let (mut tree, ids) = app::build_tree();
    assert_eq!(tree.focused(), Some(ids.sidebar));
    let (mut client, _server) = channel::pair();
    let mut state = app::AppState::new();
    let quit = app::dispatch_key(
        ctrl(KeyCode::Char('c')),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );
    assert!(quit, "Ctrl-C outside terminal quits");
}

#[test]
fn ctrl_c_in_terminal_routes_to_process_not_quit() {
    // Inside the terminal Ctrl-C is the SIGINT keystroke — needed for
    // interrupting `cat`, killing builds, escaping Claude prompts.
    // Pilot must NOT quit and must forward the byte to the agent PTY.
    let (mut tree, ids) = app::build_tree();
    tree.set_focus(ids.terminals);
    let (mut client, _server) = channel::pair();
    let mut state = app::AppState::new();
    let quit = app::dispatch_key(
        ctrl(KeyCode::Char('c')),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );
    assert!(!quit, "Ctrl-C in terminal must not quit pilot");
}

#[test]
fn tab_cycles_focus_among_top_panes() {
    let (mut tree, ids) = app::build_tree();
    let (mut client, _server) = channel::pair();
    let mut state = app::AppState::new();
    assert_eq!(tree.focused(), Some(ids.sidebar));
    app::dispatch_key(key(KeyCode::Tab), &mut tree, &ids, &mut state, &mut client);
    assert_eq!(tree.focused(), Some(ids.right));
    app::dispatch_key(key(KeyCode::Tab), &mut tree, &ids, &mut state, &mut client);
    assert_eq!(tree.focused(), Some(ids.terminals));
}

#[test]
fn tab_in_terminal_stays_in_terminal() {
    // Tab is essential for shell autocomplete and Claude's prompt
    // navigation — pilot must NOT use it to cycle focus out of the
    // terminal. Single escape sequence is the configurable `]]]`.
    let (mut tree, ids) = app::build_tree();
    tree.set_focus(ids.terminals);
    let (mut client, _server) = channel::pair();
    let mut state = app::AppState::new();
    app::dispatch_key(key(KeyCode::Tab), &mut tree, &ids, &mut state, &mut client);
    assert_eq!(
        tree.focused(),
        Some(ids.terminals),
        "Tab stays in terminal so the agent can autocomplete"
    );
}

// ── Terminal escape sequence (default `]]]`) ──────────────────────────

#[test]
fn three_brackets_in_terminal_focus_sidebar() {
    let (mut tree, ids) = app::build_tree();
    tree.set_focus(ids.terminals);
    let (mut client, _server) = channel::pair();
    let mut state = app::AppState::new();
    for _ in 0..3 {
        app::dispatch_key(
            key(KeyCode::Char(']')),
            &mut tree,
            &ids,
            &mut state,
            &mut client,
        );
    }
    assert_eq!(
        tree.focused(),
        Some(ids.sidebar),
        "]]] returns focus to the sidebar"
    );
    assert!(state.escape_buffer.is_empty(), "buffer cleared on trigger");
}

#[test]
fn brackets_outside_terminal_pass_through() {
    // The escape latch only runs when focus is on the terminal. In
    // the sidebar, `]` is just a key (currently unmapped → no-op).
    let (mut tree, ids) = app::build_tree();
    assert_eq!(tree.focused(), Some(ids.sidebar));
    let (mut client, _server) = channel::pair();
    let mut state = app::AppState::new();
    for _ in 0..5 {
        app::dispatch_key(
            key(KeyCode::Char(']')),
            &mut tree,
            &ids,
            &mut state,
            &mut client,
        );
    }
    assert_eq!(tree.focused(), Some(ids.sidebar), "stayed in sidebar");
    assert!(state.escape_buffer.is_empty());
}

#[test]
fn unit_advance_escape_latch_buffers_then_triggers() {
    // Pure-function test of the latch state machine — no tree, no
    // client. Three matching keys → Triggered; buffer empty after.
    let mut state = app::AppState::new();
    let bracket = key(KeyCode::Char(']'));
    assert_eq!(
        app::advance_escape_latch(&bracket, &mut state),
        app::EscapeOutcome::Buffered
    );
    assert_eq!(
        app::advance_escape_latch(&bracket, &mut state),
        app::EscapeOutcome::Buffered
    );
    assert_eq!(
        app::advance_escape_latch(&bracket, &mut state),
        app::EscapeOutcome::Triggered
    );
    assert!(state.escape_buffer.is_empty());
}

#[test]
fn unit_non_match_after_brackets_flushes() {
    // User typed `]]x` quickly. The two `]`s were buffered; now `x`
    // breaks the run — caller should see `Flush(buf)` so it can
    // forward the buffered chars to the agent before processing `x`.
    let mut state = app::AppState::new();
    let bracket = key(KeyCode::Char(']'));
    app::advance_escape_latch(&bracket, &mut state);
    app::advance_escape_latch(&bracket, &mut state);
    let outcome = app::advance_escape_latch(&key(KeyCode::Char('x')), &mut state);
    match outcome {
        app::EscapeOutcome::Flush(buf) => assert_eq!(buf.len(), 2),
        other => panic!("expected Flush, got {other:?}"),
    }
    assert!(state.escape_buffer.is_empty());
}

#[test]
fn unit_lone_bracket_does_not_trigger_or_pass() {
    // A single `]` should buffer but NOT pass through yet — we don't
    // know if the user is starting a sequence. The buffered char will
    // flush on the next non-`]` key (or on timeout).
    let mut state = app::AppState::new();
    assert_eq!(
        app::advance_escape_latch(&key(KeyCode::Char(']')), &mut state),
        app::EscapeOutcome::Buffered
    );
    assert_eq!(state.escape_buffer.len(), 1);
}

#[test]
fn unit_unrelated_key_with_empty_buffer_passes() {
    let mut state = app::AppState::new();
    assert_eq!(
        app::advance_escape_latch(&key(KeyCode::Char('q')), &mut state),
        app::EscapeOutcome::Pass
    );
}

#[test]
fn ordinary_key_is_not_swallowed_as_quit_when_focus_is_on_terminal() {
    // The whole point of the focus-aware quit gate: 'q' must reach the
    // tree (and ultimately become a Write command) instead of exiting
    // the app.
    let (mut tree, ids) = app::build_tree();
    tree.set_focus(ids.terminals);
    let (mut client, _server) = channel::pair();
    let mut state = app::AppState::new();
    let quit = app::dispatch_key(
        key(KeyCode::Char('q')),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );
    assert!(!quit, "q reaches the tree, doesn't quit");
}

#[test]
fn question_mark_mounts_help_overlay() {
    let (mut tree, ids) = app::build_tree();
    let (mut client, _server) = channel::pair();
    let mut state = app::AppState::new();
    app::dispatch_key(
        key(KeyCode::Char('?')),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );

    let help_child = tree
        .children_of(ids.root)
        .iter()
        .copied()
        .find(|c| tree.get::<Help>(*c).is_some())
        .expect("Help mounted");
    assert_eq!(tree.focused(), Some(help_child), "focus moves to Help");
}

#[test]
fn question_mark_does_not_stack_multiple_help_overlays() {
    let (mut tree, ids) = app::build_tree();
    let (mut client, _server) = channel::pair();
    let mut state = app::AppState::new();
    app::dispatch_key(
        key(KeyCode::Char('?')),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );
    app::dispatch_key(
        key(KeyCode::Char('?')),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );

    let count = tree
        .children_of(ids.root)
        .iter()
        .filter(|c| tree.get::<Help>(**c).is_some())
        .count();
    assert_eq!(count, 1, "duplicate Help mounts are suppressed");
}

#[test]
fn terminal_spawned_event_pulls_focus_to_terminal_stack() {
    let (mut tree, ids) = app::build_tree();
    assert_eq!(tree.focused(), Some(ids.sidebar));
    let event = Event::TerminalSpawned {
        terminal_id: TerminalId(1),
        session_key: "github:o/r#1".into(),
        kind: TerminalKind::Shell,
    };
    app::handle_ipc_side_effects(&mut tree, &ids, &event);
    assert_eq!(
        tree.focused(),
        Some(ids.terminals),
        "newly spawned terminal claims focus"
    );
}

#[test]
fn forwarded_key_emits_command_to_client() {
    // The sidebar's `g` key emits `Command::Refresh`. Dispatching it
    // should forward that command through the client to the daemon.
    let (mut tree, ids) = app::build_tree();
    let (mut client, mut server) = channel::pair();
    let mut state = app::AppState::new();
    app::dispatch_key(
        key(KeyCode::Char('g')),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );

    let cmd = server.rx.try_recv().expect("a command was sent");
    assert!(matches!(cmd, pilot_v2_ipc::Command::Refresh));
}

// ── Pane layout resize ──────────────────────────────────────────────

fn shift(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::SHIFT)
}

#[test]
fn shift_left_shrinks_sidebar_and_marks_dirty() {
    let (mut tree, ids) = app::build_tree();
    let (mut client, _server) = channel::pair();
    let mut state = app::AppState::new();
    let before = state.layout.sidebar_width;
    app::dispatch_key(
        shift(KeyCode::Left),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );
    assert!(state.layout.sidebar_width < before, "sidebar shrunk");
    assert!(state.layout_dirty, "layout marked dirty for persistence");
}

#[test]
fn shift_right_grows_sidebar() {
    let (mut tree, ids) = app::build_tree();
    let (mut client, _server) = channel::pair();
    let mut state = app::AppState::new();
    let before = state.layout.sidebar_width;
    app::dispatch_key(
        shift(KeyCode::Right),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );
    assert!(state.layout.sidebar_width > before, "sidebar grew");
}

#[test]
fn shift_up_down_resize_right_split() {
    let (mut tree, ids) = app::build_tree();
    let (mut client, _server) = channel::pair();
    let mut state = app::AppState::new();
    let before = state.layout.right_top_pct;
    app::dispatch_key(
        shift(KeyCode::Down),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );
    assert!(state.layout.right_top_pct > before, "activity pane grew");
    let mid = state.layout.right_top_pct;
    app::dispatch_key(shift(KeyCode::Up), &mut tree, &ids, &mut state, &mut client);
    assert!(state.layout.right_top_pct < mid, "activity pane shrank");
}

#[test]
fn shift_arrows_inside_terminal_fall_through_to_component() {
    // We don't intercept Shift+arrows when focus is on the terminal —
    // those keys are reserved for the shell / agent. Resize bindings
    // only fire from non-terminal panes.
    let (mut tree, ids) = app::build_tree();
    tree.set_focus(ids.terminals);
    let (mut client, _server) = channel::pair();
    let mut state = app::AppState::new();
    let before = state.layout.sidebar_width;
    app::dispatch_key(
        shift(KeyCode::Left),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );
    assert_eq!(
        state.layout.sidebar_width, before,
        "in-terminal Shift+Left does not resize"
    );
    assert!(!state.layout_dirty, "no persist when nothing changed");
}

#[test]
fn layout_persistence_round_trips_through_kv() {
    use pilot_core::{KV_KEY_LAYOUT, PaneLayout};
    use pilot_store::{MemoryStore, Store};
    let store: std::sync::Arc<dyn Store> = std::sync::Arc::new(MemoryStore::new());
    let custom = PaneLayout {
        sidebar_width: 48,
        right_top_pct: 50,
    };
    store
        .set_kv(KV_KEY_LAYOUT, &serde_json::to_string(&custom).unwrap())
        .unwrap();

    let s = app::AppState::with_store(Some(&*store));
    assert_eq!(s.layout, custom, "AppState loads layout from kv");
}

#[test]
fn layout_load_falls_back_to_default_on_corrupt_kv() {
    use pilot_core::KV_KEY_LAYOUT;
    use pilot_store::{MemoryStore, Store};
    let store: std::sync::Arc<dyn Store> = std::sync::Arc::new(MemoryStore::new());
    store.set_kv(KV_KEY_LAYOUT, "{not-valid").unwrap();
    let s = app::AppState::with_store(Some(&*store));
    assert_eq!(
        s.layout,
        pilot_core::PaneLayout::DEFAULT,
        "corrupt kv → default layout"
    );
}

// ── Mouse resize ────────────────────────────────────────────────────

fn mouse(
    kind: crossterm::event::MouseEventKind,
    col: u16,
    row: u16,
) -> crossterm::event::MouseEvent {
    crossterm::event::MouseEvent {
        kind,
        column: col,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn mouse_down_on_sidebar_splitter_starts_drag_and_unfocuses() {
    use crossterm::event::{MouseButton, MouseEventKind};
    let (mut tree, ids) = app::build_tree();
    let mut state = app::AppState::new();
    state.last_frame = (120, 40);
    // Sidebar splitter is at column == sidebar_width.
    let sx = state.layout.sidebar_width;
    app::dispatch_mouse(
        mouse(MouseEventKind::Down(MouseButton::Left), sx, 5),
        &mut tree,
        &ids,
        &mut state,
    );
    assert_eq!(state.dragging, Some(app::Drag::Sidebar));
    assert!(tree.is_unfocused(), "drag clears component focus");
}

#[test]
fn mouse_drag_resizes_sidebar_and_marks_dirty() {
    use crossterm::event::{MouseButton, MouseEventKind};
    let (mut tree, ids) = app::build_tree();
    let mut state = app::AppState::new();
    state.last_frame = (120, 40);
    state.dragging = Some(app::Drag::Sidebar);
    let before = state.layout.sidebar_width;
    app::dispatch_mouse(
        mouse(MouseEventKind::Drag(MouseButton::Left), before + 6, 5),
        &mut tree,
        &ids,
        &mut state,
    );
    assert_eq!(state.layout.sidebar_width, before + 6);
    assert!(state.layout_dirty);
}

#[test]
fn mouse_up_clears_drag() {
    use crossterm::event::{MouseButton, MouseEventKind};
    let (mut tree, ids) = app::build_tree();
    let mut state = app::AppState::new();
    state.last_frame = (120, 40);
    state.dragging = Some(app::Drag::Sidebar);
    app::dispatch_mouse(
        mouse(MouseEventKind::Up(MouseButton::Left), 50, 5),
        &mut tree,
        &ids,
        &mut state,
    );
    assert!(state.dragging.is_none());
}

/// Inject a workspace into the sidebar so build_layout produces a
/// VSplit with the RightVertical splitter (required for the right
/// splitter to be draggable — empty layouts have no right splitter).
fn seed_one_workspace(tree: &mut pilot_v2_tui::ComponentTree, ids: &app::Ids) {
    use chrono::Utc;
    use pilot_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace};
    let task = Task {
        id: TaskId {
            source: "test".into(),
            key: "o/r#1".into(),
        },
        title: "test".into(),
        body: None,
        state: TaskState::Open,
        role: TaskRole::Author,
        ci: CiStatus::None,
        review: ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: "https://x".into(),
        repo: Some("o/r".into()),
        branch: None,
        base_branch: None,
        updated_at: Utc::now(),
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
    let workspace = Workspace::from_task(task, Utc::now());
    tree.broadcast_event(&Event::WorkspaceUpserted(Box::new(workspace)));
    let _ = ids;
}

#[test]
fn mouse_drag_on_right_split_resizes_right_top_pct() {
    use crossterm::event::{MouseButton, MouseEventKind};
    let (mut tree, ids) = app::build_tree();
    let mut state = app::AppState::new();
    seed_one_workspace(&mut tree, &ids);
    // 100-row main area so percentages are easy to verify.
    state.last_frame = (200, 100);
    let sx = state.layout.sidebar_width;
    let split_y = state.layout.right_top_pct;
    app::dispatch_mouse(
        mouse(MouseEventKind::Down(MouseButton::Left), sx + 10, split_y),
        &mut tree,
        &ids,
        &mut state,
    );
    assert_eq!(state.dragging, Some(app::Drag::RightSplit));
    // Drag down to row 50 → 50% activity pane.
    app::dispatch_mouse(
        mouse(MouseEventKind::Drag(MouseButton::Left), sx + 10, 50),
        &mut tree,
        &ids,
        &mut state,
    );
    assert_eq!(state.layout.right_top_pct, 50);
}

#[test]
fn right_splitter_is_absent_when_no_session_or_terminal() {
    // With nothing selected the right column is a placeholder (no
    // VSplit), so there's no right splitter to grab. This is the
    // layout tree's correctness invariant — splitters only exist
    // where there's something to split.
    use crossterm::event::{MouseButton, MouseEventKind};
    let (mut tree, ids) = app::build_tree();
    let mut state = app::AppState::new();
    state.last_frame = (200, 100);
    let sx = state.layout.sidebar_width;
    let split_y = state.layout.right_top_pct;
    app::dispatch_mouse(
        mouse(MouseEventKind::Down(MouseButton::Left), sx + 10, split_y),
        &mut tree,
        &ids,
        &mut state,
    );
    assert!(
        state.dragging.is_none(),
        "no right splitter when there's no session/terminal"
    );
}

#[test]
fn mouse_down_off_splitter_is_inert() {
    use crossterm::event::{MouseButton, MouseEventKind};
    let (mut tree, ids) = app::build_tree();
    let mut state = app::AppState::new();
    state.last_frame = (200, 50);
    // Click in the middle of the sidebar (not on the boundary).
    app::dispatch_mouse(
        mouse(MouseEventKind::Down(MouseButton::Left), 5, 5),
        &mut tree,
        &ids,
        &mut state,
    );
    assert!(state.dragging.is_none());
    assert_eq!(tree.focused(), Some(ids.sidebar), "no focus change");
}

#[test]
fn mouse_hover_on_splitter_records_hovering_state() {
    use crossterm::event::MouseEventKind;
    let (mut tree, ids) = app::build_tree();
    let mut state = app::AppState::new();
    state.last_frame = (200, 50);
    let sx = state.layout.sidebar_width;
    app::dispatch_mouse(
        mouse(MouseEventKind::Moved, sx, 5),
        &mut tree,
        &ids,
        &mut state,
    );
    assert_eq!(state.hovering_splitter, Some(app::Drag::Sidebar));

    // Move away → hover clears.
    app::dispatch_mouse(
        mouse(MouseEventKind::Moved, sx + 20, 5),
        &mut tree,
        &ids,
        &mut state,
    );
    assert!(state.hovering_splitter.is_none());
}

#[test]
fn mouse_hit_zone_is_one_column_wide_on_each_side() {
    // Splitter is "tall and thin" — a one-pixel band feels hostile,
    // so we accept ±1 columns. This test pins that contract.
    use crossterm::event::{MouseButton, MouseEventKind};
    let (mut tree, ids) = app::build_tree();
    let mut state = app::AppState::new();
    state.last_frame = (200, 50);
    let sx = state.layout.sidebar_width;
    for col in [sx - 1, sx, sx + 1] {
        state.dragging = None;
        app::dispatch_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), col, 5),
            &mut tree,
            &ids,
            &mut state,
        );
        assert_eq!(
            state.dragging,
            Some(app::Drag::Sidebar),
            "col {col} should register as splitter"
        );
    }
    // Two cols away → no hit.
    state.dragging = None;
    app::dispatch_mouse(
        mouse(MouseEventKind::Down(MouseButton::Left), sx - 3, 5),
        &mut tree,
        &ids,
        &mut state,
    );
    assert!(state.dragging.is_none(), "col sx-3 is outside the band");
}

#[test]
fn mouse_with_no_recorded_frame_is_ignored() {
    use crossterm::event::{MouseButton, MouseEventKind};
    // Mouse events that arrive before the first paint have no
    // splitter coordinates to test against — drop them silently.
    let (mut tree, ids) = app::build_tree();
    let mut state = app::AppState::new();
    assert_eq!(state.last_frame, (0, 0));
    app::dispatch_mouse(
        mouse(MouseEventKind::Down(MouseButton::Left), 32, 5),
        &mut tree,
        &ids,
        &mut state,
    );
    assert!(state.dragging.is_none());
}

// ── ComponentTree focus = Option<ComponentId> ──────────────────────

#[test]
fn unfocus_clears_focus_to_none() {
    let (mut tree, _ids) = app::build_tree();
    assert!(tree.focused().is_some());
    tree.unfocus();
    assert!(tree.is_unfocused());
    assert!(tree.focused().is_none());
}

#[test]
fn key_dispatch_with_no_focus_emits_no_commands() {
    let (mut tree, ids) = app::build_tree();
    tree.unfocus();
    let (mut client, mut server) = channel::pair();
    let mut state = app::AppState::new();
    // `g` would normally bubble to the sidebar's Refresh handler.
    // With no focus, the focus_path is empty and nothing handles it.
    app::dispatch_key(
        key(KeyCode::Char('g')),
        &mut tree,
        &ids,
        &mut state,
        &mut client,
    );
    assert!(server.rx.try_recv().is_err(), "no command dispatched");
}

// ── Bracketed paste routing ─────────────────────────────────────────

#[test]
fn paste_into_terminal_emits_write_command() {
    use pilot_v2_ipc::{Event, TerminalId, TerminalKind};
    let (mut tree, ids) = app::build_tree();
    // Pretend a terminal is already running and the focus is on it.
    tree.broadcast_event(&Event::TerminalSpawned {
        terminal_id: TerminalId(1),
        session_key: "ws-1".into(),
        kind: TerminalKind::Shell,
    });
    {
        let stack = tree
            .get_mut::<pilot_v2_tui::components::TerminalStack>(ids.terminals)
            .unwrap();
        stack.set_active_session(Some("ws-1".into()));
    }
    tree.set_focus(ids.terminals);

    let (mut client, mut server) = channel::pair();
    app::dispatch_paste("hello world".into(), &mut tree, &ids, &mut client);

    let cmd = server.rx.try_recv().expect("paste produced a Write");
    match cmd {
        pilot_v2_ipc::Command::Write { terminal_id, bytes } => {
            assert_eq!(terminal_id, TerminalId(1));
            assert_eq!(bytes, b"hello world".to_vec());
        }
        other => panic!("expected Write, got {other:?}"),
    }
}

#[test]
fn paste_outside_terminal_focus_is_dropped() {
    // Pasting while the sidebar has focus shouldn't leak into a
    // background agent. This is the reason we gate `dispatch_paste`
    // on focus rather than just routing to the active terminal.
    let (mut tree, ids) = app::build_tree();
    assert_eq!(tree.focused(), Some(ids.sidebar));
    let (mut client, mut server) = channel::pair();
    app::dispatch_paste("noop".into(), &mut tree, &ids, &mut client);
    assert!(server.rx.try_recv().is_err(), "no Write produced");
}

#[test]
fn paste_with_no_active_terminal_is_dropped() {
    // Focus on terminals but the stack has no active terminal —
    // should silently drop instead of writing to TerminalId(0) or
    // panicking.
    let (mut tree, ids) = app::build_tree();
    tree.set_focus(ids.terminals);
    let (mut client, mut server) = channel::pair();
    app::dispatch_paste("noop".into(), &mut tree, &ids, &mut client);
    assert!(server.rx.try_recv().is_err());
}

#[test]
fn sync_panes_mirrors_sidebar_selection_into_other_panes() {
    use chrono::Utc;
    use pilot_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace};
    let (mut tree, ids) = app::build_tree();

    // Inject a workspace; the sidebar picks it up and sync_panes
    // mirrors the selection into RightPane + TerminalStack. This is
    // the wire from sidebar selection down through the pane tree —
    // the link the user clicks/scrolls/operates on every keystroke.
    let task = Task {
        id: TaskId {
            source: "github".into(),
            key: "o/r#7".into(),
        },
        title: "Hello".into(),
        body: None,
        state: TaskState::Open,
        role: TaskRole::Reviewer,
        ci: CiStatus::Success,
        review: ReviewStatus::Pending,
        checks: vec![],
        unread_count: 0,
        url: "https://x".into(),
        repo: Some("o/r".into()),
        branch: None,
        base_branch: None,
        updated_at: Utc::now(),
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
    let workspace = Workspace::from_task(task, Utc::now());
    let ws_key = workspace.key.as_str().to_string();
    tree.broadcast_event(&Event::WorkspaceUpserted(Box::new(workspace)));

    let mut state = app::AppState::new();
    let (mut client, _server) = channel::pair();
    app::sync_panes(&mut tree, &ids, &mut state, &mut client);

    let rp = tree.get::<RightPane>(ids.right).unwrap();
    assert_eq!(
        rp.selected_workspace().map(|w| w.key.as_str().to_string()),
        Some(ws_key.clone()),
    );
    let ts = tree.get::<TerminalStack>(ids.terminals).unwrap();
    assert_eq!(ts.active_session().map(|k| k.to_string()), Some(ws_key));
}
