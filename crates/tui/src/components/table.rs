//! Minimal table layout for sidebar / right-pane row rendering.
//!
//! Pilot's "tabular" views (sidebar workspaces, activity cards) had
//! been hand-laying-out columns inline in their render functions:
//! ad-hoc padding, hardcoded widths, jitter when content widths
//! varied across rows (`#7204 R` vs `#31 R` putting the `R` at
//! different x positions). This module owns the column-width
//! arithmetic so renderers describe COLUMNS once and feed CELLS per
//! row — width is computed across all rows at the top of the pass.
//!
//! The public surface is intentionally small:
//!
//! - [`Column`] — one column's width strategy + minimum width.
//! - [`compute_widths`] — pure function: given a column spec and
//!   the natural width of each cell in each row, produce the final
//!   per-column width that respects `total_width`. Single source of
//!   truth for "which column eats the slack."
//!
//! Renderers stay in charge of producing `Span`s (theming + ratatui
//! style logic varies enough per surface that a single Cell type
//! would over-abstract). They call `compute_widths` once, then pad
//! each cell to the resulting width as they build their Line.

/// How wide a column should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnWidth {
    /// Always this many cells, regardless of content. Status pills
    /// and time columns use this — they have a stable max-width
    /// known up-front.
    Fixed(usize),
    /// At least `min` cells, expands to the widest cell across all
    /// rows. PR number column uses this: `#31` → 3 cells, `#7204` →
    /// 5 cells, all rows pad to whichever wins.
    Max { min: usize },
    /// Absorbs remaining horizontal space after Fixed + Max columns
    /// are subtracted from `total_width`. Title column uses this.
    /// If multiple Flex columns share a row, each gets an equal
    /// share. Truncates rather than overflowing.
    Flex { min: usize },
}

/// One column's spec.
#[derive(Debug, Clone, Copy)]
pub struct Column {
    pub width: ColumnWidth,
}

impl Column {
    pub fn fixed(width: usize) -> Self {
        Self {
            width: ColumnWidth::Fixed(width),
        }
    }

    pub fn max(min: usize) -> Self {
        Self {
            width: ColumnWidth::Max { min },
        }
    }

    pub fn flex(min: usize) -> Self {
        Self {
            width: ColumnWidth::Flex { min },
        }
    }
}

/// Compute the final per-column width given the column specs, the
/// natural per-cell content widths (rows × cols), and the total
/// available row width.
///
/// Algorithm:
///   1. Fixed columns take their declared width.
///   2. Max columns take `max(min, max-across-rows-of-cell-width)`.
///   3. Flex columns share whatever's left equally, never less
///      than their `min`. When the table is narrower than the sum
///      of fixed+max+flex-min, Flex columns return their `min` and
///      the row simply overflows — caller's choice whether to clip.
///
/// `cell_widths[row][col]` is the natural width of the cell's
/// content in cells. Empty for an empty table.
pub fn compute_widths(
    columns: &[Column],
    cell_widths: &[Vec<usize>],
    total_width: usize,
) -> Vec<usize> {
    let n = columns.len();
    let mut widths = vec![0usize; n];

    let mut consumed = 0usize;
    let mut flex_indices: Vec<usize> = Vec::new();

    for (i, col) in columns.iter().enumerate() {
        match col.width {
            ColumnWidth::Fixed(w) => {
                widths[i] = w;
                consumed = consumed.saturating_add(w);
            }
            ColumnWidth::Max { min } => {
                let max_observed = cell_widths
                    .iter()
                    .map(|row| row.get(i).copied().unwrap_or(0))
                    .max()
                    .unwrap_or(0);
                widths[i] = max_observed.max(min);
                consumed = consumed.saturating_add(widths[i]);
            }
            ColumnWidth::Flex { min } => {
                flex_indices.push(i);
                widths[i] = min;
                consumed = consumed.saturating_add(min);
            }
        }
    }

    // Distribute remaining space across Flex columns. If consumed
    // already exceeds total_width, every flex stays at min and the
    // caller deals with overflow (truncation).
    if flex_indices.is_empty() || consumed >= total_width {
        return widths;
    }
    let remaining = total_width - consumed;
    let per_flex = remaining / flex_indices.len();
    let leftover = remaining % flex_indices.len();
    for (idx, &col_idx) in flex_indices.iter().enumerate() {
        widths[col_idx] += per_flex;
        if idx < leftover {
            widths[col_idx] += 1;
        }
    }
    widths
}

