//! `LayoutCtx` — split percentages, drag state, and the splitter math
//! the run loop hands keys + mouse events into. Extracted from
//! `Model` to keep that struct focused on orchestration; `LayoutCtx`
//! is pure data + arithmetic with no IPC, modal, or pane coupling.
//!
//! The two percentages drive the same three-rect layout the rest of
//! the TUI consumes (`pane_areas`). Callers mutate via `update_drag`
//! / `nudge_splits` and read `(sidebar_pct, right_top_pct)` straight
//! off the struct.

use tuirealm::ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Initial split percentages. Match the legacy defaults so users
/// don't see a jumpy first frame after the migration.
pub(crate) const DEFAULT_SIDEBAR_PCT: u16 = 40;
pub(crate) const DEFAULT_RIGHT_TOP_PCT: u16 = 25;
/// Min/max for either splitter (percentage). Keeps every pane
/// usable — no zero-height activity feed, no sliver sidebar.
pub(crate) const SPLIT_MIN: u16 = 15;
pub(crate) const SPLIT_MAX: u16 = 80;
/// Default step size per Shift-arrow tap. Picked so 4-5 taps cover
/// a useful range and a single tap is visibly more than a shimmer.
/// Live value reads from `ui.split_step_percent` (via
/// `pilot_config::UiDefaults`) — kept here so the tests below stay
/// readable.
#[cfg(test)]
pub(crate) const SPLIT_STEP: i16 = 3;

/// Which splitter the user is currently dragging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DragTarget {
    /// The vertical line between sidebar and the right column.
    SidebarRight,
    /// The horizontal line between activity and terminal stack.
    ActivityTerminals,
}

/// Splitter percentages + last-viewport snapshot + active drag, in
/// one place. Methods that mutate the percentages return `bool` so
/// the caller can flip its `redraw` flag without LayoutCtx knowing
/// about the wider model's redraw bookkeeping.
pub(crate) struct LayoutCtx {
    pub sidebar_pct: u16,
    pub right_top_pct: u16,
    pub last_area: Rect,
    pub active_drag: Option<DragTarget>,
}

impl LayoutCtx {
    pub fn new() -> Self {
        Self {
            sidebar_pct: DEFAULT_SIDEBAR_PCT,
            right_top_pct: DEFAULT_RIGHT_TOP_PCT,
            last_area: Rect::default(),
            active_drag: None,
        }
    }

    /// Apply persisted splits from `~/.pilot/config.yaml::ui`. `None`
    /// leaves the default in place.
    pub fn apply_persisted(&mut self, sidebar_pct: Option<u16>, right_top_pct: Option<u16>) {
        if let Some(s) = sidebar_pct {
            self.sidebar_pct = clamp_pct(s as i16);
        }
        if let Some(t) = right_top_pct {
            self.right_top_pct = clamp_pct(t as i16);
        }
    }

    /// Test whether `(col, row)` lands within tolerance of one of the
    /// two splitter lines. Tolerance: ±1 cell so users don't have to
    /// land pixel-perfect on the divider.
    pub fn hit_test_splitter(
        &self,
        col: u16,
        row: u16,
        sidebar_rect: Rect,
        right_top_rect: Rect,
    ) -> Option<DragTarget> {
        // Vertical splitter sits between sidebar and the right column.
        let v_x = sidebar_rect.x + sidebar_rect.width;
        if col + 1 >= v_x
            && col <= v_x + 1
            && row >= self.last_area.y
            && row < self.last_area.y + self.last_area.height
        {
            return Some(DragTarget::SidebarRight);
        }
        // Horizontal splitter sits between right-top and right-bottom.
        let h_y = right_top_rect.y + right_top_rect.height;
        if row + 1 >= h_y
            && row <= h_y + 1
            && col >= right_top_rect.x
            && col < right_top_rect.x + right_top_rect.width
        {
            return Some(DragTarget::ActivityTerminals);
        }
        None
    }

    /// Translate a drag's `(col, row)` into a new percentage for the
    /// active splitter and apply it. Returns `true` if the percentage
    /// actually changed so the caller can redraw.
    pub fn update_drag(&mut self, target: DragTarget, col: u16, row: u16) -> bool {
        match target {
            DragTarget::SidebarRight => {
                if self.last_area.width == 0 {
                    return false;
                }
                let rel = col.saturating_sub(self.last_area.x) as i32;
                let pct = (rel * 100 / self.last_area.width as i32)
                    .clamp(SPLIT_MIN as i32, SPLIT_MAX as i32) as u16;
                if pct != self.sidebar_pct {
                    self.sidebar_pct = pct;
                    return true;
                }
                false
            }
            DragTarget::ActivityTerminals => {
                let (_, right_top_rect, right_bottom_rect) =
                    pane_areas(self.last_area, self.sidebar_pct, self.right_top_pct);
                let right_height = right_top_rect.height + right_bottom_rect.height;
                if right_height == 0 {
                    return false;
                }
                let rel = row.saturating_sub(right_top_rect.y) as i32;
                let pct = (rel * 100 / right_height as i32)
                    .clamp(SPLIT_MIN as i32, SPLIT_MAX as i32) as u16;
                if pct != self.right_top_pct {
                    self.right_top_pct = pct;
                    return true;
                }
                false
            }
        }
    }

