//! Wires the IPC `Spawn`/`Write`/`Resize`/`Close` commands to the
//! [`SessionBackend`](crate::backend::SessionBackend) trait. The
//! server itself owns no PTY state — every backend-side operation
//! goes through `config.backend`.
//!
//! ## Per-process state on `ServerConfig`
//!
//! `ServerConfig::terminals` maps wire `TerminalId` → backend session
//! key. Multiple connections (in-process channel + a remote SSH
//! `pilot --connect`) share this map so they see the same set.
//!
//! ## Flow on Spawn
//!
//! 1. Resolve `kind` to argv:
//!    - `Agent(id)` → look up `Registry`, call `Agent::spawn(ctx)`.
//!    - `Shell` → user's `$SHELL` or fallback `/bin/sh`.
//!    - `LogTail` → `tail -F path`.
//! 2. `backend.spawn(argv, cwd, env)` returns a backend session key.
//! 3. Allocate a fresh `TerminalId`; store the pairing on
//!    `config.terminals`.
//! 4. `backend.subscribe(key)` → spawn a pump task that fans each
//!    output chunk to `config.bus` as `Event::TerminalOutput`. When
//!    the chunk stream ends, await `backend.wait_exit`, emit
//!    `Event::TerminalExited`, drop the map entry.
//! 5. Broadcast `Event::TerminalSpawned` to every subscriber.

use crate::ServerConfig;
use chrono::Utc;
use pilot_core::{
    SessionId, SessionKey, SessionKind, Workspace, WorkspaceKey, WorkspaceSession as Session,
};
use pilot_store::WorkspaceRecord;
use pilot_v2_agents::SpawnCtx;
use pilot_v2_ipc::{Event, TerminalId, TerminalKind, TerminalSnapshot};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic terminal-id allocator. Module-local so ids are unique
/// across the process even if the terminals map is wiped (tests, or
/// a future "kill all" command).
static NEXT_TERMINAL_ID: AtomicU64 = AtomicU64::new(1);

fn alloc_terminal_id() -> TerminalId {
    TerminalId(NEXT_TERMINAL_ID.fetch_add(1, Ordering::Relaxed))
}

/// Build the argv for `kind`. None means we don't know how to spawn
/// it (unknown agent id, etc.) — handled by emitting a ProviderError.
fn argv_for(
    config: &ServerConfig,
    kind: &TerminalKind,
    cwd: &Option<PathBuf>,
) -> Option<Vec<String>> {
    match kind {
        TerminalKind::Agent(agent_id) => {
            let agent = config.agents.get(agent_id)?;
            let ctx = SpawnCtx {
                session_key: String::new(),
                worktree: cwd
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
                repo: None,
                pr_number: None,
                env: Default::default(),
            };
            Some(agent.spawn(&ctx))
        }
        TerminalKind::Shell => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            Some(vec![shell])
        }
        TerminalKind::LogTail { path } => Some(vec!["tail".into(), "-F".into(), path.clone()]),
    }
}

