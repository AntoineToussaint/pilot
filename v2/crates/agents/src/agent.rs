//! The `Agent` trait and built-in implementations.

use pilot_v2_ipc::AgentState;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Context passed to `Agent::spawn` / `resume`.
#[derive(Debug, Clone)]
pub struct SpawnCtx {
    pub session_key: String,
    pub worktree: PathBuf,
    pub repo: Option<String>,
    pub pr_number: Option<String>,
    pub env: HashMap<String, String>,
}

/// Hooks into an agent's lifecycle (optional — only Claude Code today).
/// If `Some`, the daemon will write this config so the agent emits state
/// transitions the daemon can watch without PTY pattern-matching.
#[derive(Debug, Clone)]
pub struct HookConfig {
    /// JSON (or YAML) blob to write as the agent's config file.
    pub config_blob: serde_json::Value,
    /// Where to write it, relative to the worktree or user home.
    pub install_path: PathBuf,
    /// Directory the hooks write state files into. Daemon watches.
    pub state_dir: PathBuf,
}

pub trait Agent: Send + Sync {
    /// Stable id used in config and IPC (`"claude"`, `"codex"`, etc.).
    fn id(&self) -> &'static str;

    /// Human-readable display name.
    fn display_name(&self) -> &'static str;

    /// Command + args to spawn a fresh session.
    fn spawn(&self, ctx: &SpawnCtx) -> Vec<String>;

    /// Command + args to resume the most recent session for this
    /// worktree. Default: same as `spawn`. Override when the agent has
    /// a `--continue`-style flag.
    fn resume(&self, ctx: &SpawnCtx) -> Vec<String> {
        self.spawn(ctx)
    }

    /// Inspect recent PTY output and return an updated state, or None
    /// if no confident determination. Used when the agent has no hooks
    /// (Codex, Cursor) or as a fallback when hooks miss a transition.
    fn detect_state(&self, recent_output: &[u8]) -> Option<AgentState> {
        let _ = recent_output;
        None
    }

    /// Return a hook config if this agent supports it. Default: None.
    fn hooks(&self) -> Option<HookConfig> {
        None
    }

    /// Encode a prompt as bytes the daemon should write to the PTY.
    /// Most agents accept plain text + a newline; some need bracketed
    /// paste or specific control sequences.
    fn inject_prompt(&self, prompt: &str) -> Vec<u8> {
        let mut bytes = prompt.as_bytes().to_vec();
        bytes.push(b'\n');
        bytes
    }
}

/// Registry of known agents. Keyed by `Agent::id()`.
#[derive(Default, Clone)]
pub struct Registry {
    agents: HashMap<&'static str, Arc<dyn Agent>>,
}

impl Registry {
    pub fn default_builtins() -> Self {
        let mut r = Self::default();
        r.register(Arc::new(builtins::Claude::default()));
        r.register(Arc::new(builtins::Codex::default()));
        r.register(Arc::new(builtins::Cursor::default()));
        r
    }

    pub fn register(&mut self, agent: Arc<dyn Agent>) {
        self.agents.insert(agent.id(), agent);
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Agent>> {
        self.agents.get(id).cloned()
    }

    pub fn ids(&self) -> impl Iterator<Item = &&'static str> {
        self.agents.keys()
    }
}

pub mod builtins {
    use super::*;

    #[derive(Default)]
    pub struct Claude;

    impl Agent for Claude {
        fn id(&self) -> &'static str {
            "claude"
        }
        fn display_name(&self) -> &'static str {
            "Claude Code"
        }
        fn spawn(&self, _ctx: &SpawnCtx) -> Vec<String> {
            vec!["claude".into()]
        }
        fn resume(&self, _ctx: &SpawnCtx) -> Vec<String> {
            vec!["claude".into(), "--continue".into()]
        }
        // detect_state + hooks wired in Week 1-2 of the rewrite.
    }

    #[derive(Default)]
    pub struct Codex;

    impl Agent for Codex {
        fn id(&self) -> &'static str {
            "codex"
        }
        fn display_name(&self) -> &'static str {
            "Codex"
        }
        fn spawn(&self, _ctx: &SpawnCtx) -> Vec<String> {
            vec!["codex".into()]
        }
    }

    #[derive(Default)]
    pub struct Cursor;

    impl Agent for Cursor {
        fn id(&self) -> &'static str {
            "cursor-agent"
        }
        fn display_name(&self) -> &'static str {
            "Cursor Agent"
        }
        fn spawn(&self, _ctx: &SpawnCtx) -> Vec<String> {
            vec!["cursor-agent".into()]
        }
    }

    /// User-defined agent loaded from YAML. Kept minimal — spawn cmd +
    /// optional resume args + asking patterns. Lets users ship new
    /// agent integrations without code.
    #[derive(Debug, Clone)]
    pub struct GenericCli {
        pub id: &'static str,
        pub display_name: &'static str,
        pub spawn_cmd: Vec<String>,
        pub resume_cmd: Option<Vec<String>>,
        pub asking_patterns: Vec<String>,
    }

    impl Agent for GenericCli {
        fn id(&self) -> &'static str {
            self.id
        }
        fn display_name(&self) -> &'static str {
            self.display_name
        }
        fn spawn(&self, _ctx: &SpawnCtx) -> Vec<String> {
            self.spawn_cmd.clone()
        }
        fn resume(&self, ctx: &SpawnCtx) -> Vec<String> {
            self.resume_cmd.clone().unwrap_or_else(|| self.spawn(ctx))
        }
        fn detect_state(&self, recent_output: &[u8]) -> Option<AgentState> {
            if self.asking_patterns.is_empty() {
                return None;
            }
            let text = String::from_utf8_lossy(recent_output);
            if self.asking_patterns.iter().any(|p| text.contains(p)) {
                Some(AgentState::Asking)
            } else {
                None
            }
        }
    }
}
