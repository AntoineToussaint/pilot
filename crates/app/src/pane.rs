//! Tmux-style pane tree manager.
//!
//! The pane tree is a binary tree where each internal node is a horizontal or
//! vertical split, and each leaf holds a `PaneContent`. The tree can be
//! arbitrarily nested — split any pane to create a new pair.

use ratatui::prelude::Rect;

pub type PaneId = u32;

/// What a leaf pane displays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneContent {
    /// The PR inbox / session list sidebar.
    Inbox,
    /// PR detail view for a session.
    Detail(String),
    /// Embedded terminal for a session.
    Terminal(String),
    /// Empty placeholder.
    Empty,
}

/// A node in the pane tree.
#[derive(Debug, Clone)]
pub enum PaneNode {
    Leaf {
        id: PaneId,
        content: PaneContent,
    },
    HSplit {
        left: Box<PaneNode>,
        right: Box<PaneNode>,
        /// Percentage (0-100) of width allocated to the left pane.
        ratio: u16,
    },
    VSplit {
        top: Box<PaneNode>,
        bottom: Box<PaneNode>,
        /// Percentage (0-100) of height allocated to the top pane.
        ratio: u16,
    },
}

/// A resolved leaf ready for rendering.
#[derive(Debug, Clone)]
pub struct ResolvedPane {
    pub id: PaneId,
    pub content: PaneContent,
    pub area: Rect,
}

/// Manages the pane tree, focus, and ID allocation.
pub struct PaneManager {
    pub root: PaneNode,
    pub focused: PaneId,
    next_id: PaneId,
    /// If Some, this pane is fullscreened (temporarily overrides layout).
    fullscreen: Option<PaneId>,
}

impl PaneManager {
    /// Create the default layout: Inbox on the left, Detail on the right.
    pub fn default_layout() -> Self {
        let inbox = PaneNode::Leaf {
            id: 0,
            content: PaneContent::Inbox,
        };
        let detail = PaneNode::Leaf {
            id: 1,
            content: PaneContent::Detail(String::new()),
        };
        Self {
            root: PaneNode::HSplit {
                left: Box::new(inbox),
                right: Box::new(detail),
                ratio: 30,
            },
            focused: 0,
            next_id: 2,
            fullscreen: None,
        }
    }

    fn alloc_id(&mut self) -> PaneId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Resolve the tree into a flat list of (id, content, rect) for rendering.
    pub fn resolve(&self, area: Rect) -> Vec<ResolvedPane> {
        // If fullscreened, only render that one pane.
        if let Some(fs_id) = self.fullscreen
            && let Some(content) = self.find_content(fs_id) {
                return vec![ResolvedPane {
                    id: fs_id,
                    content,
                    area,
                }];
            }
        let mut result = Vec::new();
        resolve_node(&self.root, area, &mut result);
        result
    }

    /// Split the focused pane vertically (top/bottom). The existing content
    /// stays on top, new content goes bottom.
    pub fn split_vertical(&mut self, new_content: PaneContent) -> PaneId {
        let new_id = self.alloc_id();
        let target = self.focused;
        replace_node(&mut self.root, target, |old| PaneNode::VSplit {
            top: Box::new(old),
            bottom: Box::new(PaneNode::Leaf {
                id: new_id,
                content: new_content,
            }),
            ratio: 50,
        });
        self.focused = new_id;
        new_id
    }

    /// Split the focused pane vertically, putting the new content ON TOP
    /// and the existing content on the bottom. Used to hoist the embedded
    /// terminal above the detail pane — Claude Code is the primary surface,
    /// PR comments are reference material.
    pub fn split_vertical_above(&mut self, new_content: PaneContent) -> PaneId {
        let new_id = self.alloc_id();
        let target = self.focused;
        replace_node(&mut self.root, target, |old| PaneNode::VSplit {
            top: Box::new(PaneNode::Leaf {
                id: new_id,
                content: new_content,
            }),
            bottom: Box::new(old),
            ratio: 60,
        });
        self.focused = new_id;
        new_id
    }

