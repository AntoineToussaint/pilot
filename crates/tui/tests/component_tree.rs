//! Comprehensive tests for `ComponentTree` key routing.
//!
//! These are the assertions the v1 four-slot-desync bug class dies on.
//! Every "Tab stopped working" / "focus teleported" / "key went to an
//! invisible pane" failure in v1 is captured here as a structural
//! invariant of the tree.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pilot_v2_ipc::{Command, Event};
use pilot_v2_tui::{Component, ComponentId, ComponentTree, Outcome};
use ratatui::Frame;
use ratatui::prelude::Rect;
use std::cell::RefCell;
use std::rc::Rc;

/// Shared recorder state. The test holds an `Rc` to the same cell the
/// component holds; reading back what keys/events the component saw
/// needs no downcasting.
#[derive(Default)]
struct RecordState {
    keys_seen: Vec<KeyEvent>,
    events_seen: Vec<String>,
}

struct Recorder {
    id: ComponentId,
    outcome: Outcome,
    commands_to_emit: RefCell<Vec<Command>>,
    state: Rc<RefCell<RecordState>>,
}

impl Recorder {
    fn new(id: ComponentId, outcome: Outcome) -> (Self, Rc<RefCell<RecordState>>) {
        let state = Rc::new(RefCell::new(RecordState::default()));
        (
            Self {
                id,
                outcome,
                commands_to_emit: RefCell::new(Vec::new()),
                state: state.clone(),
            },
            state,
        )
    }

    fn with_command(self, cmd: Command) -> Self {
        self.commands_to_emit.borrow_mut().push(cmd);
        self
    }
}

impl Component for Recorder {
    fn id(&self) -> ComponentId {
        self.id
    }
    fn handle_key(&mut self, key: KeyEvent, cmds: &mut Vec<Command>) -> Outcome {
        self.state.borrow_mut().keys_seen.push(key);
        for cmd in self.commands_to_emit.borrow_mut().drain(..) {
            cmds.push(cmd);
        }
        self.outcome
    }
    fn on_event(&mut self, event: &Event) {
        self.state
            .borrow_mut()
            .events_seen
            .push(format!("{event:?}"));
    }
    fn render(&mut self, _: Rect, _: &mut Frame, _: bool) {}
}

