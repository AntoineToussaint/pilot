//! Pilot v2 IPC — protocol between the TUI and the daemon.
//!
//! The daemon is the single source of truth for all state (sessions,
//! worktrees, PTYs, provider polling, persistence). The TUI issues
//! `Command`s and receives `Event`s.
//!
//! **Communication is abstracted behind `Client` / `Connection` traits.**
//! The common case — TUI and daemon living in one process — uses the
//! `channel` transport: a pair of tokio mpsc channels, zero
//! serialization, zero sockets. The remote case — TUI running on a
//! laptop connecting to a daemon on a workstation over SSH — uses the
//! `socket` transport: length-prefixed bincode over a Unix socket
//! (which SSH's `-L` forwards). Client code never branches on which.
//!
//! # Wire framing (socket transport only)
//!
//! Each message on the wire is `[u32 BE length][bincode payload]`.
//! Max frame size is `MAX_FRAME_BYTES` (64 MiB).

use pilot_core::SessionKey;
use serde::{Deserialize, Serialize};
use std::fmt;

pub mod channel;
pub mod socket;

pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// Stable id for a spawned terminal. Distinct from SessionKey because a
/// single session may hold multiple terminals (agent + shell + logs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TerminalId(pub u64);

/// Stable id for a structured agent runtime. This is intentionally
/// separate from `TerminalId`: a run may be stream-json only, terminal
/// only, or mirrored into both surfaces by higher layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentRunId(pub u64);

/// Runtime surface requested for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentRuntimeMode {
    /// Traditional PTY/terminal byte stream.
    Terminal,
    /// Structured stream-json, independent of PTY bytes.
    StreamJson,
}

/// What to launch inside a terminal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TerminalKind {
    /// A known agent by id (e.g. "claude", "codex"). The daemon looks
    /// up the `Agent` impl and computes argv.
    Agent(String),
    /// Plain shell — `config.shell.command`.
    Shell,
    /// Tail a file inside the worktree.
    LogTail { path: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    Active,
    Idle,
    Asking,
    Stopped,
}

/// User input sent to a structured agent runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInputMessage {
    /// Human-readable user text.
    pub text: Option<String>,
    /// Raw JSON payload for runtimes that accept structured input.
    pub json: Option<String>,
}

/// Decision for a tool/permission request emitted by an agent runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentApprovalDecision {
    Approve,
    Deny { reason: Option<String> },
}

/// Answer to a structured question from an agent runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentQuestionAnswer {
    pub answer: String,
}

/// Token/cost usage reported by a structured agent runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    /// Cost in millionths of a USD. Integer wire value avoids float
    /// compatibility issues across languages.
    pub cost_usd_micros: Option<u64>,
}

/// Stable identity for the human or service account connected to a
/// Pilot daemon. The current local daemon uses `local`; remote/multi-user
/// clients should authenticate into distinct principal ids.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PrincipalId(String);

impl PrincipalId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn local() -> Self {
        Self("local".into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for PrincipalId {
    fn default() -> Self {
        Self::local()
    }
}

impl From<&str> for PrincipalId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for PrincipalId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Debug for PrincipalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PrincipalId").field(&self.0).finish()
    }
}

impl fmt::Display for PrincipalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Public, non-secret credential metadata that clients may receive in
/// snapshots or events. Secret material is deliberately not represented
/// here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCredentialMetadata {
    pub principal_id: PrincipalId,
    pub provider_id: String,
    pub source: String,
    pub scopes: Vec<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Secret-bearing credential bootstrap payload. Custom `Debug` keeps
