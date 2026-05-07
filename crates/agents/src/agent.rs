//! The `Agent` trait and built-in implementations.

use pilot_ipc::AgentState;
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
        r.register(Arc::new(builtins::Claude));
        r.register(Arc::new(builtins::Codex));
        r.register(Arc::new(builtins::Cursor));
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

        /// Claude Code's interactive prompt UI is recognisable by a
        /// stable footer line (`Esc to cancel · Tab to amend · …`)
        /// plus a small set of question phrasings. Matching the
        /// footer is highest-confidence — Claude only renders it
        /// when it's waiting on the user. The phrase fallbacks catch
        /// cases where the footer is scrolled off the recent buffer
        /// but the question is still visible.
        fn detect_state(&self, recent_output: &[u8]) -> Option<AgentState> {
            let s = strip_ansi_lossy(recent_output);
            // Highest-precision: the chooser footer.
            if s.contains("Esc to cancel") && s.contains("Tab to amend") {
                return Some(AgentState::Asking);
            }
            // Common question phrasings. "Do you want to" alone is
            // weak (chat output could include the phrase) so we pair
            // with a numbered choice marker.
            let has_choice = s.contains("1. Yes") || s.contains("(y/n)") || s.contains("[y/n]");
            if has_choice
                && (s.contains("Do you want to")
                    || s.contains("Allow Claude")
                    || s.contains("Approve"))
            {
                return Some(AgentState::Asking);
            }
            // Default: assume Active. Returning Some(Active) lets the
            // daemon notice the transition Asking → Active when the
            // user hits 1/2; without it the cached state would stay
            // Asking forever.
            Some(AgentState::Active)
        }
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

        /// Codex CLI uses `[y/n]` style prompts for tool approvals.
        /// Same generic pattern + a few Codex-specific phrasings.
        fn detect_state(&self, recent_output: &[u8]) -> Option<AgentState> {
            let s = strip_ansi_lossy(recent_output);
            if s.contains("[y/n]") || s.contains("(y/n)") || s.contains("approve?") {
                return Some(AgentState::Asking);
            }
            Some(AgentState::Active)
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

        fn detect_state(&self, recent_output: &[u8]) -> Option<AgentState> {
            let s = strip_ansi_lossy(recent_output);
            if s.contains("[y/n]") || s.contains("(y/n)") {
                return Some(AgentState::Asking);
            }
            Some(AgentState::Active)
        }
    }

    /// Quick-and-dirty ANSI stripper for state detection. We don't
    /// need correctness for rendering (libghostty-vt does that) — just
    /// enough to make pattern matches survive cursor moves and color
    /// codes interleaved with the literal text.
    fn strip_ansi_lossy(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b {
                // ESC: skip until the next final byte for CSI/OSC.
                i += 1;
                if i >= bytes.len() {
                    break;
                }
                let intro = bytes[i];
                i += 1;
                if intro == b'[' {
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                } else if intro == b']' {
                    while i < bytes.len() && bytes[i] != 0x07 {
                        if bytes[i] == 0x1b
                            && i + 1 < bytes.len()
                            && bytes[i + 1] == b'\\'
                        {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                    if i < bytes.len() && bytes[i] == 0x07 {
                        i += 1;
                    }
                }
                continue;
            }
            // Cheap UTF-8: push byte regardless. The pattern matcher
            // works on byte-equivalent ASCII substrings.
            out.push(bytes[i] as char);
            i += 1;
        }
        out
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
