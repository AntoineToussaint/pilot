//! # pilot-core
//!
//! Generic domain types for pilot. Source-agnostic: nothing here knows about
//! GitHub, Linear, or any specific provider.

pub mod agent;
pub mod provider;
mod session;
mod session_key;
mod task;
pub mod time;

pub use agent::AgentConfig;
pub use provider::{ProviderError, TaskProvider};
pub use session::*;
pub use session_key::SessionKey;
pub use task::*;