/// daemon command tracing from printing provider tokens.
#[derive(Clone, Serialize, Deserialize)]
pub struct ProviderCredentialInput {
    pub provider_id: String,
    pub token: String,
    pub source: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl fmt::Debug for ProviderCredentialInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderCredentialInput")
            .field("provider_id", &self.provider_id)
            .field("token", &"[REDACTED]")
            .field("source", &self.source)
            .field("scopes", &self.scopes)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// TUI → daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    /// Start streaming events. Connection replies with `Event::Snapshot`
    /// then a live stream.
    Subscribe,
    /// Create a fresh `Session` (== fresh worktree folder) inside the
    /// workspace identified by `session_key` (this name on the wire
    /// holds the workspace key — see the SessionKey docs). The
    /// daemon allocates a new `SessionId`, sets up the worktree on
    /// disk, and emits `Event::SessionCreated`. The TUI uses this
    /// when the user explicitly wants a separate folder from any
    /// existing sessions.
    CreateSession {
        session_key: SessionKey,
        kind: TerminalKind,
        /// Optional friendly label. Defaults to the kind's name.
        label: Option<String>,
    },
    /// Spawn a terminal inside a session. `session_id == Some(id)`
    /// targets that specific session; `None` lets the daemon pick the
    /// workspace's default session (creating one on the fly when the
    /// workspace has no sessions yet). The session supplies the cwd
    /// (its worktree path). `cwd` may override that for ad-hoc spawns.
    Spawn {
        session_key: SessionKey,
        #[serde(default)]
        session_id: Option<pilot_core::SessionId>,
        kind: TerminalKind,
        cwd: Option<String>,
    },
    Write {
        terminal_id: TerminalId,
        bytes: Vec<u8>,
    },
    Resize {
        terminal_id: TerminalId,
        cols: u16,
        rows: u16,
    },
    Close {
        terminal_id: TerminalId,
    },
    Kill {
        session_key: SessionKey,
    },
    MarkRead {
        session_key: SessionKey,
    },
    Snooze {
        session_key: SessionKey,
        until: chrono::DateTime<chrono::Utc>,
    },
    Unsnooze {
        session_key: SessionKey,
    },
    Merge {
        session_key: SessionKey,
    },
    Approve {
        session_key: SessionKey,
    },
    UpdateBranch {
        session_key: SessionKey,
    },
    Refresh,
    Shutdown,
    /// Start an agent runtime using a structured protocol surface. This
    /// does not replace `Spawn`; terminal clients can keep using PTY
    /// bytes while structured clients subscribe to run events.
    StartAgentRun {
        session_key: SessionKey,
        #[serde(default)]
        session_id: Option<pilot_core::SessionId>,
        agent: String,
        mode: AgentRuntimeMode,
        cwd: Option<String>,
        initial_input: Option<AgentInputMessage>,
    },
    SendAgentInput {
        run_id: AgentRunId,
        message: AgentInputMessage,
    },
    InterruptAgentRun {
        run_id: AgentRunId,
    },
    DecideAgentApproval {
        run_id: AgentRunId,
        request_id: String,
        decision: AgentApprovalDecision,
    },
    AnswerAgentQuestion {
        run_id: AgentRunId,
        question_id: String,
        answer: AgentQuestionAnswer,
    },
    /// Store/update a provider credential for one Pilot principal.
    /// This is the bootstrap path for local desktop clients; future
    /// API connection auth can make `principal_id` implicit.
    UpsertProviderCredential {
        principal_id: PrincipalId,
        credential: ProviderCredentialInput,
    },
    RemoveProviderCredential {
        principal_id: PrincipalId,
        provider_id: String,
    },
    ListProviderCredentials {
        principal_id: PrincipalId,
    },
}

