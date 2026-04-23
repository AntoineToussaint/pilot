//! The `Component` trait — the single contract every piece of UI
//! implements.
//!
//! ## Design
//!
//! A `Component` is a self-contained UI unit that:
//!
//! 1. Owns its own state. No global "app state" struct that gets
//!    passed around. Each component's state is private — the only way
//!    to influence it is via `handle_key`, `on_event`, or direct
//!    mutation at construction time.
//! 2. Declares its interest in daemon events via `on_event`. The
//!    default impl ignores everything; each component overrides to
//!    react to the slice it cares about.
//! 3. Declares what keystrokes it handles via `handle_key`. Returns an
//!    `Outcome` describing whether the key was consumed and whether
//!    focus should move. Emits zero or more `Command`s to the daemon
//!    through the `&mut Vec<Command>` sink — side effects are an
//!    explicit output, never hidden inside the component.
//! 4. Renders itself into a given `Rect` when the tree asks. The
//!    `focused` flag tells it whether IT specifically is the current
//!    focus leaf so it can style borders / cursors accordingly.
//!
//! Parent-child relationships are stored in the `ComponentTree`, not in
//! the components themselves. This means:
//! - Composites don't own `Box<dyn Component>` fields.
//! - Key dispatch can walk the focus path without recursive `find`
//!   methods every composite would otherwise need to implement.
//! - Overlays (Help / NewWorktree / Picker) slot in by mounting a
//!   component at the root; no special case.

use crossterm::event::KeyEvent;
use pilot_v2_ipc::{Command, Event};
use ratatui::prelude::Rect;
use ratatui::Frame;
use std::any::Any;

/// Blanket supertrait that gives every `Component` free `Any` downcast
/// methods. The tree uses these to let the app read typed overlay
/// state (branch name on `NewWorktree`, etc.) without requiring each
/// Component impl to write boilerplate.
pub trait AsAny: 'static {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: 'static> AsAny for T {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Globally unique identifier for a component inside one `ComponentTree`.
///
/// Assigned at mount time and immutable for the component's lifetime.
/// `ComponentTree::alloc_id` hands out a fresh id so you never collide;
/// `ComponentId::new` is exposed for tests that want deterministic ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComponentId(u64);

impl ComponentId {
    /// Construct an id with a specific raw value. Prefer
    /// `ComponentTree::alloc_id` for mounting; use this in tests where
    /// a stable id simplifies assertions.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// What happens AFTER a component finishes handling a key event.
///
/// - `Consumed`: the component handled the key; stop bubbling. Nothing
///   happens to focus.
/// - `BubbleUp`: the component didn't claim the key; the tree
///   continues walking toward the root.
/// - `FocusNext` / `FocusPrev`: consume the key AND cycle focus among
///   the **siblings of the current focus leaf**. Wraps at the ends.
/// - `FocusId`: consume the key AND move focus to a specific
///   component. Fails silently if the id isn't in the tree; this is a
///   soft contract since overlays can be mounted/dismounted at runtime.
/// - `Dismiss`: consume the key AND unmount the component that
///   returned this outcome. Focus falls back to the unmounted
///   component's parent. This is how overlays (Help, NewWorktree)
///   self-close on Esc / Enter — they don't have to know who mounted
///   them, they just signal "I'm done."
///
/// There is intentionally no "consumed AND bubbled" — every variant is
/// exclusive, which keeps reasoning about dispatch linear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Consumed,
    BubbleUp,
    FocusNext,
    FocusPrev,
    FocusId(ComponentId),
    Dismiss,
}

/// The contract every UI component implements.
///
/// Default impls cover the "I'm a boring leaf" case: no key handling,
/// no event subscriptions, no children. Override selectively.
///
/// The `AsAny` supertrait (blanket-implemented for every `'static`
/// type) gives components free `Any` downcast support so the app /
/// tree can retrieve typed state from an overlay without every impl
/// having to write `as_any` boilerplate.
pub trait Component: AsAny {
    /// The component's id. Must be stable for the component's lifetime.
    fn id(&self) -> ComponentId;

    /// Respond to a key event that has reached this component during
    /// the leaf→root bubble pass. Push any daemon commands into `cmds`;
    /// the tree forwards them to the daemon in order.
    ///
    /// Default: bubble up — useful for container components that only
    /// want to forward.
    fn handle_key(&mut self, key: KeyEvent, cmds: &mut Vec<Command>) -> Outcome {
        let _ = (key, cmds);
        Outcome::BubbleUp
    }

    /// React to a daemon-side event. Called during event broadcast by
    /// the tree; every component gets the call and decides what to do
    /// (filter internally with `matches!` on the `Event` variant).
    ///
    /// Default: ignore.
    fn on_event(&mut self, event: &Event) {
        let _ = event;
    }

    /// Render. The `focused` flag is true if this specific component
    /// is the current focus leaf — useful for drawing selected borders
    /// or the text cursor.
    fn render(&mut self, area: Rect, frame: &mut Frame, focused: bool);
}
