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

        let mut row_iter = match self.row_iter.update(self.snapshot) {
            Ok(r) => r,
            Err(_) => return,
        };

        let mut y = 0u16;
        while let Some(row) = row_iter.next() {
            if y >= area.height {
                break;
            }

            let mut cell_iter = match self.cell_iter.update(&row) {
                Ok(c) => c,
                Err(_) => {
                    y += 1;
                    continue;
                }
            };

            let mut x = 0u16;
            while let Some(cell) = cell_iter.next() {
                if x >= area.width {
                    break;
                }

                let graphemes = cell.graphemes().unwrap_or_default();
                let text: String = if graphemes.is_empty() {
                    " ".to_string()
                } else {
                    graphemes.iter().collect()
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

                // Cursor highlight.
                if let Some(ref cp) = cursor_pos {
                    if cp.x == x && cp.y == y {
                        style = style.add_modifier(Modifier::REVERSED);
                    }
                }

                let buf_x = area.x + x;
                let buf_y = area.y + y;
                if buf_x < area.x + area.width && buf_y < area.y + area.height {
                    buf[(buf_x, buf_y)].set_symbol(&text).set_style(style);
                }

                x += 1;
            }

            y += 1;
        }
    }
}