/// One cell — a sequence of pre-styled spans the caller assembled.
/// `width()` is the visible cell count; `render_table` uses it to
/// know how much padding to add or where to truncate.
#[derive(Debug, Clone, Default)]
pub struct Cell {
    pub spans: Vec<ratatui::text::Span<'static>>,
}

impl Cell {
    pub fn new(spans: Vec<ratatui::text::Span<'static>>) -> Self {
        Self { spans }
    }

    pub fn from_span(span: ratatui::text::Span<'static>) -> Self {
        Self { spans: vec![span] }
    }

    pub fn empty() -> Self {
        Self::default()
    }

    /// Total visible width of the cell — sum of all span widths in
    /// display cells (not bytes). Uses unicode-width via
    /// `crate::util::visual_width` so multi-byte glyphs count once.
    pub fn width(&self) -> usize {
        self.spans
            .iter()
            .map(|s| crate::util::visual_width(s.content.as_ref()))
            .sum()
    }
}

/// One row's cells. The slice order matches the `Column` slice
/// passed to `render_table`. Rows can have fewer cells than columns
/// (missing cells render as empty padded space).
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub cells: Vec<Cell>,
}

impl Row {
    pub fn new(cells: Vec<Cell>) -> Self {
        Self { cells }
    }
}

/// Render a table of rows to ratatui Lines, with each cell padded
/// (right) to its computed column width. Cells wider than their
/// column get their last span truncated with `…`. This is the
/// renderer half of the abstraction — `compute_widths` does the
/// math, this turns rows into Lines. Sidebar / right_pane consume
/// it instead of hand-stitching span sequences with hardcoded
/// padding.
pub fn render_table(
    rows: &[Row],
    columns: &[Column],
    total_width: usize,
) -> Vec<ratatui::text::Line<'static>> {
    let cell_widths: Vec<Vec<usize>> = rows
        .iter()
        .map(|r| r.cells.iter().map(|c| c.width()).collect())
        .collect();
    let widths = compute_widths(columns, &cell_widths, total_width);

    rows.iter()
        .map(|row| render_row(row, &widths))
        .collect()
}

