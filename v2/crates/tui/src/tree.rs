//! `ComponentTree` — owns every component and the parent/child
//! relationships between them, plus the single focus cursor.
//!
//! ## Focus model
//!
//! Focus is a single `ComponentId`: `self.focus`. The focus PATH
//! (root → … → leaf) is derived by walking `parent` edges up to the
//! root. There is no persistent "focus path" state that could drift
//! out of sync with `focus`.
//!
//! ## Key dispatch
//!
//! `handle_key` walks the focus path **leaf → root**:
//! - Each component's `handle_key` gets a chance.
//! - First `Consumed` or `Focus*` variant stops the walk.
//! - `BubbleUp` continues to the parent.
//! - Commands pushed into the sink by any handler flow back to the
//!   caller regardless of where bubbling stops — a parent handler can
//!   observe commands its child queued before bubbling up.
//!
//! ## Event broadcast
//!
//! `broadcast_event` visits every component once and calls
//! `on_event`. Order is stable (`HashMap` iteration is intentionally
//! unordered; we walk a cached id list — see `event_order`).
//!
//! ## Mounting / unmounting
//!
//! - `mount_child(parent, component)` — add under `parent`, appended
//!   to its child list.
//! - `unmount(id)` — remove the component and its subtree. If the
//!   removed subtree contained the focus leaf, focus resets to
//!   `fallback_focus` (the parent of the removed root, or the tree
//!   root if the removed node had no parent).

use crate::component::{Component, ComponentId, Outcome};
use crossterm::event::KeyEvent;
use pilot_v2_ipc::{Command, Event};
use ratatui::Frame;
use ratatui::prelude::Rect;
use std::collections::HashMap;

/// Reasons a `mount_child` call can fail. The child component is
/// returned alongside the error so callers can recover and retry.
///
/// Manual `Debug` impl because `dyn Component` intentionally isn't
/// `Debug` (we don't want to force every component to spell out its
/// state for logs). `Debug` output is the error tag plus the child's
/// id, which is what assertions and logs need.
pub enum MountError {
    MissingParent(Box<dyn Component>),
    DuplicateId(Box<dyn Component>),
}

impl MountError {
    pub fn into_component(self) -> Box<dyn Component> {
        match self {
            MountError::MissingParent(c) | MountError::DuplicateId(c) => c,
        }
    }

    pub fn child_id(&self) -> ComponentId {
        match self {
            MountError::MissingParent(c) | MountError::DuplicateId(c) => c.id(),
        }
    }
}

impl std::fmt::Debug for MountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MountError::MissingParent(c) => f
                .debug_tuple("MissingParent")
                .field(&c.id())
                .finish(),
            MountError::DuplicateId(c) => {
                f.debug_tuple("DuplicateId").field(&c.id()).finish()
            }
        }
    }
}

impl std::fmt::Display for MountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MountError::MissingParent(_) => {
                write!(f, "parent component not in tree")
            }
            MountError::DuplicateId(_) => {
                write!(f, "component id already mounted")
            }
        }
    }
}

impl std::error::Error for MountError {}

pub struct ComponentTree {
    components: HashMap<ComponentId, Box<dyn Component>>,
    parent: HashMap<ComponentId, ComponentId>,
    children: HashMap<ComponentId, Vec<ComponentId>>,
    root: ComponentId,
    focus: ComponentId,
    /// Monotonic id generator. Starts at `root.raw() + 1` so
    /// `alloc_id` never collides with the root's user-chosen id.
    next_id: u64,
    /// Stable iteration order for events. Appended on mount, filtered
    /// on unmount so every component receives events at most once per
    /// broadcast in a predictable order.
    event_order: Vec<ComponentId>,
}

impl ComponentTree {
    /// Build a tree from a root component. The root's id is preserved.
    pub fn new(root: Box<dyn Component>) -> Self {
        let root_id = root.id();
        let mut components = HashMap::new();
        let mut event_order = Vec::new();
        components.insert(root_id, root);
        event_order.push(root_id);
        Self {
            components,
            parent: HashMap::new(),
            children: HashMap::new(),
            root: root_id,
            focus: root_id,
            next_id: root_id.raw() + 1,
            event_order,
        }
    }

    /// Allocate a fresh id guaranteed not to collide with any existing
    /// component. Hand to your component constructor.
    pub fn alloc_id(&mut self) -> ComponentId {
        let id = ComponentId::new(self.next_id);
        self.next_id += 1;
        id
    }

    /// Mount `child` under `parent`. Returns the child's id on success,
    /// or a `MountError` carrying the rejected component so the caller
    /// can handle / retry / drop it explicitly (no silent leaks).
    pub fn mount_child(
        &mut self,
        parent: ComponentId,
        child: Box<dyn Component>,
    ) -> Result<ComponentId, MountError> {
        if !self.components.contains_key(&parent) {
            return Err(MountError::MissingParent(child));
        }
        let child_id = child.id();
        if self.components.contains_key(&child_id) {
            return Err(MountError::DuplicateId(child));
        }
        self.components.insert(child_id, child);
        self.parent.insert(child_id, parent);
        self.children.entry(parent).or_default().push(child_id);
        self.event_order.push(child_id);
        Ok(child_id)
    }

