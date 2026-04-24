//! Pilot v2 IPC — protocol between the TUI and the daemon.
//!
//! The daemon is the single source of truth for all state (sessions,
//! worktrees, PTYs, provider polling, persistence). The TUI issues
//! `Command`s and receives `Event`s.
//!
//! **Communication is abstracted behind `Client` / `Server` traits.**
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

pub mod channel;
pub mod socket;

pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// Stable id for a spawned terminal. Distinct from SessionKey because a
/// single session may hold multiple terminals (agent + shell + logs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TerminalId(pub u64);

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

/// TUI → daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    /// Start streaming events. Daemon replies with `Event::Snapshot`
    /// then a live stream.
    Subscribe,
    Spawn {
        session_key: SessionKey,
        kind: TerminalKind,
        /// Override cwd; default is the session's worktree path.
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
}

/// Daemon → TUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// Initial snapshot reply to `Subscribe`. Sent once before the
    /// live stream starts so the client has a baseline.
    Snapshot {
        sessions: Vec<pilot_core::Session>,
        terminals: Vec<TerminalSnapshot>,
    },
    /// `Session` is ~680 bytes; boxing keeps every `Event` in-flight
    /// small so the async channel doesn't pay the worst-case size.
    SessionUpserted(Box<pilot_core::Session>),
    SessionRemoved(SessionKey),
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
/// A daemon's main loop holds many `Server`s. Events the daemon wants
/// to broadcast get sent on each `tx`; `rx` receives commands from
/// that specific client.
pub struct Server {
    pub tx: mpsc::UnboundedSender<Event>,
    pub rx: mpsc::UnboundedReceiver<Command>,
}

impl Server {
    pub fn from_channels(
        tx: mpsc::UnboundedSender<Event>,
        rx: mpsc::UnboundedReceiver<Command>,
    ) -> Self {
        Self { tx, rx }
    }
}
