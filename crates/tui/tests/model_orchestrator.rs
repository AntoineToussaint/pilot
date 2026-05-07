//! Orchestrator-level tests for `realm::Model`. These replace the
//! deleted `app_loop.rs` tests that exercised the legacy `App` struct
//! end-to-end. Each test names the behaviour it covers — Tab cycle,
//! q-q quit latch, splitter resize, preselect, modal mount, etc.
//!
//! The tests use `Model::new_for_test` (a cfg(test)-only constructor
//! that swaps `CrosstermTerminalAdapter` for `TestTerminalAdapter`)
//! so they don't need a real terminal or raw mode.

use chrono::Utc;
use crossterm::event::{
    KeyModifiers as CtKeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use pilot_core::{SessionKey, Workspace, WorkspaceKey};
use pilot_ipc::{Event as IpcEvent, channel};
use pilot_tui::realm::Model;
use pilot_tui::realm::model::{Id, PaneFocus, Preselect};
use tuirealm::event::{Key, KeyEvent, KeyModifiers};
use tuirealm::ratatui::layout::{Rect, Size};

fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
    let (client, _server) = channel::pair();
    Model::new_for_test(client, Size::new(120, 40)).expect("model init")
}

fn key(code: Key) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn key_with(code: Key, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

#[test]
fn fresh_model_focuses_sidebar() {
    let m = build_model();
    assert_eq!(m.focus(), PaneFocus::Sidebar);
}

#[test]
fn tab_cycles_focus_through_panes() {
    // Tab cycles Sidebar → Right → Terminals → Sidebar when there's
    // no PTY swallowing keys. Inside a terminal with a live PTY,
    // Tab belongs to the shell — use `]]` to exit. The fixture
    // built by `build_model()` has no terminals running, so Tab
    // cycles all the way around.
    let mut m = build_model();
    m.dispatch_key(key(Key::Tab));
    assert_eq!(m.focus(), PaneFocus::Right);
    m.dispatch_key(key(Key::Tab));
    assert_eq!(m.focus(), PaneFocus::Terminals);
    m.dispatch_key(key(Key::Tab));
    assert_eq!(m.focus(), PaneFocus::Sidebar);
}

#[test]
fn single_q_arms_latch_does_not_quit() {
    let mut m = build_model();
    m.dispatch_key(key(Key::Char('q')));
    assert!(!m.quit, "first q must not quit");
    assert!(m.q_arm_pending(), "first q arms the latch");
}

#[test]
fn double_q_within_window_quits() {
    let mut m = build_model();
    m.dispatch_key(key(Key::Char('q')));
    m.dispatch_key(key(Key::Char('q')));
    assert!(m.quit, "second q within the window quits");
}

#[test]
fn other_key_disarms_q_latch() {
    let mut m = build_model();
    m.dispatch_key(key(Key::Char('q')));
    m.dispatch_key(key(Key::Char('j')));
    assert!(!m.q_arm_pending(), "any non-q key disarms the latch");
    m.dispatch_key(key(Key::Char('q')));
    assert!(!m.quit, "after disarm, single q does not quit");
}

#[test]
fn shift_left_shrinks_sidebar() {
    let mut m = build_model();
    let (start_sidebar, _) = m.split_pcts();
    m.dispatch_key(key_with(Key::Left, KeyModifiers::SHIFT));
    let (after, _) = m.split_pcts();
    assert!(
        after < start_sidebar,
        "Shift-Left shrinks sidebar ({start_sidebar}% → {after}%)"
    );
}

#[test]
fn shift_right_grows_sidebar() {
    let mut m = build_model();
    let (start_sidebar, _) = m.split_pcts();
    m.dispatch_key(key_with(Key::Right, KeyModifiers::SHIFT));
    let (after, _) = m.split_pcts();
    assert!(after > start_sidebar);
}

#[test]
fn shift_arrows_clamp_at_min_max() {
    let mut m = build_model();
    // Mash Shift-Left until clamped at min.
    for _ in 0..50 {
        m.dispatch_key(key_with(Key::Left, KeyModifiers::SHIFT));
    }
    let (lo, _) = m.split_pcts();
    assert!(lo >= 15, "sidebar pct stays >= SPLIT_MIN (got {lo})");
    // Mash Shift-Right until clamped at max.
    for _ in 0..50 {
        m.dispatch_key(key_with(Key::Right, KeyModifiers::SHIFT));
    }
    let (hi, _) = m.split_pcts();
    assert!(hi <= 80, "sidebar pct stays <= SPLIT_MAX (got {hi})");
}

#[test]
fn question_mark_mounts_help_modal() {
    // `dispatch_key` bypasses the run-loop's "modal is up" guard
    // and drives `handle_pane_key` directly, so this test verifies
    // the orchestrator-side wiring rather than the run-loop guard.
    let mut m = build_model();
    m.dispatch_key(key(Key::Char('?')));
    assert_eq!(m.top_modal(), Some(&Id::Help));
}

#[test]
fn handle_daemon_event_applies_preselect_on_first_snapshot() {
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let target_key = "github:owner/repo#42";
    let target = SessionKey::from(target_key);
    m = m.with_preselect(Preselect {
        workspace_key: target.clone(),
        session_id_raw: None,
    });
    // Build a snapshot with a single workspace matching the target.
    let workspace = Workspace::empty(
        WorkspaceKey(target_key.to_string()),
        "main",
        Utc::now(),
    );
    let snapshot = IpcEvent::Snapshot {
        workspaces: vec![workspace],
        terminals: Vec::new(),
    };
    m.handle_daemon_event(snapshot);
    // Sidebar should now have the target workspace selected.
    assert_eq!(
        m.sidebar().selected_workspace_key().map(|k| k.as_str()),
        Some(target.as_str())
    );
}

#[test]
fn click_in_right_pane_changes_focus() {
    let mut m = build_model();
    // Splash modal blocks the run loop's crossterm path, but tests
    // bypass that. Click somewhere clearly in the right column.
    let area = Rect::new(0, 0, 100, 30);
    m.dispatch_mouse_in(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 80, // > 40% of 100 → outside sidebar
            row: 5,     // < 25% of 30 → in right-top
            modifiers: CtKeyModifiers::empty(),
        },
        area,
    );
    assert_eq!(m.focus(), PaneFocus::Right);
}

#[test]
fn click_in_sidebar_keeps_or_returns_focus_to_sidebar() {
    let mut m = build_model();
    // Move focus elsewhere first.
    m.dispatch_key(key(Key::Tab));
    assert_eq!(m.focus(), PaneFocus::Right);
    // Click in sidebar area.
    let area = Rect::new(0, 0, 100, 30);
    m.dispatch_mouse_in(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5, // well inside the 40% sidebar column
            row: 10,
            modifiers: CtKeyModifiers::empty(),
        },
        area,
    );
    assert_eq!(m.focus(), PaneFocus::Sidebar);
}

#[test]
fn drag_on_sidebar_splitter_changes_split() {
    let mut m = build_model();
    let (before, _) = m.split_pcts();
    let area = Rect::new(0, 0, 100, 30);
    // Down on the splitter line (col == sidebar.x + sidebar.width).
    m.dispatch_mouse_in(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: before, // splitter sits at this column
            row: 10,
            modifiers: CtKeyModifiers::empty(),
        },
        area,
    );
    // Drag well into the right column.
    m.dispatch_mouse_in(
        MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 70,
            row: 10,
            modifiers: CtKeyModifiers::empty(),
        },
        area,
    );
    let (after, _) = m.split_pcts();
    assert!(
        after > before,
        "dragging right widens sidebar ({before}% → {after}%)"
    );
}
