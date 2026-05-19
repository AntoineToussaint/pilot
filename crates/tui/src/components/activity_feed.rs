//! Headless model for the right pane's activity feed.
//!
//! The activity feed owns three pieces of navigation state:
//! - **cursor**: which card the keyboard focus is on.
//! - **expanded**: indices the user has popped open with `→`/`l`.
//! - **selected**: indices the user multi-selected with `v` (for
//!   the `f` "fix these comments" prompt).
//!
//! These all change in response to keystrokes — they have nothing
//! to do with ratatui or the render path. Pulling them out into a
//! struct lets the navigation operations (`move_cursor_down`,
//! `toggle_expand`, `clear_selection`, `on_workspace_change`)
//! become plain methods the test suite can drive directly,
//! without constructing a `Frame` or a `TestBackend`.
//!
//! The renderer (`RightPane::render_activity`) reads the feed via
//! `&` and renders accordingly. Click handling mutates the feed
//! via `&mut`.

use std::collections::HashSet;

/// Navigation state for the right pane's activity feed. Pure
/// model — no ratatui, no Workspace borrow, no Theme. Lives as a
/// field on `RightPane`; replaced wholesale on workspace change
/// via `on_workspace_change()`.
///
/// `cursor` is a plain pub field so callers can `+= 1` it inline
/// without going through a getter/setter dance. `expanded` and
/// `selected` are private because they're sets — mutating them
/// individually risks getting out of sync; the methods enforce
/// the toggle / clear contracts.
#[derive(Debug, Default)]
pub struct ActivityFeed {
    /// Highlighted activity index. `0` is the newest card.
    pub cursor: usize,
    /// Indices the user has expanded inline. Default empty.
    expanded: HashSet<usize>,
    /// Indices the user has multi-selected with `v`. Default
    /// empty; `f` falls back to the cursor's row when empty.
    selected: HashSet<usize>,
}

impl ActivityFeed {
    pub fn new() -> Self {
        Self::default()
    }

    /// Move the cursor down by one, capped at `visible.saturating_sub(1)`.
    /// No-op when the feed is empty (visible == 0).
    pub fn move_cursor_down(&mut self, visible: usize) {
        if visible == 0 {
            return;
        }
        let max = visible.saturating_sub(1);
        if self.cursor < max {
            self.cursor += 1;
        }
    }

    /// Move the cursor up by one, capped at 0.
    pub fn move_cursor_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Clamp the cursor to `[0, visible-1]`. Use after the
    /// workspace's activity list shrinks (e.g. read filter
    /// applied) so the cursor doesn't dangle past the end.
    pub fn clamp_cursor(&mut self, visible: usize) {
        if visible == 0 {
            self.cursor = 0;
        } else if self.cursor >= visible {
            self.cursor = visible - 1;
        }
    }

    /// Is `idx` currently expanded?
    pub fn is_expanded(&self, idx: usize) -> bool {
        self.expanded.contains(&idx)
    }

    /// Insert/remove `idx` from the expanded set.
    pub fn toggle_expand(&mut self, idx: usize) {
        if !self.expanded.insert(idx) {
            self.expanded.remove(&idx);
        }
    }

    /// Set `idx`'s expanded state explicitly.
    pub fn set_expanded(&mut self, idx: usize, expanded: bool) {
        if expanded {
            self.expanded.insert(idx);
        } else {
            self.expanded.remove(&idx);
        }
    }

    /// Borrow the expanded set (read-only).
    pub fn expanded(&self) -> &HashSet<usize> {
        &self.expanded
    }

    /// Is `idx` currently selected?
    pub fn is_selected(&self, idx: usize) -> bool {
        self.selected.contains(&idx)
    }

    /// Insert/remove `idx` from the selection set. Returns the
    /// new state (true = now selected) so callers can surface a
    /// `✓ Selected` / `✓ Deselected` footer notice.
    pub fn toggle_select(&mut self, idx: usize) -> bool {
        if self.selected.insert(idx) {
            true
        } else {
            self.selected.remove(&idx);
            false
        }
    }

    /// Borrow the selected set (read-only). `f` reads this to
    /// build the prompt; empty set means "use the cursor row".
    pub fn selected(&self) -> &HashSet<usize> {
        &self.selected
    }

    /// Drop the entire selection. Bound to a UX key + called
    /// automatically on workspace change.
    pub fn clear_selection(&mut self) {
        self.selected.clear();
    }