    /// Split the focused pane horizontally (left/right). Existing stays left.
    pub fn split_horizontal(&mut self, new_content: PaneContent) -> PaneId {
        let new_id = self.alloc_id();
        let target = self.focused;
        replace_node(&mut self.root, target, |old| PaneNode::HSplit {
            left: Box::new(old),
            right: Box::new(PaneNode::Leaf {
                id: new_id,
                content: new_content,
            }),
            ratio: 50,
        });
        self.focused = new_id;
        new_id
    }

    /// Close a pane. Its sibling absorbs the parent's space.
    /// Returns true if the pane was found and removed.
    pub fn close(&mut self, pane_id: PaneId) -> bool {
        if let Some(sibling) = remove_node(&mut self.root, pane_id) {
            // If we closed the focused pane, focus the sibling.
            if self.focused == pane_id {
                self.focused = first_leaf_id(&sibling);
            }
            // The sibling replaced the parent in the tree via remove_node.
            // But we need to actually set it — remove_node returns the sibling
            // and replaces in-place. Actually let me re-check the approach.
            return true;
        }
        false
    }

    /// Close the currently focused pane. Returns true if successful.
    pub fn close_focused(&mut self) -> bool {
        let id = self.focused;
        self.close(id)
    }

    /// Focus a specific pane by ID. No-op if the ID doesn't exist.
    pub fn focus(&mut self, pane_id: PaneId) {
        let leaves = collect_leaf_ids(&self.root);
        if leaves.contains(&pane_id) {
            self.focused = pane_id;
        }
    }

    /// Replace any `Terminal(key)` leaves where `key` is NOT in `live_keys`
    /// with `Empty`. Call after closing terminals so the tree doesn't keep
    /// pointing at dead sessions — stale leaves cause phantom "TERM" mode
    /// and an empty pane the user can't escape from.
    pub fn prune_dead_terminals(&mut self, live_keys: &std::collections::BTreeSet<String>) {
        prune_node(&mut self.root, live_keys);
    }

    /// Enforce the pane <-> terminal invariant:
    ///   1. Every `Terminal(k)` leaf must have `k ∈ live_keys`.
    ///   2. If `active_key` is Some and no leaf shows it, either retarget an
    ///      existing Terminal leaf to `active_key` or split the detail pane
    ///      to create one. Result: the active terminal is always visible.
    ///
    /// Pure in the sense that it only depends on pane tree + the two input
    /// sets; no side channels. Unit-tested.
    pub fn enforce_terminal_invariant(
        &mut self,
        live_keys: &std::collections::BTreeSet<String>,
        active_key: Option<&str>,
    ) {
        // 1. Prune dead leaves.
        prune_node(&mut self.root, live_keys);

        // 2. Make sure the active key is visible.
        let Some(key) = active_key else { return };
        if !live_keys.contains(key) {
            return; // active_key isn't actually live; nothing to do.
        }
        let already_visible = find_pane_node(&self.root, &|c| {
            matches!(c, PaneContent::Terminal(k) if k == key)
        })
        .is_some();
        if already_visible {
            return;
        }

        // An existing Terminal leaf (for some other session) → retarget it.
        if let Some(term_id) =
            find_pane_node(&self.root, &|c| matches!(c, PaneContent::Terminal(_)))
        {
            set_content_node(&mut self.root, term_id, PaneContent::Terminal(key.to_string()));
            self.focused = term_id;
            return;
        }

        // No Terminal leaf exists at all → split the detail pane, with the
        // new Terminal leaf ON TOP (Claude is the primary surface).
        if let Some(detail_id) =
            find_pane_node(&self.root, &|c| matches!(c, PaneContent::Detail(_)))
        {
            self.focused = detail_id;
            self.split_vertical_above(PaneContent::Terminal(key.to_string()));
        }
    }

