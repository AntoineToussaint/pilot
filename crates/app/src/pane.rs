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
        if let Some(fs_id) = self.fullscreen {
            if let Some(content) = self.find_content(fs_id) {
                return vec![ResolvedPane {
                    id: fs_id,
                    content,
                    area,
                }];
            }
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
                *left = Box::new(f(old));
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
                *right = Box::new(f(old));
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
                *top = Box::new(f(old));
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
                *bottom = Box::new(f(old));
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