/// Spawn a terminal inside a session and broadcast
/// `Event::TerminalSpawned`. Failures emit `Event::ProviderError` so
/// the user gets feedback in the TUI rather than a silent miss.
///
/// Resolution order for the cwd / target session:
///
/// 1. If `cwd` is `Some`, the caller wins — use it raw.
/// 2. Else load the workspace, find a session via `session_id` (or
///    fall back to its default) and use that session's
///    `worktree_path` as cwd.
/// 3. If the workspace has no sessions yet, auto-create one rooted
///    in `cwd_or_inherited` (current dir today) and persist the
///    workspace before spawning. The auto-creation emits
///    `Event::SessionCreated`.
///
/// This keeps the user-facing flow simple — pressing `s` on a fresh
/// workspace "just works" — while preserving the invariant that
/// every terminal lives inside a session, which lives inside a
/// folder worktree.
pub async fn handle_spawn(
    config: &ServerConfig,
    session_key: SessionKey,
    session_id: Option<SessionId>,
    kind: TerminalKind,
    cwd: Option<String>,
) {
    // Resolve target session + cwd. The cwd param wins over
    // workspace lookup so the existing `Spawn { cwd: Some(...) }`
    // callers (tests, in-process flows) keep working unchanged.
    let resolved_cwd: Option<PathBuf> = if let Some(c) = cwd.as_deref() {
        Some(PathBuf::from(c))
    } else {
        match resolve_or_create_session(config, &session_key, session_id, &kind).await {
            Ok(path) => Some(path),
            Err(e) => {
                let _ = config.bus.send(Event::ProviderError {
                    source: "spawn:session".into(),
                    message: format!("{e}"),
                });
                return;
            }
        }
    };
    let cwd_path = resolved_cwd;
    let argv = match argv_for(config, &kind, &cwd_path) {
        Some(a) => a,
        None => {
            let _ = config.bus.send(Event::ProviderError {
                source: format!("spawn:{kind:?}"),
                message: "no agent registered for this id".into(),
            });
            return;
        }
    };

    let backend_key = match config.backend.spawn(&argv, cwd_path.as_deref(), &[]).await {
        Ok(k) => k,
        Err(e) => {
            let _ = config.bus.send(Event::ProviderError {
                source: "spawn".into(),
                message: format!("{e}"),
            });
            return;
        }
    };

    let terminal_id = alloc_terminal_id();
    config
        .terminals
        .lock()
        .await
        .insert(terminal_id, backend_key.clone());

    // Pump backend output → bus.
    let bus = config.bus.clone();
    let backend = config.backend.clone();
    let terminals_map = config.terminals.clone();
    let id_for_pump = terminal_id;
    let key_for_pump = backend_key.clone();
    tokio::spawn(async move {
        let mut sub = match backend.subscribe(&key_for_pump).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("backend subscribe {key_for_pump}: {e}");
                return;
            }
        };
        if !sub.replay.is_empty() {
            let _ = bus.send(Event::TerminalOutput {
                terminal_id: id_for_pump,
                bytes: sub.replay.clone(),
                seq: sub.last_seq,
            });
        }
        while let Some(chunk) = sub.live.recv().await {
            let _ = bus.send(Event::TerminalOutput {
                terminal_id: id_for_pump,
                bytes: chunk.bytes,
                seq: chunk.seq,
            });
        }
        let exit_code = backend.wait_exit(&key_for_pump).await;
        let _ = bus.send(Event::TerminalExited {
            terminal_id: id_for_pump,
            exit_code,
        });
        terminals_map.lock().await.remove(&id_for_pump);
    });

    let _ = config.bus.send(Event::TerminalSpawned {
        terminal_id,
        session_key,
        kind,
    });
}