    /// Move focus to the next pane (cycles through all leaves).
    pub fn focus_next(&mut self) {
        let leaves = collect_leaf_ids(&self.root);
        if let Some(pos) = leaves.iter().position(|&id| id == self.focused) {
            self.focused = leaves[(pos + 1) % leaves.len()];
        }
    }

    /// Move focus to the previous pane.
    pub fn focus_prev(&mut self) {
        let leaves = collect_leaf_ids(&self.root);
        if let Some(pos) = leaves.iter().position(|&id| id == self.focused) {
            self.focused = leaves[(pos + leaves.len() - 1) % leaves.len()];
        }
    }

    /// Move focus in a direction based on screen position.
    pub fn focus_direction(&mut self, dir: Direction, area: Rect) {
        let resolved = self.resolve(area);
        let current = resolved.iter().find(|p| p.id == self.focused);
        let Some(current) = current else { return };

        let (cx, cy) = (
            current.area.x + current.area.width / 2,
            current.area.y + current.area.height / 2,
        );

        let candidates: Vec<&ResolvedPane> = resolved
            .iter()
            .filter(|p| p.id != self.focused)
            .filter(|p| match dir {
                Direction::Left => p.area.x + p.area.width <= current.area.x,
                Direction::Right => p.area.x >= current.area.x + current.area.width,
                Direction::Up => p.area.y + p.area.height <= current.area.y,
                Direction::Down => p.area.y >= current.area.y + current.area.height,
            })
            .collect();

        // Pick the closest candidate.
        if let Some(best) = candidates.iter().min_by_key(|p| {
            let (px, py) = (
                p.area.x + p.area.width / 2,
                p.area.y + p.area.height / 2,
            );
            let dx = (px as i32 - cx as i32).unsigned_abs();
            let dy = (py as i32 - cy as i32).unsigned_abs();
            dx + dy
        }) {
            self.focused = best.id;
        }
    }

    /// Resize the split containing the focused pane.
    pub fn resize_focused(&mut self, delta: i16) {
        resize_node(&mut self.root, self.focused, delta);
    }

    /// Toggle fullscreen for the focused pane.
    pub fn fullscreen_toggle(&mut self) {
        if self.fullscreen == Some(self.focused) {
            self.fullscreen = None;
        } else {
            self.fullscreen = Some(self.focused);
        }
    }