    /// Remove a component and everything below it. If the focus leaf
    /// was inside the removed subtree, focus falls back to the
    /// removed root's parent (or the tree root if it had none).
    pub fn unmount(&mut self, id: ComponentId) {
        if id == self.root {
            // Refuse to unmount the root — the tree would be empty.
            return;
        }
        if !self.components.contains_key(&id) {
            return;
        }
        // Capture the parent edge BEFORE we remove it inside the loop.
        let parent_id = self.parent.get(&id).copied();
        let subtree: Vec<ComponentId> = self.collect_subtree(id);
        let focus_was_in_subtree = subtree.contains(&self.focus);

        for node in &subtree {
            self.components.remove(node);
            self.parent.remove(node);
            self.children.remove(node);
        }
        // Detach `id` from its parent's children list.
        if let Some(pid) = parent_id
            && let Some(siblings) = self.children.get_mut(&pid)
        {
            siblings.retain(|c| *c != id);
        }
        if focus_was_in_subtree {
            self.focus = parent_id.unwrap_or(self.root);
        }
        self.event_order.retain(|x| !subtree.contains(x));
    }

    fn collect_subtree(&self, id: ComponentId) -> Vec<ComponentId> {
        let mut out = Vec::new();
        let mut stack = vec![id];
        while let Some(node) = stack.pop() {
            out.push(node);
            if let Some(kids) = self.children.get(&node) {
                stack.extend(kids.iter().copied());
            }
        }
        out
    }

    // ── Focus queries ──────────────────────────────────────────────────

    pub fn root_id(&self) -> ComponentId {
        self.root
    }

    pub fn focused(&self) -> ComponentId {
        self.focus
    }

    /// Root → … → leaf. Always non-empty; root is element 0.
    pub fn focus_path(&self) -> Vec<ComponentId> {
        let mut path = vec![self.focus];
        let mut cur = self.focus;
        while let Some(&p) = self.parent.get(&cur) {
            path.push(p);
            cur = p;
        }
        path.reverse();
        path
    }

    pub fn contains(&self, id: ComponentId) -> bool {
        self.components.contains_key(&id)
    }

    pub fn parent_of(&self, id: ComponentId) -> Option<ComponentId> {
        self.parent.get(&id).copied()
    }

    pub fn children_of(&self, id: ComponentId) -> &[ComponentId] {
        self.children
            .get(&id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Move focus to `id`. No-op if `id` isn't in the tree.
    pub fn set_focus(&mut self, id: ComponentId) -> bool {
        if self.components.contains_key(&id) {
            self.focus = id;
            true
        } else {
            false
        }
    }

    /// Move focus to the next sibling of the current focus leaf,
    /// wrapping. If the focus leaf is the root (no siblings), no-op.
    pub fn focus_next_sibling(&mut self) {
        self.cycle_sibling(1);
    }

    pub fn focus_prev_sibling(&mut self) {
        self.cycle_sibling(-1);
    }

    fn cycle_sibling(&mut self, dir: isize) {
        let Some(parent) = self.parent.get(&self.focus).copied() else {
            return;
        };
        let Some(siblings) = self.children.get(&parent) else {
            return;
        };
        if siblings.len() < 2 {
            return;
        }
        let Some(pos) = siblings.iter().position(|c| *c == self.focus) else {
            return;
        };
        let n = siblings.len() as isize;
        let next = ((pos as isize + dir).rem_euclid(n)) as usize;
        self.focus = siblings[next];
    }

    // ── Key + event dispatch ───────────────────────────────────────────

    /// Walk the focus path leaf → root calling `handle_key` on each
    /// component. Returns any commands the handlers pushed.
    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<Command> {
        let mut cmds = Vec::new();
        let mut path = self.focus_path();
        path.reverse(); // leaf → root

        for id in path {
            let Some(comp) = self.components.get_mut(&id) else {
                continue;
            };
            let outcome = comp.handle_key(key, &mut cmds);
            match outcome {
                Outcome::Consumed => break,
                Outcome::BubbleUp => continue,
                Outcome::FocusNext => {
                    self.focus_next_sibling();
                    break;
                }
                Outcome::FocusPrev => {
                    self.focus_prev_sibling();
                    break;
                }
                Outcome::FocusId(target) => {
                    self.set_focus(target);
                    break;
                }
            }
        }
        cmds
    }

    /// Fan an event out to every mounted component in mount order.
    pub fn broadcast_event(&mut self, event: &Event) {
        // Clone the id list so we can mutate components during iteration
        // (e.g. an event handler mounts a new component — the new one
        // joins for the NEXT broadcast, not this one).
        let ids = self.event_order.clone();
        for id in ids {
            if let Some(comp) = self.components.get_mut(&id) {
                comp.on_event(event);
            }
        }
    }

    // ── Rendering ──────────────────────────────────────────────────────

    /// Render a single component's `render` method. Most callers
    /// compose layouts with ratatui's `Layout` and render each child
    /// into its sub-rect. This helper exists so callers can look up a
    /// component by id without holding a reference across a layout
    /// boundary.
    pub fn render_one(&mut self, id: ComponentId, area: Rect, frame: &mut Frame) {
        let focused = self.focus == id;
        if let Some(comp) = self.components.get_mut(&id) {
            comp.render(area, frame, focused);
        }
    }

    // ── Test / introspection hooks ─────────────────────────────────────

    /// Look up a component by id for test assertions.
    #[cfg(test)]
    pub(crate) fn component(&self, id: ComponentId) -> Option<&dyn Component> {
        self.components.get(&id).map(|b| b.as_ref())
    }

    pub fn component_count(&self) -> usize {
        self.components.len()
    }
}
