//! pilot-server — owns state and IO on behalf of TUI clients.
//!
//! Lives as a library so the in-process transport can call `Server::serve`
//! without a subprocess. When out-of-process (remote access, long-running
//! service), the `pilot` binary's `daemon` subcommand invokes the same
//! `Server::serve` entrypoint over a Unix socket.
//!
//! Today the daemon exposes the PTY lifecycle (spawn/write/resize/close,
//! per-terminal ring buffer, reconnect replay) and the serve loop that
//! accepts `ipc::Command`s and emits `ipc::Event`s. Provider polling,
//! worktree management, agent hook plumbing, and LLM proxy integration
//! land on top of this core in the order described in `../DESIGN.md`.

pub mod agent_runs;
pub mod agent_spawn;
pub mod agent_stream;
pub mod api_gateway;
pub mod auth;
pub mod backend;
pub mod lifecycle;
pub mod polling;
pub mod pty;
pub mod socket_service;
pub mod spawn_handler;

use crate::backend::{RawPtyBackend, SessionBackend, TmuxBackend};
use pilot_store::{MemoryStore, SqliteStore, Store};
use pilot_agents::Registry;
use pilot_ipc::{AgentRunId, Connection, Event, TerminalId};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::sync::{Mutex, broadcast};

/// Where pilot keeps its persistent state.
pub fn state_db_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".pilot")
        .join("v2")
        .join("state.db")
}

/// Open the persistent store at the canonical path. Returns `None` on
/// open failure (corrupt DB, permissions); callers fall back to skipping
/// persistence rather than aborting startup.
pub fn open_store() -> Option<Arc<dyn Store>> {
    let path = state_db_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match SqliteStore::open(&path) {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            tracing::warn!("store open failed at {}: {e}", path.display());
            None
        }
    }
}

// REMOVED: wipe_legacy_worktrees. We never delete from `~/.pilot/`.
// pilot is constrained to `~/.pilot/v2/` for everything it writes
// — `state.db`, the bare-clone cache, every worktree. If a user
// has real work in `~/.pilot/worktrees/` from a prior tool, that's
// their data and pilot leaves it alone.

/// Capacity of the daemon's process-wide event broadcast bus. Events
/// produced by the poller and the PTY/proxy subsystems land here and
/// fan out to every connected client. If a slow client lags more than
/// `BUS_CAPACITY` events behind, it skips ahead — better than blocking
/// every other client on the slowest one.
pub const BUS_CAPACITY: usize = 1024;

