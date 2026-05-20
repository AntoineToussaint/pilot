//! Ratatui widget that renders a libghostty-vt terminal.
//!
//! # Dirty-state caching
//!
//! Naive render: walk every cell every frame. For a 200×60 terminal
//! that's ~12_000 cells × 4 FFI calls per cell — ~48k FFI hits per
//! frame. Scroll bursts make claude code re-render 10–30 times per
//! gesture, so the cell walk dominates the perceived "scroll feels
//! sluggish."
//!
//! libghostty exposes two layers of dirty tracking we exploit:
//!
//! 1. **`snapshot.dirty()`** — `Clean` / `Partial` / `Full`. When
//!    `Clean`, the entire viewport is byte-identical to the previous
//!    render — we can skip the cell walk entirely and copy the
//!    cached `shadow` into ratatui's buffer.
//! 2. **`row.dirty()`** — per-row flag. In `Partial`, most rows are
//!    unchanged; only the ones libghostty touched need the cell
//!    walk. Clean rows copy from the shadow.
//!
//! The shadow is a `ratatui::buffer::Buffer` we own per terminal
//! slot. Cursor highlight is NOT baked into the shadow — we apply
//! it as a `REVERSED` modifier to the final buffer after copying,
//! so a cursor move between frames doesn't leave a "ghost" cursor
//! at the previous position.

use libghostty_vt::render::{CellIterator, Dirty, RowIterator, Snapshot};
use libghostty_vt::style::Underline;
use ratatui::buffer::Buffer;
use ratatui::prelude::*;

/// A ratatui widget that renders a libghostty-vt terminal snapshot.
///
/// Constructed fresh per frame; the persistent caching lives in
/// the caller-supplied `shadow` buffer.
pub struct GhosttyTerminal<'a, 'alloc, 's> {
    snapshot: &'a Snapshot<'alloc, 's>,
    row_iter: &'a mut RowIterator<'alloc>,
    cell_iter: &'a mut CellIterator<'alloc>,
    shadow: &'a mut Option<Buffer>,
}

impl<'a, 'alloc, 's> GhosttyTerminal<'a, 'alloc, 's> {
    /// Construct the widget with a caller-owned shadow buffer slot.
    /// First call (or after a resize) initialises the shadow; later
    /// calls reuse it to skip clean rows.
    pub fn new(
        snapshot: &'a Snapshot<'alloc, 's>,
        row_iter: &'a mut RowIterator<'alloc>,
        cell_iter: &'a mut CellIterator<'alloc>,
        shadow: &'a mut Option<Buffer>,
    ) -> Self {
        Self {
            snapshot,
            row_iter,
            cell_iter,
            shadow,
        }
    }
}