/// Look up the session whose worktree this Spawn should land in.
///
/// - `Some(session_id)` → look it up in the workspace, error if it's
///   gone (rare race where the user removed the session between
///   selecting it and pressing the spawn key).
/// - `None` → use `Workspace::default_session`, or auto-create one
///   when the workspace is empty. Auto-creation emits
///   `Event::SessionCreated` so the sidebar's expansion-on-multi-
///   session UI reacts.
async fn resolve_or_create_session(
    config: &ServerConfig,
    session_key: &SessionKey,
    session_id: Option<SessionId>,
    kind: &TerminalKind,
) -> anyhow::Result<PathBuf> {
    let workspace_key = WorkspaceKey::new(session_key.as_str());

    // Spawn against a workspace that isn't (yet) persisted — common
    // in tests and in --test mode, and fine in general: nothing
    // about the wire-side `session_key` requires the workspace to
    // exist on disk. Just root the spawn in the user's cwd.
    let mut workspace = match load_workspace(config, &workspace_key) {
        Ok(w) => w,
        Err(_) => {
            return Ok(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        }
    };

    if let Some(id) = session_id {
        let session = workspace
            .find_session(id)
            .ok_or_else(|| anyhow::anyhow!("session {id:?} not in workspace"))?;
        return Ok(session.worktree_path.clone());
    }
    if let Some(session) = workspace.default_session() {
        return Ok(session.worktree_path.clone());
    }

    // Workspace exists but has no sessions yet — provision one on
    // the fly. The cwd defaults to the user's current dir; future
    // work hands this to a real WorktreeManager (see #105 / #77).
    let path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let kind_for_session = session_kind_from_terminal(kind);
    let session = Session::new(
        workspace_key.clone(),
        kind_for_session,
        path.clone(),
        Utc::now(),
    );
    workspace.add_session(session.clone());
    persist_and_broadcast(config, &workspace).await?;
    let _ = config.bus.send(Event::SessionCreated(Box::new(session)));
    Ok(path)
}

/// Explicit session creation. Always provisions a fresh worktree
/// folder, even if the workspace already has sessions — multi-session
/// workspaces are the whole point of this entry point.
pub async fn handle_create_session(
    config: &ServerConfig,
    session_key: SessionKey,
    kind: TerminalKind,
    label: Option<String>,
) {
    let workspace_key = WorkspaceKey::new(session_key.as_str());
    let mut workspace = match load_workspace(config, &workspace_key) {
        Ok(w) => w,
        Err(e) => {
            let _ = config.bus.send(Event::ProviderError {
                source: "create_session".into(),
                message: format!("{e}"),
            });
            return;
        }
    };
    let path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut session = Session::new(
        workspace_key,
        session_kind_from_terminal(&kind),
        path,
        Utc::now(),
    );
    if let Some(label) = label {
        session.name = label;
    }
    workspace.add_session(session.clone());
    if let Err(e) = persist_and_broadcast(config, &workspace).await {
        let _ = config.bus.send(Event::ProviderError {
            source: "create_session".into(),
            message: format!("{e}"),
        });
        return;
    }
    let _ = config.bus.send(Event::SessionCreated(Box::new(session)));
}

/// Project a wire-side `TerminalKind` to a runtime `SessionKind`.
/// Today they're nearly isomorphic but they live at different layers
/// — `SessionKind` is what's persisted on the workspace, while
/// `TerminalKind` is the wire-format for spawn commands.
fn session_kind_from_terminal(kind: &TerminalKind) -> SessionKind {
    match kind {
        TerminalKind::Agent(agent_id) => SessionKind::Agent {
            agent_id: agent_id.clone(),
        },
        TerminalKind::Shell => SessionKind::Shell,
        TerminalKind::LogTail { path } => SessionKind::LogTail { path: path.clone() },
    }
}

fn load_workspace(config: &ServerConfig, key: &WorkspaceKey) -> anyhow::Result<Workspace> {
    let record = config
        .store
        .get_workspace(key)
        .map_err(|e| anyhow::anyhow!("store: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("unknown workspace {}", key.as_str()))?;
    let json = record
        .workspace_json
        .ok_or_else(|| anyhow::anyhow!("workspace {} has no json", key.as_str()))?;
    Ok(serde_json::from_str(&json)?)
}

async fn persist_and_broadcast(config: &ServerConfig, workspace: &Workspace) -> anyhow::Result<()> {
    let json = serde_json::to_string(workspace)?;
    config
        .store
        .save_workspace(&WorkspaceRecord {
            key: workspace.key.as_str().to_string(),
            created_at: workspace.created_at,
            workspace_json: Some(json),
        })
        .map_err(|e| anyhow::anyhow!("save: {e}"))?;
    let _ = config
        .bus
        .send(Event::WorkspaceUpserted(Box::new(workspace.clone())));
    Ok(())
}

pub async fn handle_write(config: &ServerConfig, terminal_id: TerminalId, bytes: &[u8]) {
    let key = match config.terminals.lock().await.get(&terminal_id).cloned() {
        Some(k) => k,
        None => {
            tracing::trace!("write to unknown terminal {terminal_id:?}");
            return;
        }
    };
    if let Err(e) = config.backend.write(&key, bytes).await {
        tracing::warn!("backend write {key}: {e}");
    }
}

pub async fn handle_resize(config: &ServerConfig, terminal_id: TerminalId, cols: u16, rows: u16) {
    let key = match config.terminals.lock().await.get(&terminal_id).cloned() {
        Some(k) => k,
        None => return,
    };
    if let Err(e) = config.backend.resize(&key, cols, rows).await {
        tracing::warn!("backend resize {key}: {e}");
    }
}

/// Stop the session via the backend. The pump task drains the
/// remaining output chunks (if any), sees the stream close, and emits
/// `Event::TerminalExited` itself.
pub async fn handle_close(config: &ServerConfig, terminal_id: TerminalId) {
    let key = match config.terminals.lock().await.get(&terminal_id).cloned() {
        Some(k) => k,
        None => return,
    };
    if let Err(e) = config.backend.kill(&key).await {
        tracing::warn!("backend kill {key}: {e}");
    }
}

/// Bind already-running backend sessions to fresh wire TerminalIds.
/// Called once at server startup so pilot restarts don't lose the
/// agents the user was running.
///
/// What we recover: just enough to let the user see + drive the
/// session. Each survivor gets a placeholder `session_key=""` and
/// `kind=Shell` because we don't currently persist the original
/// pairing. Future work: store `(backend_key → session_key, kind)`
/// in SQLite at spawn time so the sidebar reattaches properly.
pub async fn recover_sessions(config: &ServerConfig) {
    let keys = match config.backend.list().await {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!("backend.list at startup: {e}");
            return;
        }
    };
    if keys.is_empty() {
        return;
    }
    tracing::info!("recovering {} surviving session(s)", keys.len());
    for key in keys {
        let terminal_id = alloc_terminal_id();
        config
            .terminals
            .lock()
            .await
            .insert(terminal_id, key.clone());

        let bus = config.bus.clone();
        let backend = config.backend.clone();
        let terminals_map = config.terminals.clone();
        let key_for_pump = key.clone();
        tokio::spawn(async move {
            let mut sub = match backend.subscribe(&key_for_pump).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("recover subscribe {key_for_pump}: {e}");
                    return;
                }
            };
            if !sub.replay.is_empty() {
                let _ = bus.send(Event::TerminalOutput {
                    terminal_id,
                    bytes: sub.replay.clone(),
                    seq: sub.last_seq,
                });
            }
            while let Some(chunk) = sub.live.recv().await {
                let _ = bus.send(Event::TerminalOutput {
                    terminal_id,
                    bytes: chunk.bytes,
                    seq: chunk.seq,
                });
            }
            let exit_code = backend.wait_exit(&key_for_pump).await;
            let _ = bus.send(Event::TerminalExited {
                terminal_id,
                exit_code,
            });
            terminals_map.lock().await.remove(&terminal_id);
        });

        let _ = config.bus.send(Event::TerminalSpawned {
            terminal_id,
            session_key: SessionKey::from(""),
            kind: TerminalKind::Shell,
        });
    }
}

/// Used by `Subscribe` to seed a new client with what's already
/// running. Currently does NOT include scrollback replay — that
/// requires another subscribe per terminal which is wasteful when
/// many clients connect. Clients re-subscribing get replay through
/// their own subsequent Subscribe path.
pub async fn snapshot_terminals(config: &ServerConfig) -> Vec<TerminalSnapshot> {
    let map = config.terminals.lock().await;
    let mut out = Vec::with_capacity(map.len());
    for (id, _key) in map.iter() {
        out.push(TerminalSnapshot {
            terminal_id: *id,
            // The TerminalId → SessionKey pairing was announced by
            // the original TerminalSpawned event; clients reconnecting
            // mid-session don't have a way to recover it from the
            // server today. Future work: store the pairing in the
            // terminals map instead of just the backend key.
            session_key: SessionKey::from(""),
            kind: TerminalKind::Shell,
            replay: Vec::new(),
            last_seq: 0,
        });
    }
    out
}
