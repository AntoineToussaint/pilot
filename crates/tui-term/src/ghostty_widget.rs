//! Ratatui widget that renders a libghostty-vt terminal.

use libghostty_vt::render::{CellIterator, RowIterator, Snapshot};
use libghostty_vt::style::Underline;
use ratatui::prelude::*;

/// A ratatui widget that renders a libghostty-vt terminal snapshot.
///
/// Usage:
/// ```ignore
/// let (terminal, render_state, row_iter, cell_iter) = session.render_data();
/// let snapshot = render_state.update(terminal).unwrap();
/// let widget = GhosttyTerminal::new(&snapshot, row_iter, cell_iter);
/// frame.render_widget(widget, area);
/// ```
pub struct GhosttyTerminal<'a, 'alloc, 's> {
    snapshot: &'a Snapshot<'alloc, 's>,
    row_iter: &'a mut RowIterator<'alloc>,
    cell_iter: &'a mut CellIterator<'alloc>,
}

impl<'a, 'alloc, 's> GhosttyTerminal<'a, 'alloc, 's> {
    pub fn new(
        snapshot: &'a Snapshot<'alloc, 's>,
        row_iter: &'a mut RowIterator<'alloc>,
        cell_iter: &'a mut CellIterator<'alloc>,
    ) -> Self {
        Self {
            snapshot,
            row_iter,
            cell_iter,
        }
    }
}

impl Widget for GhosttyTerminal<'_, '_, '_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let colors = match self.snapshot.colors() {
            Ok(c) => c,
            Err(_) => return,
        };

        // Get cursor position for highlighting.
        let cursor_pos = self
            .snapshot
            .cursor_viewport()
            .ok()
            .flatten()
            .filter(|_| self.snapshot.cursor_visible().unwrap_or(false));
        let (cursor_x, cursor_y) = match cursor_pos {
            Some(cp) => (cp.x, cp.y),
            None => (u16::MAX, u16::MAX),
        };

        let mut row_iter = match self.row_iter.update(self.snapshot) {
            Ok(r) => r,
            Err(_) => return,
        };

        // Per-frame allocations hoisted to the top of the render —
        // a 200×60 terminal walks ~12_000 cells per frame; allocating
        // a `Vec` of graphemes + a `String` per cell was the
        // dominant cost in debug builds (24_000 allocs / frame).
        // We re-use two stack-ish buffers instead:
        // - `grapheme_buf`: receives the raw char(s) for the cell
        //   (most cells = 1 ASCII char; cap at 8 since libghostty's
        //   grapheme cluster cap is small).
        // - `text_buf`: the `&str` we hand to `Buffer::set_symbol`,
        //   re-cleared per cell. ratatui itself still clones it
        //   internally, but at least we don't double-allocate.
        let mut grapheme_buf: [char; 8] = [' '; 8];
        let mut text_buf = String::with_capacity(8);

        let mut y = 0u16;
        while let Some(row) = row_iter.next() {
            if y >= area.height {
                break;
            }

            let mut cell_iter = match self.cell_iter.update(row) {
                Ok(c) => c,
                Err(_) => {
                    y += 1;
                    continue;
                }
            };

            let buf_y = area.y + y;
            let mut x = 0u16;
            while let Some(cell) = cell_iter.next() {
                if x >= area.width {
                    break;
                }

                // Grapheme text. The two-FFI dance (`graphemes_len`
                // + `graphemes_buf`) is unavoidable per the libghostty
                // surface, but we skip the `Vec` allocation by
                // writing directly into the stack buffer.
                let glen = cell.graphemes_len().unwrap_or(0).min(grapheme_buf.len());
                let text: &str = if glen == 0 {
                    " "
                } else {
                    let _ = cell.graphemes_buf(&mut grapheme_buf[..glen]);
                    text_buf.clear();
                    for ch in &grapheme_buf[..glen] {
                        text_buf.push(*ch);
                    }
                    &text_buf
                };

                // Resolve colors.
                let fg_rgb = cell.fg_color().ok().flatten().unwrap_or(colors.foreground);
                let bg_rgb = cell.bg_color().ok().flatten().unwrap_or(colors.background);

                let fg = Color::Rgb(fg_rgb.r, fg_rgb.g, fg_rgb.b);
                let bg = Color::Rgb(bg_rgb.r, bg_rgb.g, bg_rgb.b);

                // Build style.
                let mut style = ratatui::style::Style::default().fg(fg).bg(bg);

                if let Ok(cell_style) = cell.style() {
                    if cell_style.bold {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if cell_style.italic {
                        style = style.add_modifier(Modifier::ITALIC);
                    }
                    if cell_style.underline != Underline::None {
                        style = style.add_modifier(Modifier::UNDERLINED);
                    }
                    if cell_style.strikethrough {
                        style = style.add_modifier(Modifier::CROSSED_OUT);
                    }
                    if cell_style.inverse {
                        // Swap fg/bg.
                        style = ratatui::style::Style::default()
                            .fg(bg)
                            .bg(fg)
                            .add_modifier(style.add_modifier & Modifier::all());
                    }
                }

                // Cursor highlight — compare against pre-extracted
                // x/y to avoid an `Option::as_ref` deref per cell.
                if cursor_x == x && cursor_y == y {
                    style = style.add_modifier(Modifier::REVERSED);
                }

                let buf_x = area.x + x;
                if buf_x < area.x + area.width && buf_y < area.y + area.height {
                    buf[(buf_x, buf_y)].set_symbol(text).set_style(style);
                }

                x += 1;
            }

            y += 1;
        }
    }
}