impl Widget for GhosttyTerminal<'_, '_, '_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let colors = match self.snapshot.colors() {
            Ok(c) => c,
            Err(_) => return,
        };

        // Cursor position (pre-extracted so the cell loops don't
        // pay an Option-deref per cell). Applied at the end of
        // render — NOT written into the shadow — so a cursor move
        // between frames doesn't leave a ghost at the old position.
        let cursor_pos = self
            .snapshot
            .cursor_viewport()
            .ok()
            .flatten()
            .filter(|_| self.snapshot.cursor_visible().unwrap_or(false));

        // Shadow state. Resize / first-render = no usable shadow,
        // so we treat every row as dirty regardless of libghostty's
        // per-row flag.
        let shadow_needs_init = match self.shadow.as_ref() {
            Some(b) => b.area != area,
            None => true,
        };
        if shadow_needs_init {
            *self.shadow = Some(Buffer::empty(area));
        }
        let shadow = self
            .shadow
            .as_mut()
            .expect("shadow set above when needs_init");

        // Snapshot-level dirty: `Clean` means every cell matches
        // the last `RenderState::update` — we can blit the shadow
        // unchanged and skip the whole FFI dance.
        let snapshot_dirty = self.snapshot.dirty().unwrap_or(Dirty::Full);
        if !shadow_needs_init && snapshot_dirty == Dirty::Clean {
            blit_shadow(shadow, buf, area);
            apply_cursor_highlight(buf, area, cursor_pos);
            return;
        }
        let force_all_rows = shadow_needs_init || snapshot_dirty == Dirty::Full;

        let mut row_iter = match self.row_iter.update(self.snapshot) {
            Ok(r) => r,
            Err(_) => return,
        };

        // Per-cell buffers re-used across the loop. See
        // `feedback_use_abstractions` — avoid 12_000 per-frame
        // allocations of the same shape.
        let mut grapheme_buf: [char; 8] = [' '; 8];
        let mut text_buf = String::with_capacity(8);

        let mut y = 0u16;
        while let Some(row) = row_iter.next() {
            if y >= area.height {
                break;
            }
            let row_dirty = force_all_rows || row.dirty().unwrap_or(true);
            if !row_dirty {
                // Clean row → copy from shadow into the frame buf.
                // No FFI cell walk for this row. This is the win.
                copy_row_from_shadow(shadow, buf, area, y);
                y += 1;
                continue;
            }

            // Dirty row → cell-walk, write to BOTH shadow and buf.
            let mut cell_iter = match self.cell_iter.update(row) {
                Ok(c) => c,
                Err(_) => {
                    // Iterator failed — leave the row alone rather
                    // than blanking it. The shadow still holds the
                    // last good content for this row.
                    copy_row_from_shadow(shadow, buf, area, y);
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

                // Grapheme text — write into a stack buffer to
                // skip a per-cell Vec allocation.
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

                let fg_rgb = cell.fg_color().ok().flatten().unwrap_or(colors.foreground);
                let bg_rgb = cell.bg_color().ok().flatten().unwrap_or(colors.background);
                let fg = Color::Rgb(fg_rgb.r, fg_rgb.g, fg_rgb.b);
                let bg = Color::Rgb(bg_rgb.r, bg_rgb.g, bg_rgb.b);
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
                        style = ratatui::style::Style::default()
                            .fg(bg)
                            .bg(fg)
                            .add_modifier(style.add_modifier & Modifier::all());
                    }
                }

                let buf_x = area.x + x;
                if buf_x < area.x + area.width && buf_y < area.y + area.height {
                    // Write to BOTH the live buf and the shadow.
                    // Shadow stays cursor-free — highlight is
                    // applied at the end, only to the live buf.
                    buf[(buf_x, buf_y)].set_symbol(text).set_style(style);
                    shadow[(buf_x, buf_y)].set_symbol(text).set_style(style);
                }

                x += 1;
            }
            // Pad any cells beyond the cell iterator's end with the
            // background color, so a row that shrank doesn't leave
            // stale shadow content visible.
            while x < area.width {
                let buf_x = area.x + x;
                let bg = Color::Rgb(
                    colors.background.r,
                    colors.background.g,
                    colors.background.b,
                );
                let fill = ratatui::style::Style::default().bg(bg);
                buf[(buf_x, buf_y)].set_symbol(" ").set_style(fill);
                shadow[(buf_x, buf_y)].set_symbol(" ").set_style(fill);
                x += 1;
            }

            y += 1;
        }
        // Some rows past the iterator's end exist but were never
        // visited — pad them like the in-row case above so the
        // shadow stays in sync.
        while y < area.height {
            let buf_y = area.y + y;
            let bg = Color::Rgb(
                colors.background.r,
                colors.background.g,
                colors.background.b,
            );
            let fill = ratatui::style::Style::default().bg(bg);
            for x in 0..area.width {
                let buf_x = area.x + x;
                buf[(buf_x, buf_y)].set_symbol(" ").set_style(fill);
                shadow[(buf_x, buf_y)].set_symbol(" ").set_style(fill);
            }
            y += 1;
        }

        apply_cursor_highlight(buf, area, cursor_pos);
    }
}

/// Copy the entire shadow buffer onto the live buf. Used when the
/// snapshot reports `Dirty::Clean` — fastest possible "render."
fn blit_shadow(shadow: &Buffer, buf: &mut Buffer, area: Rect) {
    for y in 0..area.height {
        for x in 0..area.width {
            let px = area.x + x;
            let py = area.y + y;
            buf[(px, py)] = shadow[(px, py)].clone();
        }
    }
}

/// Copy one row from the shadow into the live buf. Used when the
/// snapshot is `Dirty::Partial` and this row's `row.dirty()` is
/// false — we have a known-good cached render.
fn copy_row_from_shadow(shadow: &Buffer, buf: &mut Buffer, area: Rect, y: u16) {
    let py = area.y + y;
    for x in 0..area.width {
        let px = area.x + x;
        buf[(px, py)] = shadow[(px, py)].clone();
    }
}

/// Apply the cursor `REVERSED` modifier to the live buf only. The
/// shadow stays cursor-free so a future copy doesn't leave ghosts
/// at old cursor positions.
fn apply_cursor_highlight(
    buf: &mut Buffer,
    area: Rect,
    cursor: Option<libghostty_vt::render::CursorViewport>,
) {
    let Some(cp) = cursor else {
        return;
    };
    if cp.x >= area.width || cp.y >= area.height {
        return;
    }
    let px = area.x + cp.x;
    let py = area.y + cp.y;
    let cell = &mut buf[(px, py)];
    cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
}
