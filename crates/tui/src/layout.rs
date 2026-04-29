//! Declarative layout tree.
//!
//! A `Node` describes WHERE components go in screen space; `resolve`
//! turns it into a flat `LayoutOutcome` with concrete `Rect`s for
//! every leaf and every interactive splitter. Both the renderer
//! (`app::draw`) and the mouse handler (`app::dispatch_mouse`) read
//! the same outcome — so what you see and what you can grab can
//! never drift.
//!
//! ## Why a tree
//!
//! v0 expressed layout imperatively in `draw()`: `Layout::horizontal`,
//! `Layout::vertical`, ad-hoc rect math. The mouse hit-test in
//! `dispatch_mouse` had to recompute the same coordinates from
//! `PaneLayout` — which meant any future layout tweak (status line
//! height, modal overlay, nested splits) needed two coordinated
//! edits or the hit zone would drift off the visible splitter.
//!
//! ## What's NOT in here
//!
//! - Component rendering. `Node::Component(id)` is a leaf marker; the
//!   renderer walks the outcome and calls `tree.render_one(id, rect)`.
//! - Overlays. The setup screen / Help / NewWorktree are mounted as
//!   root children and rendered after the main layout finishes —
//!   they own their own modal sizing, not the layout tree.
//! - Status line. `app::draw` carves a 1-row strip off the bottom
//!   BEFORE handing the rest to `resolve`. The status line is a
//!   cross-cutting concern (driven by AppState transients) so it
//!   doesn't fit the static layout-tree model.

use crate::ComponentId;
use ratatui::layout::Rect;

/// Identifier for an interactive splitter the user can drag. Every
/// `HSplit`/`VSplit` may attach one. None means "fixed split, not
/// draggable" — used for layout-only splits like the status line
/// boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SplitterId {
    /// Vertical splitter between the sidebar and the right column.
    Sidebar,
    /// Horizontal splitter inside the right column (activity feed
    /// vs terminal stack).
    RightVertical,
}

/// Tag for placeholder content the renderer should paint directly
/// without consulting the component tree. Lets the layout describe
/// "show the empty-state message here" declaratively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Placeholder {
    /// "Pilot is ready" onboarding message in the right column.
    EmptyRight,
}

#[derive(Debug, Clone, Copy)]
pub enum HSizing {
    /// First child gets a fixed-cell width; second gets the rest.
    LeftFixed(u16),
    /// First child gets `pct` percent of the available width
    /// (clamped 0..=100); second gets the remainder.
    LeftPct(u16),
}

#[derive(Debug, Clone, Copy)]
pub enum VSizing {
    TopFixed(u16),
    TopPct(u16),
}

/// One node in the layout tree.
#[derive(Debug, Clone)]
pub enum Node {
    /// Render a specific mounted component into this rect.
    Component(ComponentId),
    /// Render a placeholder. The renderer paints these directly.
    Placeholder(Placeholder),
    HSplit {
        splitter: Option<SplitterId>,
        left: Box<Node>,
        right: Box<Node>,
        sizing: HSizing,
    },
    VSplit {
        splitter: Option<SplitterId>,
        top: Box<Node>,
        bottom: Box<Node>,
        sizing: VSizing,
    },
}

impl Node {
    pub fn h_split(splitter: Option<SplitterId>, sizing: HSizing, left: Node, right: Node) -> Self {
        Self::HSplit {
            splitter,
            left: Box::new(left),
            right: Box::new(right),
            sizing,
        }
    }

    pub fn v_split(splitter: Option<SplitterId>, sizing: VSizing, top: Node, bottom: Node) -> Self {
        Self::VSplit {
            splitter,
            top: Box::new(top),
            bottom: Box::new(bottom),
            sizing,
        }
    }
}

/// One placed leaf or placeholder, with the rect it occupies.
#[derive(Debug, Clone, Copy)]
pub enum Slot {
    Component(ComponentId, Rect),
    Placeholder(Placeholder, Rect),
}

