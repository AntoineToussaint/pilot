//! pilot-tui — the client TUI: realm-based component tree, key
//! routing, event dispatch, rendering.
//!
//! Built on `tuirealm` 4.1 (which sits on `ratatui`); modal /
//! component / orchestrator types live under `crate::realm`. Pilot's
//! domain components (Sidebar, RightPane, TerminalStack, Mailbox,
//! activity-feed renderers, status pills) live under
//! `crate::components`.

pub mod components;
pub mod latch_set;
pub mod pane;
pub mod pilot_theme;
pub mod realm;
pub mod setup;
pub mod setup_flow;
pub mod test_mode;
pub mod theme;

// ── re-exported from pilot-tui-core ─────────────────────────────
// These modules used to live here; they were extracted into
// `pilot-tui-core` so edits to (say) `intent.rs` don't trigger a
// pilot-tui rebuild. Re-exported at the same paths so existing
// `pilot_tui::intent::Foo` / `crate::intent::Foo` keeps resolving.
pub use pilot_tui_core::{
    agent_attention, confirm_latch, editors, intent, platform, prompts, util,
};

pub use pane::{Binding, DetachSpec, PaneId, PaneOutcome};
pub use theme::Theme;
