//! Pilot domain components — pane structs (Sidebar, RightPane,
//! TerminalStack), the activity-feed renderer, status icons, task
//! labels. Realm modal/component ports live under `crate::realm`.

pub mod comment_render;
pub mod icons;
pub mod right_pane;
pub mod sidebar;
pub mod task_label;
pub mod terminal_stack;

pub use right_pane::RightPane;
pub use sidebar::{Mailbox, Sidebar, VisibleRow};
pub use terminal_stack::TerminalStack;