/// `ServerConfig` is the per-process state shared across all client
/// connections — the persistent store, the broadcast bus the poller
/// pushes events into, and the agent registry the spawn handler reads.
/// Cheaply cloneable: `store` is `Arc`, `bus` is a tokio broadcast
/// `Sender` (clone is a refcount), `agents` is a small struct.
///
/// Per-process invariant: there is exactly **one** `ServerConfig` for
/// the whole process. Both `run_embedded` and `pilot daemon start`
/// build it once at startup so the polling loop's `SessionUpserted`
/// events reach every connected TUI.
#[derive(Clone)]
pub struct ServerConfig {
    pub agents: Registry,
    /// Persistent state at `~/.pilot/v2/state.db`.
    pub store: Arc<dyn Store>,
    /// Process-wide event bus. Producers (poller, PTY, proxy) call
    /// `bus.send(event)`; each `Server::serve` connection subscribes
    /// and forwards events into its own `Server.tx`.
    pub bus: broadcast::Sender<Event>,
    /// Pluggable session manager. Owns the actual agent processes —
    /// the server delegates spawn/write/resize/kill/subscribe.
    /// Default is `RawPtyBackend`; `TmuxBackend` adds persistence.
    pub backend: Arc<dyn SessionBackend>,
    /// Wire-side `TerminalId` ↔ backend session key. The server
    /// allocates numeric ids for the IPC stream; the backend uses its
    /// own stable string keys (e.g. tmux session names). This map
    /// translates between them. Every connection's serve loop reads
    /// + writes it.
    pub terminals: Arc<Mutex<HashMap<TerminalId, String>>>,
    /// Wire-side `TerminalId` → owning `SessionId`. Lets the
    /// migration code freeze just one session's runners during a
    /// `git worktree move`, instead of freezing every backend
    /// session in the process. Populated by `handle_spawn` when a
    /// terminal is created against a known session; entries are
    /// removed on `TerminalExited`.
    pub terminal_sessions: Arc<Mutex<HashMap<TerminalId, pilot_core::SessionId>>>,
    /// Cached `AgentState` per agent terminal. Populated by the
    /// output pump's state detector; transitions are broadcast as
    /// `Event::AgentState`. Caching avoids broadcasting on every
    /// PTY chunk when nothing changed.
    pub agent_states: Arc<Mutex<HashMap<TerminalId, pilot_ipc::AgentState>>>,
    /// Wire-side metadata per terminal: `(session_key, kind)`. The
    /// `terminals` map only carries the backend key; clients
    /// reconnecting via Subscribe need the full pairing so the
    /// initial Snapshot can route terminals into the right tab
    /// strip. Populated by `handle_spawn`, cleaned on
    /// `TerminalExited`.
    pub terminal_meta:
        Arc<Mutex<HashMap<TerminalId, (pilot_core::SessionKey, pilot_ipc::TerminalKind)>>>,
    /// Structured stream-json agent runs. Keyed by wire-side run id.
    pub agent_runs: Arc<Mutex<HashMap<AgentRunId, agent_runs::AgentRunHandle>>>,
    /// Process-wide structured run id allocator.
    pub next_agent_run_id: Arc<AtomicU64>,
    /// Per-principal provider credential store. The default in-memory
    /// implementation is intentionally non-persistent until the
    /// encrypted production store is chosen.
    pub credential_store: Arc<dyn auth::CredentialStore>,
    /// Local/dev fallback principal. API auth can replace this with a
    /// per-connection principal later.
    pub default_principal_id: pilot_ipc::PrincipalId,
}

impl ServerConfig {
    /// Open the store at `~/.pilot/v2/state.db`.
    ///
    /// Open failures (permissions, disk corruption) fall back to an
    /// in-memory store so the daemon still starts — better empty than
    /// dead.
    pub fn from_user_config() -> Self {
        let path = state_db_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let store = match SqliteStore::open(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "falling back to in-memory store: couldn't open {}: {e}",
                    path.display()
                );
                return Self::with_store(Arc::new(MemoryStore::new()));
            }
        };

        // Pick the strongest available backend. tmux means sessions
        // survive pilot-server restart and can be attached externally
        // via `tmux -L pilot attach -t <key>`; raw-pty is the
        // ephemeral fallback when tmux isn't installed.
        let backend: Arc<dyn SessionBackend> = match TmuxBackend::detect() {
            Some(t) => {
                tracing::info!("session backend: tmux");
                Arc::new(t)
            }
            None => {
                tracing::info!("session backend: raw-pty (tmux unavailable)");
                Arc::new(RawPtyBackend::new())
            }
        };
        Self::with_store_and_backend(Arc::new(store), backend)
    }

    /// Build a config with an explicit store and the deterministic
    /// raw-pty backend. Used by tests and the `--test` mode that
    /// don't want tmux side-effects (a real pilot tmux server, leftover
    /// sessions on disk). Production paths go through
    /// `from_user_config` which auto-detects tmux.
    pub fn with_store(store: Arc<dyn Store>) -> Self {
        Self::with_store_and_backend(store, Arc::new(RawPtyBackend::new()))
    }

    /// Build with explicit store + backend. Used by tests that want
    /// a stub backend, and by the binary wiring once backend
    /// detection (tmux vs raw-pty) lands.
    pub fn with_store_and_backend(store: Arc<dyn Store>, backend: Arc<dyn SessionBackend>) -> Self {
        let (bus, _) = broadcast::channel(BUS_CAPACITY);
        Self {
            agents: Registry::default_builtins(),
            store,
            bus,
            backend,
            terminals: Arc::new(Mutex::new(HashMap::new())),
            terminal_sessions: Arc::new(Mutex::new(HashMap::new())),
            agent_states: Arc::new(Mutex::new(HashMap::new())),
            terminal_meta: Arc::new(Mutex::new(HashMap::new())),
            agent_runs: Arc::new(Mutex::new(HashMap::new())),
            next_agent_run_id: Arc::new(AtomicU64::new(1)),
            credential_store: Arc::new(auth::MemoryCredentialStore::new()),
            default_principal_id: pilot_ipc::PrincipalId::local(),
        }
    }

    /// Convenience: in-memory config. Never touches the filesystem.
    pub fn in_memory() -> Self {
        Self::with_store(Arc::new(MemoryStore::new()))
    }
}

