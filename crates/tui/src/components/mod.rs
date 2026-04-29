//! Concrete UI components. Each module implements `Component` for a
//! specific piece of the TUI (sidebar, right pane, overlays, etc.).

pub mod overlays;
pub mod right_pane;
pub mod root;
pub mod sidebar;
pub mod terminal_stack;

pub use overlays::{Help, NewWorktree, NewWorktreeResult};
pub use right_pane::RightPane;
pub use root::Root;
pub use sidebar::{Mailbox, Sidebar, VisibleRow};
pub use terminal_stack::TerminalStack;
