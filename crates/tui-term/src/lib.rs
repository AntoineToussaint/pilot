//! # pilot-tui-term
//!
//! Embedded terminal widget for ratatui, powered by libghostty-vt.

mod ghostty_widget;
mod session;

pub use ghostty_widget::GhosttyTerminal;
pub use portable_pty::PtySize;
pub use session::TermSession;

/// Render a terminal session into a ratatui frame.
pub fn render_to_frame(
    term: &mut TermSession,
    frame: &mut ratatui::Frame,
    area: ratatui::prelude::Rect,
) {
    let (terminal, render_state, row_iter, cell_iter) = term.render_data();
    if let Ok(snapshot) = render_state.update(terminal) {
        let widget = GhosttyTerminal::new(&snapshot, row_iter, cell_iter);
        frame.render_widget(widget, area);
    }
}
