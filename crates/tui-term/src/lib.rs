//! # pilot-tui-term
//!
//! Embedded terminal widget for ratatui, powered by libghostty-vt.
//! Supports both local PTY sessions and remote daemon-backed sessions.

mod ghostty_widget;
pub mod local;
pub mod remote;

pub use ghostty_widget::GhosttyTerminal;
pub use local::{LocalTermSession, TermError};
pub use portable_pty::PtySize;
pub use remote::RemoteTermSession;

use std::path::Path;
use std::time::Instant;

/// A terminal session — either local (direct PTY) or remote (via daemon).
pub enum TermSession {
    Local(LocalTermSession),
    Remote(RemoteTermSession),
}

impl TermSession {
    /// Spawn a local PTY session (direct, no daemon).
    pub fn spawn_local(
        cmd: &[&str],
        size: PtySize,
        cwd: Option<&Path>,
        env: Vec<(String, String)>,
    ) -> Result<Self, TermError> {
        LocalTermSession::spawn(cmd, size, cwd, env).map(TermSession::Local)
    }

    /// Connect to daemon and spawn a remote session.
    pub fn spawn_remote(
        socket_path: &Path,
        session_id: &str,
        cmd: &[&str],
        size: PtySize,
        cwd: Option<&Path>,
        env: Vec<(String, String)>,
    ) -> Result<Self, TermError> {
        RemoteTermSession::connect(socket_path, session_id, cmd, size, cwd, env)
            .map(TermSession::Remote)
    }

    pub fn process_pending(&mut self) -> bool {
        match self {
            Self::Local(s) => s.process_pending(),
            Self::Remote(s) => s.process_pending(),
        }
    }

    pub fn write(&mut self, data: &[u8]) -> Result<(), TermError> {
        match self {
            Self::Local(s) => s.write(data),
            Self::Remote(s) => s.write(data),
        }
    }

    pub fn resize(&mut self, size: PtySize) -> Result<(), TermError> {
        match self {
            Self::Local(s) => s.resize(size),
            Self::Remote(s) => s.resize(size),
        }
    }

    pub fn is_finished(&self) -> bool {
        match self {
            Self::Local(s) => s.is_finished(),
            Self::Remote(s) => s.is_finished(),
        }
    }

    pub fn last_output_at(&self) -> Instant {
        match self {
            Self::Local(s) => s.last_output_at(),
            Self::Remote(s) => s.last_output_at(),
        }
    }

    pub fn recent_output(&self) -> &[u8] {
        match self {
            Self::Local(s) => s.recent_output(),
            Self::Remote(s) => s.recent_output(),
        }
    }

    pub fn size(&self) -> PtySize {
        match self {
            Self::Local(s) => s.size(),
            Self::Remote(s) => s.size(),
        }
    }

    pub fn render_data(
        &mut self,
    ) -> (
        &mut libghostty_vt::Terminal<'static, 'static>,
        &mut libghostty_vt::RenderState<'static>,
        &mut libghostty_vt::render::RowIterator<'static>,
        &mut libghostty_vt::render::CellIterator<'static>,
    ) {
        match self {
            Self::Local(s) => s.render_data(),
            Self::Remote(s) => s.render_data(),
        }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        match self {
            Self::Local(s) => s.scroll_up(lines),
            Self::Remote(s) => s.scroll_up(lines),
        }
    }

    pub fn scroll_down(&mut self, lines: usize) {
        match self {
            Self::Local(s) => s.scroll_down(lines),
            Self::Remote(s) => s.scroll_down(lines),
        }
    }

    pub fn scroll_reset(&mut self) {
        match self {
            Self::Local(s) => s.scroll_reset(),
            Self::Remote(s) => s.scroll_reset(),
        }
    }

    pub fn is_scrolled(&self) -> bool {
        match self {
            Self::Local(s) => s.is_scrolled(),
            Self::Remote(s) => s.is_scrolled(),
        }
    }
}

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