    /// Drop all expansion state. Called when the activity list
    /// length changes — every index has shifted, so an "expanded
    /// row 3" no longer points at the same comment. (Selection
    /// state intentionally NOT cleared here, matching the
    /// pre-refactor behavior; a future fix could reset both.)
    pub fn clear_expanded(&mut self) {
        self.expanded.clear();
    }

    /// Reset all per-workspace state. Activity indices are
    /// per-workspace (workspace A's index 0 has nothing to do
    /// with workspace B's index 0), so cursor/expanded/selected
    /// must reset when the focused workspace changes.
    pub fn on_workspace_change(&mut self) {
        self.cursor = 0;
        self.expanded.clear();
        self.selected.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_at_cursor_zero_with_empty_sets() {
        let f = ActivityFeed::new();
        assert_eq!(f.cursor, 0);
        assert!(f.expanded().is_empty());
        assert!(f.selected().is_empty());
    }

    #[test]
    fn move_cursor_down_caps_at_visible_minus_one() {
        let mut f = ActivityFeed::new();
        f.move_cursor_down(3);
        f.move_cursor_down(3);
        f.move_cursor_down(3);
        // Cursor was 0, +1+1+1 capped at 2 (3-1).
        assert_eq!(f.cursor, 2);
    }

    #[test]
    fn move_cursor_down_noop_when_empty() {
        let mut f = ActivityFeed::new();
        f.move_cursor_down(0);
        assert_eq!(f.cursor, 0);
    }

    #[test]
    fn move_cursor_up_clamps_at_zero() {
        let mut f = ActivityFeed::new();
        f.cursor = 2;
        f.move_cursor_up();
        f.move_cursor_up();
        f.move_cursor_up();
        // 2 → 1 → 0 → 0 (clamped).
        assert_eq!(f.cursor, 0);
    }

    #[test]
    fn clamp_cursor_pulls_into_range() {
        let mut f = ActivityFeed::new();
        f.cursor = 10;
        f.clamp_cursor(3);
        assert_eq!(f.cursor, 2);
    }

    #[test]
    fn clamp_cursor_zero_when_empty() {
        let mut f = ActivityFeed::new();
        f.cursor = 10;
        f.clamp_cursor(0);
        assert_eq!(f.cursor, 0);
    }

    #[test]
    fn toggle_expand_round_trips() {
        let mut f = ActivityFeed::new();
        assert!(!f.is_expanded(2));
        f.toggle_expand(2);
        assert!(f.is_expanded(2));
        f.toggle_expand(2);
        assert!(!f.is_expanded(2));
    }

    #[test]
    fn set_expanded_overrides_state() {
        let mut f = ActivityFeed::new();
        f.set_expanded(1, true);
        f.set_expanded(1, true);
        assert!(f.is_expanded(1));
        f.set_expanded(1, false);
        assert!(!f.is_expanded(1));
    }

    #[test]
    fn toggle_select_returns_new_state() {
        let mut f = ActivityFeed::new();
        assert!(f.toggle_select(7));
        assert!(f.is_selected(7));
        assert!(!f.toggle_select(7));
        assert!(!f.is_selected(7));
    }

    #[test]
    fn clear_selection_drops_all_indices() {
        let mut f = ActivityFeed::new();
        f.toggle_select(1);
        f.toggle_select(3);
        f.toggle_select(5);
        f.clear_selection();
        assert!(f.selected().is_empty());
    }

    /// Workspace change is the single point that resets the feed.
    /// Cursor, expanded, selected all wipe.
    #[test]
    fn on_workspace_change_resets_everything() {
        let mut f = ActivityFeed::new();
        f.cursor = 5;
        f.toggle_expand(2);
        f.toggle_select(3);
        f.on_workspace_change();
        assert_eq!(f.cursor, 0);
        assert!(f.expanded().is_empty());
        assert!(f.selected().is_empty());
    }

    /// Expansion + selection are independent — toggling one
    /// doesn't touch the other. This pins the contract; the
    /// original right_pane code mixed these through a shared
    /// `if !is_expanded { continue; }` block which led to the
    /// "click-to-select doesn't work on collapsed cards" bug.
    #[test]
    fn expansion_and_selection_are_independent_sets() {
        let mut f = ActivityFeed::new();
        f.toggle_expand(2);
        f.toggle_select(2);
        assert!(f.is_expanded(2));
        assert!(f.is_selected(2));
        f.toggle_expand(2);
        // Card collapses but stays selected.
        assert!(!f.is_expanded(2));
        assert!(f.is_selected(2));
    }
}
