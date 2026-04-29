//! Root — the invisible anchor that owns the top-level children
//! (Sidebar, RightPane, TerminalStack, any overlays). Has no rendering
//! of its own: the run loop computes the global layout and calls
//! `tree.render_one(child_id, rect, frame)` directly.
//!
//! The only reason this exists is because `ComponentTree` requires a
//! single root, and we want Sidebar / RightPane / TerminalStack to be
//! **siblings** (so `FocusNext` cycles among them) rather than nested.

use crate::{Component, ComponentId, Outcome};
use crossterm::event::KeyEvent;
use pilot_v2_ipc::Command;
use ratatui::Frame;
use ratatui::prelude::Rect;

pub struct Root {
    id: ComponentId,
}

impl Root {
    pub fn new(id: ComponentId) -> Self {
        Self { id }
    }
}

impl Component for Root {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn handle_key(&mut self, _key: KeyEvent, _cmds: &mut Vec<Command>) -> Outcome {
        // Root is the end of the bubble chain — nothing to do.
        Outcome::BubbleUp
    }

    fn render(&mut self, _area: Rect, _frame: &mut Frame, _focused: bool) {
        // Intentionally empty: the run loop composites the layout.
    }
}
