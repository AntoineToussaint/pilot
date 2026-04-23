//! Concrete UI components. Each module implements `Component` for a
//! specific piece of the TUI (sidebar, right pane, overlays, etc.).

pub mod right_pane;
pub mod sidebar;

pub use right_pane::RightPane;
pub use sidebar::{Mailbox, Sidebar};