    /// Adjust the split percentages. `dx > 0` widens the sidebar;
    /// `dy > 0` grows the activity row at the terminal stack's
    /// expense. Persists to YAML on change. Returns `true` if any
    /// percentage actually changed.
    pub fn nudge_splits(&mut self, dx: i16, dy: i16) -> bool {
        let new_sidebar = clamp_pct(self.sidebar_pct as i16 + dx);
        let new_top = clamp_pct(self.right_top_pct as i16 + dy);
        if new_sidebar != self.sidebar_pct || new_top != self.right_top_pct {
            self.sidebar_pct = new_sidebar;
            self.right_top_pct = new_top;
            self.persist();
            return true;
        }
        false
    }

    /// Best-effort save of the current split percentages.
    pub fn persist(&self) {
        let s = self.sidebar_pct;
        let t = self.right_top_pct;
        if let Err(e) = pilot_config::Config::save_with(|c| {
            c.ui.sidebar_pct = Some(s);
            c.ui.right_top_pct = Some(t);
        }) {
            tracing::warn!("save splits failed: {e}");
        }
    }
}

/// Clamp a candidate percentage into the legal split range.
pub(crate) fn clamp_pct(raw: i16) -> u16 {
    raw.clamp(SPLIT_MIN as i16, SPLIT_MAX as i16) as u16
}

/// Hard cap on the sidebar's column count. Past this, no matter
/// what `sidebar_pct` says, extra horizontal space goes to the
/// right pane. The sidebar's longest natural row (`[PR] #1234 A ●
/// long title here    C CONFLICT  1d`) is around 90 cols; 100
/// gives a small margin without leaving the sidebar dominating an
/// ultra-wide monitor.
///
/// User can manually nudge the percentage up via `Shift-Right` —
/// but the absolute cap stays in force. To override, future work:
/// expose `ui.sidebar_max_cols` in `config.yaml`.
pub(crate) const SIDEBAR_MAX_COLS: u16 = 100;

/// Minimum sidebar width even on a narrow terminal — below this
/// the row content is unreadable. Picked to fit the
/// `[PR] #NNN A …` prefix plus a meaningful slice of the title.
pub(crate) const SIDEBAR_MIN_COLS: u16 = 30;

