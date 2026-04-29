//! # pilot-core
//!
//! Generic domain types for pilot. Source-agnostic: nothing here knows about
//! GitHub, Linear, or any specific provider.

pub mod agent;
pub mod config;
pub mod issue_links;
pub mod provider;
pub mod scope;
mod session;
mod session_key;
mod task;
pub mod time;
mod workspace;

pub use agent::AgentConfig;
pub use config::{KV_KEY_LAYOUT, KV_KEY_SETUP, PaneLayout, PersistedSetup, ProviderConfig};
pub use issue_links::{IssueLink, extract as extract_issue_links};
pub use provider::{ProviderError, TaskProvider};
pub use scope::{MockScopeSource, Scope, ScopeKind, ScopeSource};
pub use session::*;
pub use session_key::SessionKey;
pub use task::*;
pub use workspace::{
    Session as WorkspaceSession, SessionId, SessionKind, SessionRunState, Workspace, WorkspaceKey,
    WorkspaceState, workspace_key_for,
};
