//! pilot-v2-tui — the client TUI: component tree, key routing, event
//! dispatch, rendering.
//!
//! The architectural bet of v2 is in this crate: a single-path focus
//! tree of `Component`s, replacing the four-slot state-desync mess of
//! v1 (`state.selected` / `panes.focused` / `terminal_index.active_tab`
//! / pane-tree target keys). Focus is ONE id; every other position is
//! derived from it.
//!
//! Modules:
//! - `component` — the `Component` trait and its surrounding types
//!   (`ComponentId`, `Outcome`).
//! - `tree` — `ComponentTree`, the owner of components, parent/child
//!   edges, and focus state. Owns the key and event dispatch loops.

pub mod app;
pub mod component;
pub mod components;
pub mod layout;
pub mod setup;
pub mod setup_flow;
pub mod test_mode;
pub mod tree;

pub use component::{Component, ComponentId, Outcome};
pub use tree::{ComponentTree, MountError};