/// Result of `Node::resolve`. The slot list is in pre-order; the
/// splitter map is keyed for direct lookup by hit-test.
#[derive(Debug, Clone, Default)]
pub struct LayoutOutcome {
    pub slots: Vec<Slot>,
    /// Splitter rect by id. Each rect is exactly 1 cell thick on the
    /// drag axis — `hit_test` widens this with a tolerance band.
    pub splitters: Vec<(SplitterId, Rect)>,
}

impl LayoutOutcome {
    pub fn splitter_rect(&self, id: SplitterId) -> Option<Rect> {
        self.splitters
            .iter()
            .find(|(sid, _)| *sid == id)
            .map(|(_, r)| *r)
    }
}

impl Node {
    /// Compute the layout against `area`. Pure function — same input
    /// always yields the same output, no I/O.
    pub fn resolve(&self, area: Rect) -> LayoutOutcome {
        let mut out = LayoutOutcome::default();
        self.resolve_into(area, &mut out);
        out
    }

    fn resolve_into(&self, area: Rect, out: &mut LayoutOutcome) {
        match self {
            Node::Component(id) => out.slots.push(Slot::Component(*id, area)),
            Node::Placeholder(p) => out.slots.push(Slot::Placeholder(*p, area)),
            Node::HSplit {
                splitter,
                left,
                right,
                sizing,
            } => {
                let left_w = match *sizing {
                    HSizing::LeftFixed(w) => w.min(area.width),
                    HSizing::LeftPct(pct) => {
                        ((area.width as u32 * pct.min(100) as u32) / 100) as u16
                    }
                };
                let right_w = area.width.saturating_sub(left_w);
                let left_rect = Rect {
                    x: area.x,
                    y: area.y,
                    width: left_w,
                    height: area.height,
                };
                let right_rect = Rect {
                    x: area.x + left_w,
                    y: area.y,
                    width: right_w,
                    height: area.height,
                };
                // Register the splitter BEFORE recursing into
                // children so outer splits take priority at corners
                // where two splitter bands meet — `hit_test` walks
                // first-match-wins.
                if let Some(sid) = splitter {
                    out.splitters.push((
                        *sid,
                        Rect {
                            x: area.x + left_w,
                            y: area.y,
                            width: 1,
                            height: area.height,
                        },
                    ));
                }
                left.resolve_into(left_rect, out);
                right.resolve_into(right_rect, out);
            }
            Node::VSplit {
                splitter,
                top,
                bottom,
                sizing,
            } => {
                let top_h = match *sizing {
                    VSizing::TopFixed(h) => h.min(area.height),
                    VSizing::TopPct(pct) => {
                        ((area.height as u32 * pct.min(100) as u32) / 100) as u16
                    }
                };
                let bottom_h = area.height.saturating_sub(top_h);
                let top_rect = Rect {
                    x: area.x,
                    y: area.y,
                    width: area.width,
                    height: top_h,
                };
                let bottom_rect = Rect {
                    x: area.x,
                    y: area.y + top_h,
                    width: area.width,
                    height: bottom_h,
                };
                if let Some(sid) = splitter {
                    out.splitters.push((
                        *sid,
                        Rect {
                            x: area.x,
                            y: area.y + top_h,
                            width: area.width,
                            height: 1,
                        },
                    ));
                }
                top.resolve_into(top_rect, out);
                bottom.resolve_into(bottom_rect, out);
            }
        }
    }
}

/// Find the splitter at `(x, y)` with `tolerance` cells of slack
/// in the perpendicular axis (so 1-pixel splitters don't feel
/// hostile). First splitter to match wins; later splitters can be
/// targeted by clicking outside the earlier one's tolerance band.
pub fn hit_test(out: &LayoutOutcome, x: u16, y: u16, tolerance: u16) -> Option<SplitterId> {
    for (sid, rect) in &out.splitters {
        if rect_contains_with_tolerance(*rect, x, y, tolerance) {
            return Some(*sid);
        }
    }
    None
}