    /// Whether a pane is currently fullscreened.
    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen.is_some()
    }

    /// Get the content of the focused pane.
    pub fn focused_content(&self) -> Option<PaneContent> {
        self.find_content(self.focused)
    }

    /// Update a pane's content by ID.
    pub fn set_content(&mut self, pane_id: PaneId, content: PaneContent) {
        set_content_node(&mut self.root, pane_id, content);
    }

    /// Find the first pane with the given content type.
    pub fn find_pane(&self, predicate: impl Fn(&PaneContent) -> bool) -> Option<PaneId> {
        find_pane_node(&self.root, &predicate)
    }

    fn find_content(&self, pane_id: PaneId) -> Option<PaneContent> {
        find_content_node(&self.root, pane_id)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

// ─── Tree operations ───────────────────────────────────────────────────────

fn resolve_node(node: &PaneNode, area: Rect, out: &mut Vec<ResolvedPane>) {
    match node {
        PaneNode::Leaf { id, content } => {
            out.push(ResolvedPane {
                id: *id,
                content: content.clone(),
                area,
            });
        }
        PaneNode::HSplit { left, right, ratio } => {
            let left_width = (area.width as u32 * *ratio as u32 / 100) as u16;
            let left_area = Rect::new(area.x, area.y, left_width, area.height);
            let right_area = Rect::new(
                area.x + left_width,
                area.y,
                area.width.saturating_sub(left_width),
                area.height,
            );
            resolve_node(left, left_area, out);
            resolve_node(right, right_area, out);
        }
        PaneNode::VSplit { top, bottom, ratio } => {
            let top_height = (area.height as u32 * *ratio as u32 / 100) as u16;
            let top_area = Rect::new(area.x, area.y, area.width, top_height);
            let bottom_area = Rect::new(
                area.x,
                area.y + top_height,
                area.width,
                area.height.saturating_sub(top_height),
            );
            resolve_node(top, top_area, out);
            resolve_node(bottom, bottom_area, out);
        }
    }
}

/// Replace a leaf node in-place using a transform function.
fn replace_node(node: &mut PaneNode, target_id: PaneId, f: impl FnOnce(PaneNode) -> PaneNode) -> bool {
    replace_node_inner(node, target_id, Some(f)).is_some()
}

fn replace_node_inner<F: FnOnce(PaneNode) -> PaneNode>(
    node: &mut PaneNode,
    target_id: PaneId,
    f: Option<F>,
) -> Option<()> {
    match node {
        PaneNode::Leaf { id, .. } if *id == target_id => {
            let f = f?;
            let old = std::mem::replace(
                node,
                PaneNode::Leaf {
                    id: 0,
                    content: PaneContent::Empty,
                },
            );
            *node = f(old);
            Some(())
        }
        PaneNode::HSplit { left, right, .. } => {
            // Try left first.
            if matches!(**left, PaneNode::Leaf { id, .. } if id == target_id) {
                let f = f?;
                let old = std::mem::replace(
                    left.as_mut(),
                    PaneNode::Leaf {
                        id: 0,
                        content: PaneContent::Empty,
                    },
                );
                **left = f(old);
                return Some(());
            }
            if matches!(**right, PaneNode::Leaf { id, .. } if id == target_id) {
                let f = f?;
                let old = std::mem::replace(
                    right.as_mut(),
                    PaneNode::Leaf {
                        id: 0,
                        content: PaneContent::Empty,
                    },
                );
                **right = f(old);
                return Some(());
            }
            replace_node_inner(left.as_mut(), target_id, f)
                .or_else(|| replace_node_inner::<F>(right.as_mut(), target_id, None))
        }
        PaneNode::VSplit { top, bottom, .. } => {
            if matches!(**top, PaneNode::Leaf { id, .. } if id == target_id) {
                let f = f?;
                let old = std::mem::replace(
                    top.as_mut(),
                    PaneNode::Leaf {
                        id: 0,
                        content: PaneContent::Empty,
                    },
                );
                **top = f(old);
                return Some(());
            }
            if matches!(**bottom, PaneNode::Leaf { id, .. } if id == target_id) {
                let f = f?;
                let old = std::mem::replace(
                    bottom.as_mut(),
                    PaneNode::Leaf {
                        id: 0,
                        content: PaneContent::Empty,
                    },
                );
                **bottom = f(old);
                return Some(());
            }
            replace_node_inner(top.as_mut(), target_id, f)
                .or_else(|| replace_node_inner::<F>(bottom.as_mut(), target_id, None))
        }
        _ => None,
    }
}

/// Remove a leaf node. The sibling replaces the parent split.
/// Returns true if the node was found and removed.
fn remove_node(root: &mut PaneNode, target_id: PaneId) -> Option<PaneNode> {
    match root {
        PaneNode::Leaf { .. } => None,
        PaneNode::HSplit { left, right, .. } => {
            if matches!(**left, PaneNode::Leaf { id, .. } if id == target_id) {
                let sibling = *right.clone();
                *root = sibling.clone();
                return Some(sibling);
            }
            if matches!(**right, PaneNode::Leaf { id, .. } if id == target_id) {
                let sibling = *left.clone();
                *root = sibling.clone();
                return Some(sibling);
            }
            remove_node(left, target_id).or_else(|| remove_node(right, target_id))
        }
        PaneNode::VSplit { top, bottom, .. } => {
            if matches!(**top, PaneNode::Leaf { id, .. } if id == target_id) {
                let sibling = *bottom.clone();
                *root = sibling.clone();
                return Some(sibling);
            }
            if matches!(**bottom, PaneNode::Leaf { id, .. } if id == target_id) {
                let sibling = *top.clone();
                *root = sibling.clone();
                return Some(sibling);
            }
            remove_node(top, target_id).or_else(|| remove_node(bottom, target_id))
        }
    }
}

fn collect_leaf_ids(node: &PaneNode) -> Vec<PaneId> {
    match node {
        PaneNode::Leaf { id, .. } => vec![*id],
        PaneNode::HSplit { left, right, .. } => {
            let mut ids = collect_leaf_ids(left);
            ids.extend(collect_leaf_ids(right));
            ids
        }
        PaneNode::VSplit { top, bottom, .. } => {
            let mut ids = collect_leaf_ids(top);
            ids.extend(collect_leaf_ids(bottom));
            ids
        }
    }
}

fn first_leaf_id(node: &PaneNode) -> PaneId {
    match node {
        PaneNode::Leaf { id, .. } => *id,
        PaneNode::HSplit { left, .. } => first_leaf_id(left),
        PaneNode::VSplit { top, .. } => first_leaf_id(top),
    }
}

fn resize_node(node: &mut PaneNode, target_id: PaneId, delta: i16) {
    match node {
        PaneNode::Leaf { .. } => {}
        PaneNode::HSplit { left, right, ratio } => {
            if contains_id(left, target_id) || contains_id(right, target_id) {
                let new_ratio = (*ratio as i16 + delta).clamp(10, 90) as u16;
                *ratio = new_ratio;
            } else {
                resize_node(left, target_id, delta);
                resize_node(right, target_id, delta);
            }
        }
        PaneNode::VSplit { top, bottom, ratio } => {
            if contains_id(top, target_id) || contains_id(bottom, target_id) {
                let new_ratio = (*ratio as i16 + delta).clamp(10, 90) as u16;
                *ratio = new_ratio;
            } else {
                resize_node(top, target_id, delta);
                resize_node(bottom, target_id, delta);
            }
        }
    }
}

fn contains_id(node: &PaneNode, target_id: PaneId) -> bool {
    match node {
        PaneNode::Leaf { id, .. } => *id == target_id,
        PaneNode::HSplit { left, right, .. } => {
            contains_id(left, target_id) || contains_id(right, target_id)
        }
        PaneNode::VSplit { top, bottom, .. } => {
            contains_id(top, target_id) || contains_id(bottom, target_id)
        }
    }
}

fn find_content_node(node: &PaneNode, target_id: PaneId) -> Option<PaneContent> {
    match node {
        PaneNode::Leaf { id, content } if *id == target_id => Some(content.clone()),
        PaneNode::HSplit { left, right, .. } => {
            find_content_node(left, target_id).or_else(|| find_content_node(right, target_id))
        }
        PaneNode::VSplit { top, bottom, .. } => {
            find_content_node(top, target_id).or_else(|| find_content_node(bottom, target_id))
        }
        _ => None,
    }
}

fn set_content_node(node: &mut PaneNode, target_id: PaneId, new_content: PaneContent) {
    match node {
        PaneNode::Leaf { id, content } if *id == target_id => {
            *content = new_content;
        }
        PaneNode::HSplit { left, right, .. } => {
            set_content_node(left, target_id, new_content.clone());
            set_content_node(right, target_id, new_content);
        }
        PaneNode::VSplit { top, bottom, .. } => {
            set_content_node(top, target_id, new_content.clone());
            set_content_node(bottom, target_id, new_content);
        }
        _ => {}
    }
}

fn prune_node(node: &mut PaneNode, live: &std::collections::BTreeSet<String>) {
    match node {
        PaneNode::Leaf { content, .. } => {
            if let PaneContent::Terminal(key) = content
                && !live.contains(key) {
                    *content = PaneContent::Empty;
                }
        }
        PaneNode::HSplit { left, right, .. } => {
            prune_node(left, live);
            prune_node(right, live);
        }
        PaneNode::VSplit { top, bottom, .. } => {
            prune_node(top, live);
            prune_node(bottom, live);
        }
    }
}

fn find_pane_node(node: &PaneNode, predicate: &impl Fn(&PaneContent) -> bool) -> Option<PaneId> {
    match node {
        PaneNode::Leaf { id, content } if predicate(content) => Some(*id),
        PaneNode::HSplit { left, right, .. } => {
            find_pane_node(left, predicate).or_else(|| find_pane_node(right, predicate))
        }
        PaneNode::VSplit { top, bottom, .. } => {
            find_pane_node(top, predicate).or_else(|| find_pane_node(bottom, predicate))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layout_has_inbox_and_detail() {
        let p = PaneManager::default_layout();
        assert!(p.find_pane(|c| matches!(c, PaneContent::Inbox)).is_some());
        assert!(p.find_pane(|c| matches!(c, PaneContent::Detail(_))).is_some());
    }

    #[test]
    fn find_pane_then_focus() {
        let mut p = PaneManager::default_layout();
        let detail_id = p.find_pane(|c| matches!(c, PaneContent::Detail(_))).unwrap();
        p.focus(detail_id);
        assert!(matches!(p.focused_content(), Some(PaneContent::Detail(_))));
    }

    #[test]
    fn focus_nonexistent_is_noop() {
        let mut p = PaneManager::default_layout();
        let before = p.focused;
        p.focus(9999);
        assert_eq!(p.focused, before);
    }

    #[test]
    fn split_vertical_adds_terminal_pane() {
        let mut p = PaneManager::default_layout();
        p.split_vertical(PaneContent::Terminal("key1".into()));
        assert!(p
            .find_pane(|c| matches!(c, PaneContent::Terminal(k) if k == "key1"))
            .is_some());
        // The new pane becomes focused.
        assert!(matches!(p.focused_content(), Some(PaneContent::Terminal(_))));
    }

    #[test]
    fn split_vertical_above_places_new_on_top() {
        // Regression: when spawning Claude, the terminal must land ABOVE
        // the detail pane — Claude is the primary surface.
        let mut p = PaneManager::default_layout();
        let detail_id = p
            .find_pane(|c| matches!(c, PaneContent::Detail(_)))
            .unwrap();
        p.focus(detail_id);
        p.split_vertical_above(PaneContent::Terminal("t1".into()));

        // Resolve with a known area and assert the Terminal's rect sits
        // above the Detail's rect.
        let area = Rect::new(0, 0, 100, 100);
        let resolved = p.resolve(area);
        let term = resolved
            .iter()
            .find(|r| matches!(r.content, PaneContent::Terminal(_)))
            .expect("terminal in tree");
        let detail = resolved
            .iter()
            .find(|r| matches!(r.content, PaneContent::Detail(_)))
            .expect("detail in tree");
        assert!(
            term.area.y < detail.area.y,
            "terminal y={} should be above detail y={}",
            term.area.y,
            detail.area.y,
        );
    }

    #[test]
    fn focus_next_cycles_through_leaves() {
        let mut p = PaneManager::default_layout();
        let first = p.focused;
        p.focus_next();
        assert_ne!(p.focused, first);
        p.focus_next();
        assert_eq!(p.focused, first);
    }

    #[test]
    fn prune_replaces_dead_terminal_with_empty() {
        // Regression: if a pane points at a closed terminal key and nothing
        // rewrites it, determine_mode sees Terminal content → stays stuck in
        // TERM mode even though no terminal is rendered. prune_dead_terminals
        // is the fix.
        let mut p = PaneManager::default_layout();
        p.split_vertical(PaneContent::Terminal("dead-key".into()));
        assert!(p
            .find_pane(|c| matches!(c, PaneContent::Terminal(k) if k == "dead-key"))
            .is_some());

        let live = std::collections::BTreeSet::new(); // no terminals alive
        p.prune_dead_terminals(&live);

        assert!(p
            .find_pane(|c| matches!(c, PaneContent::Terminal(_)))
            .is_none());
        assert!(p.find_pane(|c| matches!(c, PaneContent::Empty)).is_some());
    }

    fn keys(ks: &[&str]) -> std::collections::BTreeSet<String> {
        ks.iter().map(|k| k.to_string()).collect()
    }

    #[test]
    fn enforce_invariant_creates_pane_when_terminal_active_but_not_shown() {
        // REGRESSION: user pressed `f` → prompt was sent to Claude → but
        // the terminal map had the terminal and no Terminal pane existed,
        // so the user saw DETAIL mode with no terminal visible.
        let mut p = PaneManager::default_layout();
        assert!(p
            .find_pane(|c| matches!(c, PaneContent::Terminal(_)))
            .is_none());

        p.enforce_terminal_invariant(&keys(&["sess-a"]), Some("sess-a"));

        let term_id = p
            .find_pane(|c| matches!(c, PaneContent::Terminal(k) if k == "sess-a"))
            .expect("Terminal pane should have been created");
        // It must be the focused pane.
        assert_eq!(p.focused, term_id);
    }

    #[test]
    fn enforce_invariant_retargets_existing_pane_for_different_session() {
        let mut p = PaneManager::default_layout();
        p.split_vertical(PaneContent::Terminal("old".into()));
        // Switch active session to "new".
        p.enforce_terminal_invariant(&keys(&["new"]), Some("new"));
        // "old" was pruned (not in live_keys), "new" took over.
        assert!(p
            .find_pane(|c| matches!(c, PaneContent::Terminal(k) if k == "new"))
            .is_some());
        assert!(p
            .find_pane(|c| matches!(c, PaneContent::Terminal(k) if k == "old"))
            .is_none());
    }

    #[test]
    fn enforce_invariant_noop_when_active_already_visible() {
        let mut p = PaneManager::default_layout();
        p.split_vertical(PaneContent::Terminal("a".into()));
        let before_focused = p.focused;
        let before_tree = format!("{:?}", p.root);
        p.enforce_terminal_invariant(&keys(&["a"]), Some("a"));
        // Tree unchanged, focus unchanged.
        assert_eq!(format!("{:?}", p.root), before_tree);
        assert_eq!(p.focused, before_focused);
    }

    #[test]
    fn enforce_invariant_prunes_dead_without_active_key() {
        let mut p = PaneManager::default_layout();
        p.split_vertical(PaneContent::Terminal("zombie".into()));
        p.enforce_terminal_invariant(&keys(&[]), None);
        // Dead pane replaced with Empty, no new pane created.
        assert!(p
            .find_pane(|c| matches!(c, PaneContent::Terminal(_)))
            .is_none());
    }

    #[test]
    fn enforce_invariant_ignores_stale_active_key() {
        // If active_key points at a session that isn't actually alive,
        // don't create a pane for it — that would be a lie.
        let mut p = PaneManager::default_layout();
        p.enforce_terminal_invariant(&keys(&[]), Some("stale"));
        assert!(p
            .find_pane(|c| matches!(c, PaneContent::Terminal(_)))
            .is_none());
    }

    #[test]
    fn prune_keeps_live_terminals() {
        let mut p = PaneManager::default_layout();
        p.split_vertical(PaneContent::Terminal("alive".into()));
        let mut live = std::collections::BTreeSet::new();
        live.insert("alive".to_string());
        p.prune_dead_terminals(&live);
        assert!(p
            .find_pane(|c| matches!(c, PaneContent::Terminal(k) if k == "alive"))
            .is_some());
    }

    #[test]
    fn close_removes_pane_and_refocuses() {
        let mut p = PaneManager::default_layout();
        p.split_vertical(PaneContent::Terminal("k".into()));
        let term_id = p
            .find_pane(|c| matches!(c, PaneContent::Terminal(_)))
            .unwrap();
        assert_eq!(p.focused, term_id);
        assert!(p.close(term_id));
        assert!(p.find_pane(|c| matches!(c, PaneContent::Terminal(_))).is_none());
        // Focus must land somewhere valid.
        assert!(p.focused_content().is_some());
    }
}