pub struct Server {
    config: ServerConfig,
}

impl Server {
    pub fn new(config: ServerConfig) -> Self {
        Self { config }
    }

    /// Accept a client connection (either an in-process `Server` from
    /// `ipc::channel::pair` or a remote `Server` from `ipc::socket::serve`).
    ///
    /// The loop selects on:
    /// - inbound commands from the client (Subscribe, Shutdown, …),
    /// - the process-wide broadcast bus (SessionUpserted, etc).
    ///
    /// Bus events are forwarded straight to the client. Commands are
    /// dispatched here; handlers that don't have a backing subsystem
    /// yet are trace-logged and dropped so adding a command at the IPC
    /// layer never breaks an existing client.
    pub async fn serve(&self, mut conn: Connection) -> anyhow::Result<()> {
        let mut bus_rx = self.config.bus.subscribe();
        loop {
            tokio::select! {
                cmd = conn.rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    tracing::debug!("daemon ← {cmd:?}");
                    match cmd {
                        pilot_ipc::Command::Subscribe => {
                            let workspaces = load_workspaces(&*self.config.store);
                            let terminals = spawn_handler::snapshot_terminals(&self.config).await;
                            let _ = conn.tx.send(Event::Snapshot {
                                workspaces,
                                terminals,
                            });
                        }
                        pilot_ipc::Command::Spawn { session_key, session_id, kind, cwd } => {
                            spawn_handler::handle_spawn(
                                &self.config,
                                session_key,
                                session_id,
                                kind,
                                cwd,
                            )
                            .await;
                        }
                        pilot_ipc::Command::CreateSession { session_key, kind, label } => {
                            spawn_handler::handle_create_session(
                                &self.config,
                                session_key,
                                kind,
                                label,
                            )
                            .await;
                        }
                        pilot_ipc::Command::Write { terminal_id, bytes } => {
                            spawn_handler::handle_write(&self.config, terminal_id, &bytes).await;
                        }
                        pilot_ipc::Command::Resize { terminal_id, cols, rows } => {
                            spawn_handler::handle_resize(&self.config, terminal_id, cols, rows).await;
                        }
                        pilot_ipc::Command::Close { terminal_id } => {
                            spawn_handler::handle_close(&self.config, terminal_id).await;
                        }
                        pilot_ipc::Command::StartAgentRun {
                            session_key,
                            session_id,
                            agent,
                            mode,
                            cwd,
                            initial_input,
                        } => {
                            agent_runs::handle_start_agent_run(
                                &self.config,
                                session_key,
                                session_id,
                                agent,
                                mode,
                                cwd,
                                initial_input,
                            )
                            .await;
                        }
                        pilot_ipc::Command::SendAgentInput { run_id, message } => {
                            agent_runs::handle_send_agent_input(&self.config, run_id, message)
                                .await;
                        }
                        pilot_ipc::Command::InterruptAgentRun { run_id } => {
                            agent_runs::handle_interrupt_agent_run(&self.config, run_id).await;
                        }
                        pilot_ipc::Command::DecideAgentApproval {
                            run_id,
                            request_id,
                            decision,
                        } => {
                            agent_runs::handle_decide_agent_approval(
                                &self.config,
                                run_id,
                                request_id,
                                decision,
                            )
                            .await;
                        }
                        pilot_ipc::Command::AnswerAgentQuestion {
                            run_id,
                            question_id,
                            answer,
                        } => {
                            agent_runs::handle_answer_agent_question(
                                &self.config,
                                run_id,
                                question_id,
                                answer,
                            )
                            .await;
                        }
                        pilot_ipc::Command::UpsertProviderCredential {
                            principal_id,
                            credential,
                        } => {
                            auth::handle_upsert_provider_credential(
                                &self.config,
                                &conn.tx,
                                principal_id,
                                credential,
                            )
                            .await;
                        }
                        pilot_ipc::Command::RemoveProviderCredential {
                            principal_id,
                            provider_id,
                        } => {
                            auth::handle_remove_provider_credential(
                                &self.config,
                                &conn.tx,
                                principal_id,
                                provider_id,
                            )
                            .await;
                        }
                        pilot_ipc::Command::ListProviderCredentials { principal_id } => {
                            auth::handle_list_provider_credentials(
                                &self.config,
                                &conn.tx,
                                principal_id,
                            )
                            .await;
                        }
                        pilot_ipc::Command::MarkRead { session_key } => {
                            let key = pilot_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            polling::mark_workspace_read(&self.config, &key);
                        }
                        pilot_ipc::Command::MarkActivityRead { session_key, index } => {
                            let key = pilot_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            polling::mark_activity_read(&self.config, &key, index);
                        }
                        pilot_ipc::Command::UnmarkActivityRead { session_key, index } => {
                            let key = pilot_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            polling::unmark_activity_read(&self.config, &key, index);
                        }
                        pilot_ipc::Command::CreateWorkspace { name } => {
                            polling::create_empty_workspace(&self.config, &name);
                        }
                        pilot_ipc::Command::Snooze { session_key, until } => {
                            let key = pilot_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            polling::set_snooze(&self.config, &key, Some(until));
                        }
                        pilot_ipc::Command::Unsnooze { session_key } => {
                            let key = pilot_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            polling::set_snooze(&self.config, &key, None);
                        }
                        pilot_ipc::Command::Kill { session_key } => {
                            let key = pilot_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            polling::delete_workspace(&self.config, &key).await;
                        }
                        pilot_ipc::Command::Refresh => {
                            // No-op for now: the polling loop runs on
                            // its own interval. Wired so the catch-all
                            // doesn't trace-log every `g` press as
                            // "command handler not yet wired" — and so
                            // a future manual-trigger refactor has the
                            // arm to slot into.
                        }
                        pilot_ipc::Command::PostReply { session_key, body } => {
                            polling::post_reply(&self.config, session_key, body).await;
                        }
                        pilot_ipc::Command::SetSessionLayout {
                            session_key,
                            session_id_raw,
                            layout_json,
                        } => {
                            let key = pilot_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            let session_id = uuid::Uuid::parse_str(&session_id_raw)
                                .ok()
                                .map(pilot_core::SessionId);
                            let layout: Option<pilot_core::SessionLayout> =
                                serde_json::from_str(&layout_json).ok();
                            if let (Some(sid), Some(lay)) = (session_id, layout) {
                                polling::set_session_layout(&self.config, &key, sid, lay);
                            } else {
                                tracing::warn!(
                                    "SetSessionLayout: bad payload (id={:?})",
                                    session_id_raw
                                );
                            }
                        }
                        pilot_ipc::Command::Shutdown => break,
                    }
                }
                bus = bus_rx.recv() => {
                    match bus {
                        Ok(evt) => {
                            let _ = conn.tx.send(evt);
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            // Slow client missed `n` events. Tracing
                            // tells us when this is happening; the loop
                            // continues from the next still-buffered
                            // event so we don't block the bus.
                            tracing::warn!("client lagged behind bus by {n} events");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
        Ok(())
    }
}

/// Deserialize every persisted `Workspace`. Bad JSON is logged and
/// skipped so a single corrupted row doesn't break startup.
fn load_workspaces(store: &dyn Store) -> Vec<pilot_core::Workspace> {
    let records = match store.list_workspaces() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("list_workspaces failed: {e}");
            return vec![];
        }
    };
    records
        .into_iter()
        .filter_map(|r| {
            let json = r.workspace_json?;
            match serde_json::from_str::<pilot_core::Workspace>(&json) {
                Ok(w) => Some(w),
                Err(e) => {
                    tracing::warn!("skipping unreadable workspace {}: {e}", r.key);
                    None
                }
            }
        })
        .collect()
}