/// Compute the three pane rects (sidebar / right-top / right-bottom).
/// `sidebar_pct` is the sidebar's share of the total width;
/// `right_top_pct` is the activity row's share of the right column's
/// height. Both should already be clamped to `[SPLIT_MIN, SPLIT_MAX]`.
///
/// The sidebar width gets a final `[SIDEBAR_MIN_COLS, SIDEBAR_MAX_COLS]`
/// clamp so it never collapses below readable on narrow terminals
/// nor dominates an ultra-wide monitor (where 40% of 400 cols =
/// 160 cols sidebar, four times what the longest row needs).
pub(crate) fn pane_areas(
    area: Rect,
    sidebar_pct: u16,
    right_top_pct: u16,
) -> (Rect, Rect, Rect) {
    // Resolve the percentage → absolute cols, then clamp. Doing the
    // clamp here (rather than mutating `sidebar_pct`) preserves the
    // user's persisted preference: if they nudge to 60% on a wide
    // monitor it stays 60%, but the rendered sidebar still caps at
    // SIDEBAR_MAX_COLS. Resize the terminal smaller and the
    // sidebar shrinks back toward their 60%.
    let preferred = (area.width as u32 * sidebar_pct as u32 / 100) as u16;
    let sidebar_cols = preferred
        .clamp(SIDEBAR_MIN_COLS, SIDEBAR_MAX_COLS)
        .min(area.width);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(sidebar_cols), Constraint::Min(0)])
        .split(area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(right_top_pct), Constraint::Min(0)])
        .split(cols[1]);
    (cols[0], rows[0], rows[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 100, 50)
    }

    fn ctx() -> LayoutCtx {
        let mut c = LayoutCtx::new();
        c.last_area = area();
        c
    }

    #[test]
    fn defaults_are_in_range() {
        let c = LayoutCtx::new();
        assert_eq!(c.sidebar_pct, DEFAULT_SIDEBAR_PCT);
        assert_eq!(c.right_top_pct, DEFAULT_RIGHT_TOP_PCT);
        assert!(c.sidebar_pct >= SPLIT_MIN && c.sidebar_pct <= SPLIT_MAX);
        assert!(c.right_top_pct >= SPLIT_MIN && c.right_top_pct <= SPLIT_MAX);
        assert!(c.active_drag.is_none());
    }

    #[test]
    fn apply_persisted_clamps_into_legal_range() {
        let mut c = LayoutCtx::new();
        // Below min → clamped up.
        c.apply_persisted(Some(0), Some(0));
        assert_eq!(c.sidebar_pct, SPLIT_MIN);
        assert_eq!(c.right_top_pct, SPLIT_MIN);
        // Above max → clamped down.
        c.apply_persisted(Some(99), Some(99));
        assert_eq!(c.sidebar_pct, SPLIT_MAX);
        assert_eq!(c.right_top_pct, SPLIT_MAX);
        // None leaves the existing value alone.
        c.apply_persisted(None, None);
        assert_eq!(c.sidebar_pct, SPLIT_MAX);
    }

    #[test]
    fn nudge_widens_and_narrows_sidebar() {
        let mut c = LayoutCtx::new();
        let start = c.sidebar_pct;
        // We don't assert the persisted side-effect — that's a YAML
        // write that's tested elsewhere.
        let _ = c.nudge_splits(SPLIT_STEP, 0);
        assert_eq!(c.sidebar_pct, start + SPLIT_STEP as u16);
        let _ = c.nudge_splits(-SPLIT_STEP, 0);
        assert_eq!(c.sidebar_pct, start);
    }

    #[test]
    fn nudge_returns_false_when_clamped_against_a_wall() {
        let mut c = LayoutCtx::new();
        c.sidebar_pct = SPLIT_MAX;
        c.right_top_pct = SPLIT_MAX;
        // Already at the ceiling on both axes — no change, no
        // redraw, no YAML write.
        assert!(!c.nudge_splits(SPLIT_STEP, SPLIT_STEP));
    }

    #[test]
    fn hit_test_finds_the_vertical_splitter() {
        let c = ctx();
        let (sidebar, right_top, _) = pane_areas(area(), c.sidebar_pct, c.right_top_pct);
        // Hover one cell right of the sidebar's right edge → vertical splitter.
        let v_x = sidebar.x + sidebar.width;
        assert_eq!(
            c.hit_test_splitter(v_x, 10, sidebar, right_top),
            Some(DragTarget::SidebarRight)
        );
    }

    #[test]
    fn hit_test_finds_the_horizontal_splitter() {
        let c = ctx();
        let (sidebar, right_top, _) = pane_areas(area(), c.sidebar_pct, c.right_top_pct);
        let h_y = right_top.y + right_top.height;
        assert_eq!(
            c.hit_test_splitter(right_top.x + 5, h_y, sidebar, right_top),
            Some(DragTarget::ActivityTerminals)
        );
    }

    #[test]
    fn hit_test_misses_inside_a_pane() {
        let c = ctx();
        let (sidebar, right_top, _) = pane_areas(area(), c.sidebar_pct, c.right_top_pct);
        // Middle of the sidebar — not on any splitter.
        assert_eq!(c.hit_test_splitter(2, 10, sidebar, right_top), None);
    }

    #[test]
    fn update_drag_moves_sidebar_to_drop_column() {
        let mut c = ctx();
        // Drop at column 25 out of 100 → ~25% sidebar.
        let changed = c.update_drag(DragTarget::SidebarRight, 25, 10);
        assert!(changed);
        assert_eq!(c.sidebar_pct, 25);
    }

    #[test]
    fn update_drag_clamps_to_split_max() {
        let mut c = ctx();
        // Way past the right edge — clamps to SPLIT_MAX.
        let changed = c.update_drag(DragTarget::SidebarRight, 95, 10);
        assert!(changed);
        assert_eq!(c.sidebar_pct, SPLIT_MAX);
    }

    #[test]
    fn update_drag_returns_false_when_pct_unchanged() {
        let mut c = ctx();
        let start = c.sidebar_pct;
        // Drop at the column already corresponding to the current pct.
        let target_col = (start as u32 * c.last_area.width as u32 / 100) as u16;
        let _ = c.update_drag(DragTarget::SidebarRight, target_col, 10);
        // Second drag at the same column → no change → false.
        let changed = c.update_drag(DragTarget::SidebarRight, target_col, 10);
        assert!(!changed);
    }
}