/// Connection → TUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    // ── v2 hierarchy reminder ─────────────────────────────────────
    //
    // Repo (string `"owner/name"` from the task's provider)
    //  └── Workspace (one unit-of-work; `Workspace`)
    //       └── Session (= one folder worktree; runtime state)
    //            └── Terminal (= one PTY rooted in that folder)
    //
    // Snapshot carries Workspace rows; Sessions and Terminals are
    // recovered separately so a client reconnecting mid-flight can
    // re-bind to its running agents. `WorkspaceUpserted` /
    // `WorkspaceRemoved` are the fan-out events; `TerminalSpawned` /
    // `TerminalOutput` / `TerminalExited` track the bottom layer.
    /// Initial snapshot reply to `Subscribe`. Sent once before the
    /// live stream starts so the client has a baseline. The v2 row
    /// model is `Workspace` — one per worktree, holding the linked
    /// PR + issues. The legacy `Session` (one task per row) is gone:
    /// every component reads from the workspace directly and
    /// projects to a primary task via `workspace.primary_task()`.
    Snapshot {
        workspaces: Vec<pilot_core::Workspace>,
        terminals: Vec<TerminalSnapshot>,
    },
    /// A workspace was created or updated.
    /// Boxed because Workspace is several KB once activity is
    /// populated; keeping the `Event` enum slim avoids worst-case
    /// async-channel overhead.
    WorkspaceUpserted(Box<pilot_core::Workspace>),
    WorkspaceRemoved(pilot_core::WorkspaceKey),
    /// A new session (= folder worktree) was provisioned inside its
    /// workspace. Sent in response to `Command::CreateSession` and
    /// also when the daemon auto-creates a session for a workspace-
    /// scoped Spawn. Sidebar uses this to expand the workspace row
    /// into session sub-rows once the count crosses 1.
    SessionCreated(Box<pilot_core::WorkspaceSession>),
    /// A session ended (process exited and the worktree was reaped,
    /// OR the user explicitly killed it). Includes the workspace
    /// key so consumers can look up which row to update without
    /// parsing the session id.
    SessionEnded {
        workspace_key: pilot_core::WorkspaceKey,
        session_id: pilot_core::SessionId,
    },
    TerminalSpawned {
        terminal_id: TerminalId,
        session_key: SessionKey,
        kind: TerminalKind,
    },
    TerminalOutput {
        terminal_id: TerminalId,
        bytes: Vec<u8>,
        /// Monotonic per-terminal sequence for gap detection.
        seq: u64,
    },
    TerminalExited {
        terminal_id: TerminalId,
        exit_code: Option<i32>,
    },
    AgentState {
        session_key: SessionKey,
        state: AgentState,
    },
    ProviderError {
        source: String,
        message: String,
    },
    Notification {
        title: String,
        body: String,
    },
    /// Structured telemetry from the LLM proxy: one record per
    /// request/response the agent made through the daemon-injected
    /// HTTP proxy. Clients use this to populate the Cost/Tokens tile
    /// and the tool-call activity timeline.
    ProxyRecord(pilot_v2_llm_proxy::ProxyRecord),
    AgentRunStarted {
        run_id: AgentRunId,
        session_key: SessionKey,
        #[serde(default)]
        session_id: Option<pilot_core::SessionId>,
        agent: String,
        mode: AgentRuntimeMode,
    },
    /// Lossless raw stream-json line or object text from the runtime.
    AgentRawJson {
        run_id: AgentRunId,
        json: String,
    },
    AgentDebug {
        run_id: AgentRunId,
        message: String,
    },
    AgentAssistantTextDelta {
        run_id: AgentRunId,
        delta: String,
    },
    AgentToolCallStarted {
        run_id: AgentRunId,
        call_id: String,
        name: String,
        input_json: Option<String>,
    },
    AgentToolCallDelta {
        run_id: AgentRunId,
        call_id: String,
        delta_json: String,
    },
    AgentToolCallFinished {
        run_id: AgentRunId,
        call_id: String,
        output_json: Option<String>,
        error: Option<String>,
    },
    AgentPermissionRequest {
        run_id: AgentRunId,
        request_id: String,
        tool_name: String,
        input_json: Option<String>,
        reason: Option<String>,
    },
    AgentUserQuestion {
        run_id: AgentRunId,
        question_id: String,
        prompt: String,
        choices: Vec<String>,
        allow_freeform: bool,
    },
    AgentUsage {
        run_id: AgentRunId,
        usage: AgentUsage,
    },
    AgentTurnFinished {
        run_id: AgentRunId,
        result: Option<String>,
        session_id: Option<String>,
        error: Option<String>,
    },
    AgentRunFinished {
        run_id: AgentRunId,
        exit_code: Option<i32>,
        error: Option<String>,
    },
    ProviderCredentialUpdated {
        principal_id: PrincipalId,
        provider_id: String,
        metadata: ProviderCredentialMetadata,
    },
    ProviderCredentialRemoved {
        principal_id: PrincipalId,
        provider_id: String,
    },
    ProviderCredentialsListed {
        principal_id: PrincipalId,
        credentials: Vec<ProviderCredentialMetadata>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalSnapshot {
    pub terminal_id: TerminalId,
    pub session_key: SessionKey,
    pub kind: TerminalKind,
    /// Recent PTY output (daemon-side ring buffer). The client feeds
    /// this into its libghostty-vt to reconstruct the screen.
    pub replay: Vec<u8>,
    pub last_seq: u64,
}

// ── Transport abstraction ──────────────────────────────────────────────

use tokio::sync::mpsc;

/// A live connection to the daemon. Owned by the TUI.
///
/// Local (in-process) daemons hand back a `Client` whose `send`/`recv`
/// are tokio channel operations — no serialization at all. Remote
/// daemons hand back a `Client` whose internals read and write frames
/// over a socket. The TUI doesn't see the difference.
pub struct Client {
    tx: mpsc::UnboundedSender<Command>,
    rx: mpsc::UnboundedReceiver<Event>,
}

impl Client {
    /// Build a `Client` from a pair of pre-wired channels. Used by both
    /// transports — `channel::spawn` for in-process, `socket::connect`
    /// for remote.
    pub fn from_channels(
        tx: mpsc::UnboundedSender<Command>,
        rx: mpsc::UnboundedReceiver<Event>,
    ) -> Self {
        Self { tx, rx }
    }

    pub fn send(&self, cmd: Command) -> Result<(), mpsc::error::SendError<Command>> {
        self.tx.send(cmd)
    }

    pub async fn recv(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}

/// The server-side of a connection. One per connected client.
///
/// A daemon's main loop holds many `Connection`s. Events the daemon wants
/// to broadcast get sent on each `tx`; `rx` receives commands from
/// that specific client.
pub struct Connection {
    pub tx: mpsc::UnboundedSender<Event>,
    pub rx: mpsc::UnboundedReceiver<Command>,
}

impl Connection {
    pub fn from_channels(
        tx: mpsc::UnboundedSender<Event>,
        rx: mpsc::UnboundedReceiver<Command>,
    ) -> Self {
        Self { tx, rx }
    }
}
