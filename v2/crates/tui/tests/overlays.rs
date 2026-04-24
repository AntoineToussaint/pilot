//! Overlay behavior + integration tests.
//!
//! These pin down the "can't get trapped in a dialog" invariant: no
//! matter what, Esc / Ctrl-C dismiss. And the "overlay is composable"
//! invariant: mounting an overlay steals focus, dismissing returns
//! focus to its parent, and AppRoot can read typed overlay state via
//! `tree.get::<T>()` before/after dismiss.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pilot_v2_ipc::Event;
use pilot_v2_tui::components::{Help, NewWorktree, NewWorktreeResult};
use pilot_v2_tui::{Component, ComponentId, ComponentTree, Outcome};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::prelude::Rect;

fn ch(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn code(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

// ── Help ───────────────────────────────────────────────────────────────

#[test]
fn help_any_key_dismisses() {
    let mut h = Help::default_help(ComponentId::new(1));
    assert_eq!(h.handle_key(ch('x'), &mut Vec::new()), Outcome::Dismiss);
    assert_eq!(
        h.handle_key(code(KeyCode::Esc), &mut Vec::new()),
        Outcome::Dismiss
    );
    assert_eq!(
        h.handle_key(code(KeyCode::Enter), &mut Vec::new()),
        Outcome::Dismiss
    );
}

#[test]
fn help_renders_content() {
    let mut h = Help::new(ComponentId::new(1), vec!["one".into(), "two".into()]);
    let backend = TestBackend::new(60, 10);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| {
        h.render(Rect::new(0, 0, 60, 10), f, true);
    })
    .unwrap();
    let buf = term.backend().buffer();
    let rendered: String = (0..buf.area.height)
        .flat_map(|y| (0..buf.area.width).map(move |x| buf[(x, y)].symbol().to_string()))
        .collect();
    assert!(rendered.contains("one"));
    assert!(rendered.contains("two"));
    assert!(rendered.contains("Help"), "title present");
}

// ── NewWorktree ────────────────────────────────────────────────────────

#[test]
fn newworktree_accumulates_typed_chars() {
    let mut n = NewWorktree::new(ComponentId::new(1), "Branch off main:");
    for c in "feat/x".chars() {
        n.handle_key(ch(c), &mut Vec::new());
    }
    assert_eq!(n.input(), "feat/x");
    assert_eq!(n.result(), &NewWorktreeResult::Pending);
}

#[test]
fn newworktree_backspace_removes_last() {
    let mut n = NewWorktree::new(ComponentId::new(1), "");
    for c in "abcd".chars() {
        n.handle_key(ch(c), &mut Vec::new());
    }
    n.handle_key(code(KeyCode::Backspace), &mut Vec::new());
    assert_eq!(n.input(), "abc");
}

#[test]
fn newworktree_esc_cancels_and_dismisses() {
    let mut n = NewWorktree::new(ComponentId::new(1), "");
    for c in "halfway".chars() {
        n.handle_key(ch(c), &mut Vec::new());
    }
    assert_eq!(
        n.handle_key(code(KeyCode::Esc), &mut Vec::new()),
        Outcome::Dismiss
    );
    assert_eq!(n.result(), &NewWorktreeResult::Canceled);
}

#[test]
fn newworktree_ctrl_c_also_cancels() {
    let mut n = NewWorktree::new(ComponentId::new(1), "");
    n.handle_key(ch('a'), &mut Vec::new());
    assert_eq!(n.handle_key(ctrl('c'), &mut Vec::new()), Outcome::Dismiss);
    assert_eq!(n.result(), &NewWorktreeResult::Canceled);
}

#[test]
fn newworktree_enter_with_valid_name_confirms() {
    let mut n = NewWorktree::new(ComponentId::new(1), "");
    for c in "feat/nice-branch".chars() {
        n.handle_key(ch(c), &mut Vec::new());
    }
    assert_eq!(
        n.handle_key(code(KeyCode::Enter), &mut Vec::new()),
        Outcome::Dismiss
    );
    assert_eq!(
        n.result(),
        &NewWorktreeResult::Confirmed("feat/nice-branch".to_string())
    );
}

#[test]
fn newworktree_enter_with_empty_name_is_noop() {
    let mut n = NewWorktree::new(ComponentId::new(1), "");
    // Enter with nothing typed: must NOT dismiss, must NOT confirm.
    let outcome = n.handle_key(code(KeyCode::Enter), &mut Vec::new());
    assert_eq!(outcome, Outcome::Consumed);
    assert_eq!(n.result(), &NewWorktreeResult::Pending);
}

#[test]
fn newworktree_enter_with_invalid_name_is_noop() {
    let mut n = NewWorktree::new(ComponentId::new(1), "");
    for c in "has space".chars() {
        n.handle_key(ch(c), &mut Vec::new());
    }
    let outcome = n.handle_key(code(KeyCode::Enter), &mut Vec::new());
    assert_eq!(outcome, Outcome::Consumed, "invalid — stay up");
    assert_eq!(n.result(), &NewWorktreeResult::Pending);
}

#[test]
fn newworktree_invalid_chars_rejected_from_confirm() {
    // Check every bad char individually blocks confirm.
    for bad in ['~', '^', ':', '?', '*', '[', '\\'] {
        let mut n = NewWorktree::new(ComponentId::new(1), "");
        for c in "feat".chars() {
            n.handle_key(ch(c), &mut Vec::new());
        }
        n.handle_key(ch(bad), &mut Vec::new());
        let outcome = n.handle_key(code(KeyCode::Enter), &mut Vec::new());
        assert_eq!(
            outcome,
            Outcome::Consumed,
            "branch name with {bad:?} must not confirm"
        );
    }
}

#[test]
fn newworktree_rejects_leading_dash() {
    let mut n = NewWorktree::new(ComponentId::new(1), "");
    for c in "-feat".chars() {
        n.handle_key(ch(c), &mut Vec::new());
    }
    assert_eq!(
        n.handle_key(code(KeyCode::Enter), &mut Vec::new()),
        Outcome::Consumed
    );
}

#[test]
fn newworktree_rejects_dotdot() {
    let mut n = NewWorktree::new(ComponentId::new(1), "");
    for c in "foo..bar".chars() {
        n.handle_key(ch(c), &mut Vec::new());
    }
    assert_eq!(
        n.handle_key(code(KeyCode::Enter), &mut Vec::new()),
        Outcome::Consumed
    );
}

#[test]
fn newworktree_with_input_prefill() {
    let mut n = NewWorktree::new(ComponentId::new(1), "").with_input("db-config");
    assert_eq!(n.input(), "db-config");
    assert_eq!(
        n.handle_key(code(KeyCode::Enter), &mut Vec::new()),
        Outcome::Dismiss
    );
    assert_eq!(
        n.result(),
        &NewWorktreeResult::Confirmed("db-config".to_string())
    );
}

#[test]
fn newworktree_ignores_irrelevant_events() {
    let mut n = NewWorktree::new(ComponentId::new(1), "");
    n.handle_key(ch('a'), &mut Vec::new());
    n.on_event(&Event::Notification {
        title: "hi".into(),
        body: "".into(),
    });
    assert_eq!(n.input(), "a");
}

// ── Tree integration: overlay mount + dismiss ──────────────────────────

/// Helper: a minimal parent that just swallows keys. Overlays mount
/// under it; we test that dismissing the overlay returns focus here.
struct Blank {
    id: ComponentId,
}

impl Component for Blank {
    fn id(&self) -> ComponentId {
        self.id
    }
    fn handle_key(&mut self, _: KeyEvent, _: &mut Vec<pilot_v2_ipc::Command>) -> Outcome {
        Outcome::Consumed
    }
    fn render(&mut self, _: Rect, _: &mut ratatui::Frame, _: bool) {}
}

#[test]
fn dismiss_outcome_unmounts_and_returns_focus_to_parent() {
    let root_id = ComponentId::new(1);
    let overlay_id = ComponentId::new(2);
    let mut tree = ComponentTree::new(Box::new(Blank { id: root_id }));
    tree.mount_child(root_id, Box::new(Help::new(overlay_id, vec!["hi".into()])))
        .unwrap();
    tree.set_focus(overlay_id);
    assert!(tree.contains(overlay_id));
    assert_eq!(tree.focused(), overlay_id);

    // Any key → Help dismisses → tree unmounts it, focus falls back.
    tree.handle_key(ch('x'));
    assert!(!tree.contains(overlay_id), "overlay was unmounted");
    assert_eq!(
        tree.focused(),
        root_id,
        "focus returns to overlay's parent (root)"
    );
}

#[test]
fn overlay_steals_focus_from_sibling_while_mounted() {
    // Two siblings under root; overlay mounts and takes focus. Keys
    // route to overlay, not the sidebar sibling.
    let root_id = ComponentId::new(1);
    let sibling_id = ComponentId::new(2);
    let overlay_id = ComponentId::new(3);
    let mut tree = ComponentTree::new(Box::new(Blank { id: root_id }));
    tree.mount_child(root_id, Box::new(Blank { id: sibling_id }))
        .unwrap();
    tree.set_focus(sibling_id);

    // Overlay mounts under root (not sibling — overlays are app-level).
    tree.mount_child(root_id, Box::new(NewWorktree::new(overlay_id, "prompt")))
        .unwrap();
    tree.set_focus(overlay_id);

    // Type some chars — they go to the overlay.
    for c in "abc".chars() {
        tree.handle_key(ch(c));
    }
    let overlay_ref = tree
        .get::<NewWorktree>(overlay_id)
        .expect("typed get works");
    assert_eq!(overlay_ref.input(), "abc");

    // Dismiss — focus falls back to the overlay's parent (root),
    // NOT automatically to the sibling. AppRoot restores sibling
    // focus separately; we just test the tree's default.
    tree.handle_key(code(KeyCode::Esc));
    assert!(!tree.contains(overlay_id));
    assert_eq!(tree.focused(), root_id);
}

#[test]
fn typed_get_returns_none_for_wrong_type() {
    let root_id = ComponentId::new(1);
    let overlay_id = ComponentId::new(2);
    let mut tree = ComponentTree::new(Box::new(Blank { id: root_id }));
    tree.mount_child(root_id, Box::new(Help::new(overlay_id, vec!["hi".into()])))
        .unwrap();

    // Asking for the wrong concrete type returns None, not a crash.
    let wrong: Option<&NewWorktree> = tree.get::<NewWorktree>(overlay_id);
    assert!(wrong.is_none());
    let right: Option<&Help> = tree.get::<Help>(overlay_id);
    assert!(right.is_some());
}

#[test]
fn apptroot_style_flow_read_branch_name_on_dismiss() {
    // This mimics the real AppRoot flow:
    //   1. User presses N.
    //   2. Mount NewWorktree under root, set focus.
    //   3. User types a branch name, presses Enter.
    //   4. Dismiss unmounts overlay.
    //   5. AppRoot would have read result() BEFORE dismiss... but the
    //      overlay is gone now. So AppRoot must observe the result
    //      BEFORE the Dismiss outcome unmounts.
    //
    // This test lays out the actual sequence: snapshot result
    // immediately after handle_key, before the tree unmounts.
    let root_id = ComponentId::new(1);
    let overlay_id = ComponentId::new(2);
    let mut tree = ComponentTree::new(Box::new(Blank { id: root_id }));
    tree.mount_child(root_id, Box::new(NewWorktree::new(overlay_id, "")))
        .unwrap();
    tree.set_focus(overlay_id);

    for c in "feat/nice".chars() {
        tree.handle_key(ch(c));
    }
    // Read the buffer while the overlay is still mounted.
    {
        let overlay = tree.get::<NewWorktree>(overlay_id).unwrap();
        assert_eq!(overlay.input(), "feat/nice");
    }

    // Enter → Dismiss → unmount. Result is observable one more time
    // if we captured a clone before, but after dismiss the component
    // is gone.
    tree.handle_key(code(KeyCode::Enter));
    assert!(!tree.contains(overlay_id));
}
