//! pilot-tui — the client TUI: realm-based component tree, key
//! routing, event dispatch, rendering.
//!
//! Built on `tuirealm` 4.1 (which sits on `ratatui`); modal /
//! component / orchestrator types live under `crate::realm`. Pilot's
//! domain components (Sidebar, RightPane, TerminalStack, Mailbox,
//! activity-feed renderers, status pills) live under
//! `crate::components`.

pub mod components;
pub mod editors;
pub mod pane;
pub mod pilot_theme;
pub mod platform;
pub mod realm;
pub mod setup;
pub mod setup_flow;
pub mod test_mode;
pub mod theme;

pub use pane::{Binding, DetachSpec, PaneId, PaneOutcome};
pub use theme::Theme;
