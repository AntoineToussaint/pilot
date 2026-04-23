//! Agent abstractions — Claude Code, Codex, Cursor, or any CLI.
//!
//! An `Agent` is a recipe for (1) launching an AI coding tool inside a
//! worktree, (2) recognizing when it's working / idle / asking for
//! input, and (3) injecting prompts. Adding a new agent is one file.
//!
//! The `SessionWrapper` abstraction lives here too — tmux is the
//! default, but `screen`, `zellij`, or raw PTYs can slot in.

use std::path::Path;

pub mod agent;
pub mod session_wrapper;

pub use agent::{Agent, HookConfig, Registry, SpawnCtx};
pub use pilot_v2_ipc::AgentState;
pub use session_wrapper::{SessionWrapper, TmuxWrapper};

/// Look up a built-in agent by id, or fall back to a `GenericCli`
/// configured from YAML.
pub fn registry() -> agent::Registry {
    agent::Registry::default_builtins()
}

/// Default session wrapper — tmux. Overridable via config.
pub fn default_wrapper() -> Box<dyn SessionWrapper> {
    Box::new(TmuxWrapper::new())
}

/// Helper: ensure a directory exists before spawning inside it.
pub(crate) fn ensure_dir(p: &Path) -> std::io::Result<()> {
    if !p.exists() {
        std::fs::create_dir_all(p)?;
    }
    Ok(())
}
