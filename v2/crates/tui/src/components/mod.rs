//! Concrete UI components. Each module implements `Component` for a
//! specific piece of the TUI (sidebar, right pane, overlays, etc.).

pub mod sidebar;

pub use sidebar::{Mailbox, Sidebar};