fn key_char(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

/// Event stub used in broadcast tests — Event has no PartialEq so we
/// just grab a cheap variant we know serializes fine.
fn notif() -> Event {
    Event::Notification {
        title: String::new(),
        body: String::new(),
    }
}

// ── Basic structure ────────────────────────────────────────────────────

#[test]
fn new_tree_focuses_root() {
    let (rec, _) = Recorder::new(ComponentId::new(1), Outcome::BubbleUp);
    let tree = ComponentTree::new(Box::new(rec));
    assert_eq!(tree.focused(), Some(ComponentId::new(1)));
    assert_eq!(tree.root_id(), ComponentId::new(1));
    assert_eq!(tree.focus_path(), vec![ComponentId::new(1)]);
}

#[test]
fn mount_child_is_reachable() {
    let root_id = ComponentId::new(1);
    let child_id = ComponentId::new(2);
    let (root, _) = Recorder::new(root_id, Outcome::BubbleUp);
    let (child, _) = Recorder::new(child_id, Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    tree.mount_child(root_id, Box::new(child))
        .expect("mount ok");
    assert!(tree.contains(child_id));
    assert_eq!(tree.parent_of(child_id), Some(root_id));
    assert_eq!(tree.children_of(root_id), &[child_id]);
}

#[test]
fn mount_under_missing_parent_errors() {
    let (root, _) = Recorder::new(ComponentId::new(1), Outcome::BubbleUp);
    let (other, _) = Recorder::new(ComponentId::new(2), Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    let result = tree.mount_child(ComponentId::new(99), Box::new(other));
    assert!(result.is_err(), "mount under unknown parent must fail");
}

#[test]
fn mount_duplicate_id_errors() {
    let root_id = ComponentId::new(1);
    let dup = ComponentId::new(42);
    let (root, _) = Recorder::new(root_id, Outcome::BubbleUp);
    let (a, _) = Recorder::new(dup, Outcome::BubbleUp);
    let (b, _) = Recorder::new(dup, Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    tree.mount_child(root_id, Box::new(a)).expect("first ok");
    assert!(tree.mount_child(root_id, Box::new(b)).is_err());
}

// ── Focus navigation ───────────────────────────────────────────────────

#[test]
fn focus_path_reflects_nesting() {
    let root_id = ComponentId::new(1);
    let child_id = ComponentId::new(2);
    let grand_id = ComponentId::new(3);
    let (root, _) = Recorder::new(root_id, Outcome::BubbleUp);
    let (child, _) = Recorder::new(child_id, Outcome::BubbleUp);
    let (grand, _) = Recorder::new(grand_id, Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    tree.mount_child(root_id, Box::new(child)).unwrap();
    tree.mount_child(child_id, Box::new(grand)).unwrap();

    tree.set_focus(grand_id);
    assert_eq!(
        tree.focus_path(),
        vec![root_id, child_id, grand_id],
        "focus path is root → … → leaf"
    );
}

#[test]
fn set_focus_rejects_unknown_id() {
    let (root, _) = Recorder::new(ComponentId::new(1), Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    assert!(!tree.set_focus(ComponentId::new(999)));
    assert_eq!(tree.focused(), Some(ComponentId::new(1)));
}

#[test]
fn focus_next_sibling_cycles_and_wraps() {
    let root_id = ComponentId::new(1);
    let a = ComponentId::new(2);
    let b = ComponentId::new(3);
    let c = ComponentId::new(4);
    let (root, _) = Recorder::new(root_id, Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    for id in [a, b, c] {
        let (r, _) = Recorder::new(id, Outcome::BubbleUp);
        tree.mount_child(root_id, Box::new(r)).unwrap();
    }
    tree.set_focus(a);
    tree.focus_next_sibling();
    assert_eq!(tree.focused(), Some(b));
    tree.focus_next_sibling();
    assert_eq!(tree.focused(), Some(c));
    tree.focus_next_sibling();
    assert_eq!(tree.focused(), Some(a), "wraps last → first");
}

#[test]
fn focus_prev_sibling_wraps_the_other_way() {
    let root_id = ComponentId::new(1);
    let a = ComponentId::new(2);
    let b = ComponentId::new(3);
    let (root, _) = Recorder::new(root_id, Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    for id in [a, b] {
        let (r, _) = Recorder::new(id, Outcome::BubbleUp);
        tree.mount_child(root_id, Box::new(r)).unwrap();
    }
    tree.set_focus(a);
    tree.focus_prev_sibling();
    assert_eq!(tree.focused(), Some(b), "wraps first → last");
}

#[test]
fn focus_next_on_root_is_noop() {
    let (root, _) = Recorder::new(ComponentId::new(1), Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    tree.focus_next_sibling();
    assert_eq!(tree.focused(), Some(ComponentId::new(1)));
}

#[test]
fn focus_next_single_child_is_noop() {
    let root_id = ComponentId::new(1);
    let only = ComponentId::new(2);
    let (root, _) = Recorder::new(root_id, Outcome::BubbleUp);
    let (child, _) = Recorder::new(only, Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    tree.mount_child(root_id, Box::new(child)).unwrap();
    tree.set_focus(only);
    tree.focus_next_sibling();
    assert_eq!(tree.focused(), Some(only));
}

// ── Key dispatch: bubbling ─────────────────────────────────────────────

#[test]
fn leaf_consumed_stops_bubble() {
    let root_id = ComponentId::new(1);
    let leaf = ComponentId::new(2);
    let (root, root_state) = Recorder::new(root_id, Outcome::BubbleUp);
    let (child, child_state) = Recorder::new(leaf, Outcome::Consumed);
    let mut tree = ComponentTree::new(Box::new(root));
    tree.mount_child(root_id, Box::new(child)).unwrap();
    tree.set_focus(leaf);

    let cmds = tree.handle_key(key_char('j'));
    assert!(cmds.is_empty());
    assert_eq!(child_state.borrow().keys_seen.len(), 1);
    assert_eq!(
        root_state.borrow().keys_seen.len(),
        0,
        "Consumed stops the bubble before root sees the key"
    );
}

#[test]
fn leaf_bubbles_up_reaches_root() {
    let root_id = ComponentId::new(1);
    let leaf = ComponentId::new(2);
    let (root, root_state) = Recorder::new(root_id, Outcome::Consumed);
    let (child, child_state) = Recorder::new(leaf, Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    tree.mount_child(root_id, Box::new(child)).unwrap();
    tree.set_focus(leaf);
    tree.handle_key(key_char('x'));
    assert_eq!(child_state.borrow().keys_seen.len(), 1);
    assert_eq!(root_state.borrow().keys_seen.len(), 1);
}

#[test]
fn bubble_all_the_way_up_stops_at_root() {
    let root_id = ComponentId::new(1);
    let mid = ComponentId::new(2);
    let leaf = ComponentId::new(3);
    let (root, rs) = Recorder::new(root_id, Outcome::BubbleUp);
    let (mid_r, ms) = Recorder::new(mid, Outcome::BubbleUp);
    let (leaf_r, ls) = Recorder::new(leaf, Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    tree.mount_child(root_id, Box::new(mid_r)).unwrap();
    tree.mount_child(mid, Box::new(leaf_r)).unwrap();
    tree.set_focus(leaf);

    let cmds = tree.handle_key(key_char('q'));
    assert!(cmds.is_empty());
    assert_eq!(ls.borrow().keys_seen.len(), 1);
    assert_eq!(ms.borrow().keys_seen.len(), 1);
    assert_eq!(rs.borrow().keys_seen.len(), 1);
}

#[test]
fn parent_can_override_child_via_bubbleup_chain() {
    let root_id = ComponentId::new(1);
    let leaf = ComponentId::new(2);
    let (root, rs) = Recorder::new(root_id, Outcome::Consumed);
    let (child, ls) = Recorder::new(leaf, Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    tree.mount_child(root_id, Box::new(child)).unwrap();
    tree.set_focus(leaf);
    tree.handle_key(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT));
    assert_eq!(ls.borrow().keys_seen.len(), 1);
    assert_eq!(rs.borrow().keys_seen.len(), 1);
}

// ── Key dispatch: commands ─────────────────────────────────────────────

#[test]
fn consumed_with_command_returns_command() {
    let root_id = ComponentId::new(1);
    let leaf = ComponentId::new(2);
    let (root, _) = Recorder::new(root_id, Outcome::BubbleUp);
    let (child, _) = Recorder::new(leaf, Outcome::Consumed);
    let child = child.with_command(Command::Refresh);
    let mut tree = ComponentTree::new(Box::new(root));
    tree.mount_child(root_id, Box::new(child)).unwrap();
    tree.set_focus(leaf);

    let cmds = tree.handle_key(key_char('g'));
    assert_eq!(cmds.len(), 1);
    assert_eq!(format!("{:?}", cmds[0]), format!("{:?}", Command::Refresh));
}

#[test]
fn commands_accumulate_across_bubble_chain() {
    let root_id = ComponentId::new(1);
    let leaf = ComponentId::new(2);
    let (root, _) = Recorder::new(root_id, Outcome::Consumed);
    let root = root.with_command(Command::Refresh);
    let (child, _) = Recorder::new(leaf, Outcome::BubbleUp);
    let child = child.with_command(Command::Shutdown);
    let mut tree = ComponentTree::new(Box::new(root));
    tree.mount_child(root_id, Box::new(child)).unwrap();
    tree.set_focus(leaf);

    let cmds = tree.handle_key(key_char('x'));
    assert_eq!(cmds.len(), 2, "both handlers pushed commands");
    assert_eq!(format!("{:?}", cmds[0]), format!("{:?}", Command::Shutdown));
    assert_eq!(format!("{:?}", cmds[1]), format!("{:?}", Command::Refresh));
}

// ── Key dispatch: focus outcomes ───────────────────────────────────────

#[test]
fn focus_next_outcome_moves_focus_and_stops_bubble() {
    let root_id = ComponentId::new(1);
    let a = ComponentId::new(2);
    let b = ComponentId::new(3);
    let (root, rs) = Recorder::new(root_id, Outcome::Consumed);
    let (ar, _) = Recorder::new(a, Outcome::FocusNext);
    let (br, _) = Recorder::new(b, Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    tree.mount_child(root_id, Box::new(ar)).unwrap();
    tree.mount_child(root_id, Box::new(br)).unwrap();
    tree.set_focus(a);

    tree.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(tree.focused(), Some(b), "FocusNext moved focus");
    assert_eq!(
        rs.borrow().keys_seen.len(),
        0,
        "FocusNext is terminal — root must not have seen the Tab"
    );
}

#[test]
fn focus_id_jumps_across_branches() {
    // The v1 `/` JumpToNextAsking pattern: a sidebar component sends
    // FocusId pointing directly at a terminal living under the right
    // pane. v1 had to juggle selected/focused/active_tab to pull this
    // off; here it's one outcome.
    let root_id = ComponentId::new(1);
    let left = ComponentId::new(2);
    let right = ComponentId::new(3);
    let term = ComponentId::new(4);
    let (root, _) = Recorder::new(root_id, Outcome::BubbleUp);
    let (l, _) = Recorder::new(left, Outcome::FocusId(term));
    let (r, _) = Recorder::new(right, Outcome::BubbleUp);
    let (t, _) = Recorder::new(term, Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    tree.mount_child(root_id, Box::new(l)).unwrap();
    tree.mount_child(root_id, Box::new(r)).unwrap();
    tree.mount_child(right, Box::new(t)).unwrap();
    tree.set_focus(left);

    tree.handle_key(key_char('/'));
    assert_eq!(tree.focused(), Some(term));
    assert_eq!(tree.focus_path(), vec![root_id, right, term]);
}

#[test]
fn focus_id_for_unknown_id_is_ignored() {
    let root_id = ComponentId::new(1);
    let leaf = ComponentId::new(2);
    let (root, _) = Recorder::new(root_id, Outcome::BubbleUp);
    let (child, _) = Recorder::new(leaf, Outcome::FocusId(ComponentId::new(9999)));
    let mut tree = ComponentTree::new(Box::new(root));
    tree.mount_child(root_id, Box::new(child)).unwrap();
    tree.set_focus(leaf);

    tree.handle_key(key_char('x'));
    assert_eq!(
        tree.focused(),
        Some(leaf),
        "FocusId with unknown id keeps current focus"
    );
}

// ── Unmount + focus fallback ───────────────────────────────────────────

#[test]
fn unmount_removes_subtree() {
    let root_id = ComponentId::new(1);
    let parent = ComponentId::new(2);
    let c1 = ComponentId::new(3);
    let c2 = ComponentId::new(4);
    let (root, _) = Recorder::new(root_id, Outcome::BubbleUp);
    let (p, _) = Recorder::new(parent, Outcome::BubbleUp);
    let (a, _) = Recorder::new(c1, Outcome::BubbleUp);
    let (b, _) = Recorder::new(c2, Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    tree.mount_child(root_id, Box::new(p)).unwrap();
    tree.mount_child(parent, Box::new(a)).unwrap();
    tree.mount_child(parent, Box::new(b)).unwrap();

    assert_eq!(tree.component_count(), 4);
    tree.unmount(parent);
    assert_eq!(tree.component_count(), 1, "only root remains");
    assert!(!tree.contains(parent));
    assert!(!tree.contains(c1));
    assert!(!tree.contains(c2));
    assert!(tree.children_of(root_id).is_empty());
}

#[test]
fn unmount_focused_leaf_falls_back_to_parent() {
    let root_id = ComponentId::new(1);
    let leaf = ComponentId::new(2);
    let (root, _) = Recorder::new(root_id, Outcome::BubbleUp);
    let (child, _) = Recorder::new(leaf, Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    tree.mount_child(root_id, Box::new(child)).unwrap();
    tree.set_focus(leaf);

    tree.unmount(leaf);
    assert_eq!(tree.focused(), Some(root_id));
}

#[test]
fn unmount_root_is_refused() {
    let root_id = ComponentId::new(1);
    let (root, _) = Recorder::new(root_id, Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    tree.unmount(root_id);
    assert!(tree.contains(root_id), "root can never be unmounted");
}

// ── Event broadcast ────────────────────────────────────────────────────

#[test]
fn broadcast_reaches_every_component() {
    let root_id = ComponentId::new(1);
    let a = ComponentId::new(2);
    let b = ComponentId::new(3);
    let (root, rs) = Recorder::new(root_id, Outcome::BubbleUp);
    let (ar, as_) = Recorder::new(a, Outcome::BubbleUp);
    let (br, bs) = Recorder::new(b, Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    tree.mount_child(root_id, Box::new(ar)).unwrap();
    tree.mount_child(a, Box::new(br)).unwrap();

    tree.broadcast_event(&notif());
    assert_eq!(rs.borrow().events_seen.len(), 1);
    assert_eq!(as_.borrow().events_seen.len(), 1);
    assert_eq!(bs.borrow().events_seen.len(), 1);
}

#[test]
fn unmounted_components_stop_receiving_events() {
    let root_id = ComponentId::new(1);
    let leaf = ComponentId::new(2);
    let (root, rs) = Recorder::new(root_id, Outcome::BubbleUp);
    let (child, ls) = Recorder::new(leaf, Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    tree.mount_child(root_id, Box::new(child)).unwrap();

    tree.broadcast_event(&notif());
    tree.unmount(leaf);
    tree.broadcast_event(&notif());

    assert_eq!(rs.borrow().events_seen.len(), 2);
    assert_eq!(
        ls.borrow().events_seen.len(),
        1,
        "leaf only saw the pre-unmount broadcast"
    );
}

#[test]
fn broadcast_order_is_mount_order() {
    // Mount order IS event order. This matters because components
    // that react to e.g. `SessionUpserted` by mounting a child should
    // rely on the parent having already processed the event.
    let root_id = ComponentId::new(1);
    let a = ComponentId::new(10);
    let b = ComponentId::new(20);
    let c = ComponentId::new(30);
    let (root, _) = Recorder::new(root_id, Outcome::BubbleUp);
    let (ar, _) = Recorder::new(a, Outcome::BubbleUp);
    let (br, _) = Recorder::new(b, Outcome::BubbleUp);
    let (cr, _) = Recorder::new(c, Outcome::BubbleUp);

    // Shared order log — each recorder pushes its id when on_event fires.
    let order: Rc<RefCell<Vec<u64>>> = Rc::new(RefCell::new(Vec::new()));

    struct OrderLogger {
        id: ComponentId,
        log: Rc<RefCell<Vec<u64>>>,
    }
    impl Component for OrderLogger {
        fn id(&self) -> ComponentId {
            self.id
        }
        fn on_event(&mut self, _: &Event) {
            self.log.borrow_mut().push(self.id.raw());
        }
        fn render(&mut self, _: Rect, _: &mut Frame, _: bool) {}
    }

    let mut tree = ComponentTree::new(Box::new(OrderLogger {
        id: root_id,
        log: order.clone(),
    }));
    // Tip: we ignore the pre-built Recorders here and mount
    // OrderLogger instead. The Recorders are kept above only to
    // reserve the ids.
    drop((root, ar, br, cr));
    for id in [a, b, c] {
        tree.mount_child(
            root_id,
            Box::new(OrderLogger {
                id,
                log: order.clone(),
            }),
        )
        .unwrap();
    }
    tree.broadcast_event(&notif());
    assert_eq!(
        *order.borrow(),
        vec![root_id.raw(), a.raw(), b.raw(), c.raw()],
        "broadcast order matches mount order"
    );
}

#[test]
fn event_handler_can_emit_commands_via_key_dispatch_next() {
    // Contract: on_event mutates state; a subsequent key press can
    // observe that state and push commands. This is how "daemon sent
    // AgentState(Asking); next time the user hits /, jump to it"
    // composes.
    struct SessionObserver {
        id: ComponentId,
        last_asking: Option<String>,
    }
    impl Component for SessionObserver {
        fn id(&self) -> ComponentId {
            self.id
        }
        fn on_event(&mut self, event: &Event) {
            if let Event::AgentState { session_key, state } = event
                && matches!(state, pilot_v2_ipc::AgentState::Asking)
            {
                self.last_asking = Some(format!("{session_key:?}"));
            }
        }
        fn handle_key(&mut self, key: KeyEvent, cmds: &mut Vec<Command>) -> Outcome {
            if key.code == KeyCode::Char('/')
                && let Some(_key_str) = self.last_asking.as_ref()
            {
                cmds.push(Command::Refresh);
                return Outcome::Consumed;
            }
            Outcome::BubbleUp
        }
        fn render(&mut self, _: Rect, _: &mut Frame, _: bool) {}
    }

    let id = ComponentId::new(1);
    let mut tree = ComponentTree::new(Box::new(SessionObserver {
        id,
        last_asking: None,
    }));

    // No asking state yet — `/` does nothing.
    let cmds = tree.handle_key(key_char('/'));
    assert!(cmds.is_empty());

    // Server says an agent is asking.
    tree.broadcast_event(&Event::AgentState {
        session_key: "github:o/r#1".into(),
        state: pilot_v2_ipc::AgentState::Asking,
    });

    // Now `/` produces the command the observer queued up.
    let cmds = tree.handle_key(key_char('/'));
    assert_eq!(cmds.len(), 1);
    assert_eq!(format!("{:?}", cmds[0]), format!("{:?}", Command::Refresh));
}

// ── alloc_id sanity ────────────────────────────────────────────────────

#[test]
fn alloc_id_hands_out_unique_ids() {
    let (root, _) = Recorder::new(ComponentId::new(1), Outcome::BubbleUp);
    let mut tree = ComponentTree::new(Box::new(root));
    let a = tree.alloc_id();
    let b = tree.alloc_id();
    let c = tree.alloc_id();
    assert_ne!(a, b);
    assert_ne!(b, c);
    assert_ne!(a, c);
    // Never collides with an existing mount.
    assert!(!tree.contains(a));
}