fn render_row(row: &Row, widths: &[usize]) -> ratatui::text::Line<'static> {
    let mut spans: Vec<ratatui::text::Span<'static>> = Vec::new();
    for (i, target_w) in widths.iter().enumerate() {
        let empty = Cell::empty();
        let cell = row.cells.get(i).unwrap_or(&empty);
        let cell_w = cell.width();
        if cell_w <= *target_w {
            spans.extend(cell.spans.iter().cloned());
            if cell_w < *target_w {
                spans.push(ratatui::text::Span::raw(" ".repeat(*target_w - cell_w)));
            }
        } else {
            // Truncate: walk spans until we've consumed `target_w - 1`
            // cells, then push a `…` to mark the cut.
            let mut consumed = 0usize;
            let budget = target_w.saturating_sub(1);
            for span in &cell.spans {
                let span_w = crate::util::visual_width(span.content.as_ref());
                if consumed + span_w <= budget {
                    spans.push(span.clone());
                    consumed += span_w;
                } else {
                    let remaining = budget - consumed;
                    if remaining > 0 {
                        let truncated: String = span
                            .content
                            .chars()
                            .scan(0usize, |w, ch| {
                                let cw = crate::util::char_visual_width(ch);
                                if *w + cw > remaining {
                                    return None;
                                }
                                *w += cw;
                                Some(ch)
                            })
                            .collect();
                        spans.push(ratatui::text::Span::styled(truncated, span.style));
                    }
                    break;
                }
            }
            spans.push(ratatui::text::Span::raw("…"));
        }
    }
    ratatui::text::Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;

    #[test]
    fn fixed_takes_declared_width() {
        let cols = [Column::fixed(5), Column::fixed(10)];
        let widths = compute_widths(&cols, &[], 100);
        assert_eq!(widths, vec![5, 10]);
    }

    #[test]
    fn max_takes_widest_observed_clamped_to_min() {
        let cols = [Column::max(3)];
        // Rows have natural widths 1, 4, 2 in col 0.
        let widths = compute_widths(&cols, &[vec![1], vec![4], vec![2]], 100);
        // max(3, 4) = 4.
        assert_eq!(widths, vec![4]);
    }

    #[test]
    fn max_uses_min_when_all_rows_narrower() {
        let cols = [Column::max(5)];
        let widths = compute_widths(&cols, &[vec![1], vec![2]], 100);
        assert_eq!(widths, vec![5]);
    }

    #[test]
    fn flex_absorbs_remaining_space() {
        // 100 total, fixed 10 + max 5 + flex = 100 → flex = 85.
        let cols = [Column::fixed(10), Column::max(5), Column::flex(1)];
        let widths = compute_widths(&cols, &[vec![0, 3, 0]], 100);
        assert_eq!(widths, vec![10, 5, 85]);
    }

    #[test]
    fn multiple_flex_columns_split_evenly_with_remainder_on_left() {
        // 100 - 10 = 90 remaining across 3 flexes → 30 each, no remainder.
        let cols = [Column::fixed(10), Column::flex(1), Column::flex(1), Column::flex(1)];
        let widths = compute_widths(&cols, &[], 100);
        assert_eq!(widths, vec![10, 30, 30, 30]);
    }

    #[test]
    fn multiple_flex_columns_distribute_remainder_left_to_right() {
        // 11 remaining across 3 flexes → 3 + 3 + 3 + 2 leftover →
        // leftover spreads to the leftmost flex columns.
        let cols = [Column::flex(1), Column::flex(1), Column::flex(1)];
        let widths = compute_widths(&cols, &[], 11);
        // per_flex = 3, leftover = 2 → cols 0,1 get +1.
        assert_eq!(widths, vec![4, 4, 3]);
    }

    #[test]
    fn flex_stays_at_min_when_overflow() {
        // Fixed 60 + flex_min 50 = 110, total = 100 → flex stays at 50,
        // caller clips.
        let cols = [Column::fixed(60), Column::flex(50)];
        let widths = compute_widths(&cols, &[], 100);
        assert_eq!(widths, vec![60, 50]);
    }

    #[test]
    fn empty_table_returns_min_or_declared() {
        let cols = [Column::fixed(5), Column::max(3), Column::flex(1)];
        let widths = compute_widths(&cols, &[], 20);
        // fixed=5, max=3 (no rows, uses min), flex=remaining=12.
        assert_eq!(widths, vec![5, 3, 12]);
    }

    /// Cell width sums its spans by visual cells, not bytes.
    #[test]
    fn cell_width_sums_span_widths() {
        let cell = Cell::new(vec![Span::raw("hi"), Span::raw(" ●")]);
        assert_eq!(cell.width(), 4);
    }

    /// Render pads short cells out to column width.
    #[test]
    fn render_pads_short_cells_to_column_width() {
        let cols = [Column::fixed(5), Column::fixed(3)];
        let rows = vec![Row::new(vec![
            Cell::from_span(Span::raw("a")),
            Cell::from_span(Span::raw("b")),
        ])];
        let lines = render_table(&rows, &cols, 20);
        // "a" + 4 spaces + "b" + 2 spaces = 8 cells across the two columns.
        let joined: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(joined, "a    b  ");
    }

    /// Render truncates over-wide cells with an ellipsis.
    #[test]
    fn render_truncates_overlong_cell_with_ellipsis() {
        let cols = [Column::fixed(5)];
        let rows = vec![Row::new(vec![Cell::from_span(Span::raw("hello world"))])];
        let lines = render_table(&rows, &cols, 5);
        let joined: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        // "hell" + "…" = 5 cells.
        assert_eq!(joined, "hell…");
    }

    /// Max column picks the widest cell across all rows.
    #[test]
    fn render_uses_widest_observed_for_max_column() {
        let cols = [Column::max(3)];
        let rows = vec![
            Row::new(vec![Cell::from_span(Span::raw("#1"))]),
            Row::new(vec![Cell::from_span(Span::raw("#7204"))]),
        ];
        let lines = render_table(&rows, &cols, 20);
        // Both rows pad to 5 cells (width of "#7204").
        let row0: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        let row1: String = lines[1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(row0, "#1   ");
        assert_eq!(row1, "#7204");
    }
}
