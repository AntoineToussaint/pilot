//! Startup detection: which agents and providers are usable on this
//! machine. Drives the first-run setup screen and is also useful as a
//! standalone diagnostic (`pilot doctor` later).
//!
//! ## What gets detected
//!
//! - **Agents** — `claude`, `codex`, `cursor`. Probed by spawning the
//!   binary with `--version` and looking for a 0 exit. Cheaper than
//!   parsing PATH ourselves and matches what the daemon will do when
//!   it actually spawns the agent.
//! - **GitHub** — the same credential chain the daemon uses
//!   (`GH_TOKEN` → `GITHUB_TOKEN` → `gh auth token`). If any provider
//!   resolves a token we report ready; we don't try to validate it
//!   against the API here because that would slow down startup and
//!   require network.
//! - **Linear** — `LINEAR_API_KEY` env var present and non-empty.
//!
//! Detection is fully concurrent: each probe runs as its own future
//! and they're joined together, so a slow `claude --version` doesn't
//! delay `gh auth status`.

use pilot_auth::{CommandProvider, CredentialChain, EnvProvider};
use std::time::Duration;

/// One row on the setup screen: a tool we tried to detect plus what we
/// found. `display_name` is the human-readable label; `id` is a short
/// machine name used for tests and telemetry.
#[derive(Debug, Clone)]
pub struct ToolStatus {
    pub id: &'static str,
    pub display_name: &'static str,
    pub category: Category,
    pub state: ToolState,
    /// Shown under the row when state is `Missing`. Empty string if
    /// the tool is found or has no useful install pointer.
    pub install_hint: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Provider,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolState {
    /// Binary present (or env var set). `detail` describes what we
    /// found — e.g. "claude 1.0.42" or "GH_TOKEN".
    Found { detail: String },
    /// Tool is not installed / not configured.
    Missing,
}

impl ToolState {
    pub fn is_found(&self) -> bool {
        matches!(self, ToolState::Found { .. })
    }
}

/// Aggregate of every detection result.
#[derive(Debug, Clone)]
pub struct SetupReport {
    pub tools: Vec<ToolStatus>,
}

impl SetupReport {
    /// Pilot needs at least one task source AND at least one agent to
    /// be useful. Anything weaker than that and the user gets an empty
    /// sidebar or a broken `c` keystroke — better to surface the
    /// problem upfront than fail silently.
    pub fn is_ready(&self) -> bool {
        let has_provider = self
            .tools
            .iter()
            .any(|t| t.category == Category::Provider && t.state.is_found());
        let has_agent = self
            .tools
            .iter()
            .any(|t| t.category == Category::Agent && t.state.is_found());
        has_provider && has_agent
    }

    pub fn find(&self, id: &str) -> Option<&ToolStatus> {
        self.tools.iter().find(|t| t.id == id)
    }
}

/// Run all probes concurrently. Bounded — every probe has its own
/// timeout so a hanging `gh auth token` can't block startup forever.
pub async fn detect_all() -> SetupReport {
    let (claude, codex, cursor, github, linear) = tokio::join!(
        detect_binary("claude", "Claude Code", "https://claude.ai/code"),
        detect_binary("codex", "Codex", "npm install -g @openai/codex"),
        // Probe `cursor-agent` rather than `cursor` because that's the
        // CLI binary the agent registry expects to spawn. Users may
        // have the IDE installed without the headless agent.
        detect_binary("cursor-agent", "Cursor Agent", "https://cursor.com/cli"),
        detect_github(),
        detect_linear(),
    );
    SetupReport {
        tools: vec![github, linear, claude, codex, cursor],
    }
}

async fn detect_binary(
    bin: &'static str,
    display_name: &'static str,
    install_hint: &'static str,
) -> ToolStatus {
    let mk = |state| ToolStatus {
        id: bin,
        display_name,
        category: Category::Agent,
        state,
        install_hint,
    };
    let probe = async {
        tokio::process::Command::new(bin)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await
    };
    match tokio::time::timeout(Duration::from_secs(2), probe).await {
        Ok(Ok(output)) if output.status.success() => {
            let raw = String::from_utf8_lossy(&output.stdout);
            let detail = raw.lines().next().unwrap_or(bin).trim().to_string();
            mk(ToolState::Found { detail })
        }
        _ => mk(ToolState::Missing),
    }
}

async fn detect_github() -> ToolStatus {
    let mk = |state| ToolStatus {
        id: "github",
        display_name: "GitHub",
        category: Category::Provider,
        state,
        install_hint: "brew install gh && gh auth login",
    };

    let chain = CredentialChain::new()
        .with(EnvProvider::new("GH_TOKEN"))
        .with(EnvProvider::new("GITHUB_TOKEN"))
        .with(CommandProvider::new("gh", &["auth", "token"]));

    match tokio::time::timeout(Duration::from_secs(3), chain.resolve("github")).await {
        Ok(Ok(cred)) => mk(ToolState::Found {
            detail: cred.source,
        }),
        _ => mk(ToolState::Missing),
    }
}

async fn detect_linear() -> ToolStatus {
    let state = match std::env::var("LINEAR_API_KEY") {
        Ok(v) if !v.is_empty() => ToolState::Found {
            detail: "LINEAR_API_KEY".into(),
        },
        _ => ToolState::Missing,
    };
    ToolStatus {
        id: "linear",
        display_name: "Linear",
        category: Category::Provider,
        state,
        install_hint: "export LINEAR_API_KEY=lin_api_…",
    }
}
