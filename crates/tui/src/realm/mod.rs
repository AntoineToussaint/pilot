//! Pilot's UI on `tuirealm`. Lives parallel to the `tui-kit`-based
//! tree (`crate::app`, `crate::components`) during the migration;
//! once every pane + modal is ported here and the new `app::run`
//! works end-to-end, the old code is deleted and `crate::tui_kit`
//! becomes unused.
//!
//! ## How it's organized
//!
//! ```text
//! realm/
//! ├── mod.rs         this file — re-exports + the Msg/Id types
//! ├── model.rs       the Application + main loop (pilot's `App`
//! │                  equivalent under tuirealm)
//! └── components/    one file per pane / modal port
//!     ├── splash.rs
//!     ├── error.rs
//!     ├── ...
//! ```
//!
//! ## Naming conventions during the migration
//!
//! - Old `Pane`/`Modal` impls live in `crate::components::*` and use
//!   `tui_kit::*`. **Don't touch them** — they're still load-bearing
//!   for `crate::app::run`.
//! - New ports live in `crate::realm::components::*` and use
//!   `tuirealm::*`. Reuse render functions / state structs from the
//!   old impls where it's clean — duplicate them and edit when it's
//!   not.
//!
//! ## What's pilot-domain (stays) vs framework-shaped (rewires)
//!
//! - **State + render bodies + helpers** — copy verbatim from
//!   `crate::components::*`. The ratatui calls work identically
//!   inside `Component::view`.
//! - **Trait impl + key routing** — rewrite. `Pane::handle_key
//!   → PaneOutcome` becomes `AppComponent::on(&Event) → Option<Msg>`.

pub mod components;
pub mod keymap;
pub mod model;
pub mod user_event;

pub use model::{Id, Model, Msg};
pub use user_event::UserEvent;