fn rect_contains_with_tolerance(r: Rect, x: u16, y: u16, tol: u16) -> bool {
    // Splitters are always 1 cell thick on one axis — that's where
    // we apply the tolerance. The other axis stays untouched so
    // clicks outside the splitter's length don't accidentally
    // resize.
    let (rx, rw, ry, rh) = if r.width == 1 {
        (
            r.x.saturating_sub(tol),
            r.width.saturating_add(2 * tol),
            r.y,
            r.height,
        )
    } else if r.height == 1 {
        (
            r.x,
            r.width,
            r.y.saturating_sub(tol),
            r.height.saturating_add(2 * tol),
        )
    } else {
        (r.x, r.width, r.y, r.height)
    };
    x >= rx && x < rx.saturating_add(rw) && y >= ry && y < ry.saturating_add(rh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ComponentId;

    fn area(w: u16, h: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }

    fn cid(n: u64) -> ComponentId {
        ComponentId::new(n)
    }

    // ── HSplit ────────────────────────────────────────────────────

    #[test]
    fn hsplit_fixed_left_yields_correct_rects() {
        let n = Node::h_split(
            Some(SplitterId::Sidebar),
            HSizing::LeftFixed(32),
            Node::Component(cid(1)),
            Node::Component(cid(2)),
        );
        let out = n.resolve(area(100, 40));
        assert_eq!(out.slots.len(), 2);
        if let Slot::Component(_, r) = out.slots[0] {
            assert_eq!(
                r,
                Rect {
                    x: 0,
                    y: 0,
                    width: 32,
                    height: 40
                }
            );
        } else {
            panic!("expected first leaf to be sidebar");
        }
        if let Slot::Component(_, r) = out.slots[1] {
            assert_eq!(
                r,
                Rect {
                    x: 32,
                    y: 0,
                    width: 68,
                    height: 40
                }
            );
        }
        let sp = out.splitter_rect(SplitterId::Sidebar).unwrap();
        assert_eq!(
            sp,
            Rect {
                x: 32,
                y: 0,
                width: 1,
                height: 40
            }
        );
    }

    #[test]
    fn hsplit_pct_yields_proportional_rects() {
        let n = Node::h_split(
            None,
            HSizing::LeftPct(25),
            Node::Component(cid(1)),
            Node::Component(cid(2)),
        );
        let out = n.resolve(area(100, 40));
        if let Slot::Component(_, r) = out.slots[0] {
            assert_eq!(r.width, 25);
        }
        if let Slot::Component(_, r) = out.slots[1] {
            assert_eq!(r.width, 75);
        }
        assert!(out.splitters.is_empty(), "no splitter id → not interactive");
    }

    #[test]
    fn hsplit_clamps_left_fixed_to_total_width() {
        let n = Node::h_split(
            None,
            HSizing::LeftFixed(120),
            Node::Component(cid(1)),
            Node::Component(cid(2)),
        );
        let out = n.resolve(area(80, 10));
        if let Slot::Component(_, r) = out.slots[0] {
            assert_eq!(r.width, 80);
        }
        if let Slot::Component(_, r) = out.slots[1] {
            assert_eq!(r.width, 0, "right side gets nothing when left swallows all");
        }
    }

    // ── VSplit ────────────────────────────────────────────────────

    #[test]
    fn vsplit_pct_yields_proportional_rects() {
        let n = Node::v_split(
            Some(SplitterId::RightVertical),
            VSizing::TopPct(25),
            Node::Component(cid(1)),
            Node::Component(cid(2)),
        );
        let out = n.resolve(area(60, 40));
        if let Slot::Component(_, r) = out.slots[0] {
            assert_eq!(r.height, 10);
        }
        if let Slot::Component(_, r) = out.slots[1] {
            assert_eq!(r.height, 30);
        }
        let sp = out.splitter_rect(SplitterId::RightVertical).unwrap();
        assert_eq!(sp.y, 10);
        assert_eq!(sp.height, 1);
    }

    // ── Nested splits ─────────────────────────────────────────────

    #[test]
    fn nested_layout_produces_canonical_three_pane_arrangement() {
        // Sidebar | (Activity / Terminal) — the actual pilot layout.
        let n = Node::h_split(
            Some(SplitterId::Sidebar),
            HSizing::LeftFixed(32),
            Node::Component(cid(1)), // sidebar
            Node::v_split(
                Some(SplitterId::RightVertical),
                VSizing::TopPct(25),
                Node::Component(cid(2)), // activity
                Node::Component(cid(3)), // terminal
            ),
        );
        let out = n.resolve(area(120, 40));
        assert_eq!(out.slots.len(), 3, "three leaves placed");
        assert_eq!(out.splitters.len(), 2, "both splitters tracked");
    }

    // ── hit_test ──────────────────────────────────────────────────

    #[test]
    fn hit_test_finds_splitter_at_exact_position() {
        let n = Node::h_split(
            Some(SplitterId::Sidebar),
            HSizing::LeftFixed(32),
            Node::Component(cid(1)),
            Node::Component(cid(2)),
        );
        let out = n.resolve(area(100, 40));
        assert_eq!(hit_test(&out, 32, 5, 0), Some(SplitterId::Sidebar));
    }

    #[test]
    fn hit_test_with_tolerance_grows_grab_zone() {
        let n = Node::h_split(
            Some(SplitterId::Sidebar),
            HSizing::LeftFixed(32),
            Node::Component(cid(1)),
            Node::Component(cid(2)),
        );
        let out = n.resolve(area(100, 40));
        for col in [31, 32, 33] {
            assert_eq!(
                hit_test(&out, col, 5, 1),
                Some(SplitterId::Sidebar),
                "col {col} should match"
            );
        }
        assert!(hit_test(&out, 30, 5, 1).is_none());
        assert!(hit_test(&out, 34, 5, 1).is_none());
    }

    #[test]
    fn hit_test_returns_none_off_splitters() {
        let n = Node::h_split(
            Some(SplitterId::Sidebar),
            HSizing::LeftFixed(32),
            Node::Component(cid(1)),
            Node::Component(cid(2)),
        );
        let out = n.resolve(area(100, 40));
        assert!(hit_test(&out, 5, 5, 1).is_none());
        assert!(hit_test(&out, 80, 5, 1).is_none());
    }

    #[test]
    fn hit_test_horizontal_splitter_uses_y_tolerance() {
        let n = Node::v_split(
            Some(SplitterId::RightVertical),
            VSizing::TopPct(25),
            Node::Component(cid(1)),
            Node::Component(cid(2)),
        );
        let out = n.resolve(area(100, 40));
        // top_h = 40*25/100 = 10; splitter at y=10
        for row in [9, 10, 11] {
            assert_eq!(
                hit_test(&out, 50, row, 1),
                Some(SplitterId::RightVertical),
                "row {row} should match"
            );
        }
        assert!(hit_test(&out, 50, 8, 1).is_none());
        assert!(hit_test(&out, 50, 12, 1).is_none());
    }

    #[test]
    fn hit_test_priority_is_first_match() {
        // Two splitters that geometrically overlap → first registered
        // wins. With our pilot layout, the Sidebar HSplit is the
        // outermost, so its splitter goes in first.
        let n = Node::h_split(
            Some(SplitterId::Sidebar),
            HSizing::LeftFixed(32),
            Node::Component(cid(1)),
            Node::v_split(
                Some(SplitterId::RightVertical),
                VSizing::TopPct(25),
                Node::Component(cid(2)),
                Node::Component(cid(3)),
            ),
        );
        let out = n.resolve(area(100, 40));
        // (32, 10) sits on the corner where both splitter bands meet.
        assert_eq!(hit_test(&out, 32, 10, 1), Some(SplitterId::Sidebar));
    }
}
