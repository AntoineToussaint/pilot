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
use pilot_agents::SpawnCtx;
use pilot_ipc::{Event, TerminalId, TerminalKind, TerminalSnapshot};
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
    // Singleton enforcement at the daemon (the source of truth for
    // who's running what). The TUI also intercepts duplicates
    // client-side for snappy focus-not-spawn behavior, but that
    // alone fails the moment a second client connects to the same
    // daemon. The guard here protects the invariant for everyone:
    // at most one Claude per session, one Codex per session, etc.
    if let Some(existing) = find_existing_singleton(config, &session_key, &kind).await {
        let _ = config.bus.send(Event::TerminalFocusRequested {
            terminal_id: existing,
        });
        return;
    }
    // Resolve target session + cwd. The cwd param wins over
    // workspace lookup so the existing `Spawn { cwd: Some(...) }`
    // callers (tests, in-process flows) keep working unchanged.
    // `owning_session` is the session id this spawn lives in — used
    // to populate `terminal_sessions` so the migration freeze can
    // scope correctly. None when cwd was overridden out-of-band.
    let (cwd_path, owning_session): (Option<PathBuf>, Option<pilot_core::SessionId>) =
        if let Some(c) = cwd.as_deref() {
            (Some(PathBuf::from(c)), None)
        } else {
            match resolve_or_create_session(config, &session_key, session_id, &kind).await {
                Ok((path, sid)) => (Some(path), Some(sid)),
                Err(e) => {
                    let _ = config.bus.send(Event::ProviderError {
                        source: "spawn:session".into(),
                        message: format!("{e}"),
                        detail: String::new(),
                        kind: String::new(),
                    });
                    return;
                }
            }
        };
    let argv = match argv_for(config, &kind, &cwd_path) {
        Some(a) => a,
        None => {
            let _ = config.bus.send(Event::ProviderError {
                source: format!("spawn:{kind:?}"),
                message: "no agent registered for this id".into(),
            detail: String::new(),
            kind: String::new(),
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
            detail: String::new(),
            kind: String::new(),
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
    config
        .terminal_meta
        .lock()
        .await
        .insert(terminal_id, (session_key.clone(), kind.clone()));
    if let Some(sid) = owning_session {
        config
            .terminal_sessions
            .lock()
            .await
            .insert(terminal_id, sid);
    }
    // Persist the (backend_key → session_key, kind) pairing so the
    // next pilot start can reattach surviving tmux sessions to their
    // owning workspace. Without this, `recover_sessions` reattaches
    // raw PTYs but doesn't know which workspace they belong to —
    // sidebar badges go blank, even though the agent is still alive.
    persist_terminal_meta(config, &backend_key, &session_key, &kind).await;

    // Pump backend output → bus. Also runs agent-state detection
    // on each chunk so the user sees a "needs input" badge when
    // Claude/Codex is waiting on an approval prompt. State is
    // cached per-terminal so we only broadcast on transitions.
    let bus = config.bus.clone();
    let backend = config.backend.clone();
    let terminals_map = config.terminals.clone();
    let term_sessions_map = config.terminal_sessions.clone();
    let agent_states_map = config.agent_states.clone();
    let terminal_meta_map = config.terminal_meta.clone();
    let store_for_pump = config.store.clone();
    let id_for_pump = terminal_id;
    let key_for_pump = backend_key.clone();
    let agent_for_pump: Option<std::sync::Arc<dyn pilot_agents::Agent>> = match &kind {
        TerminalKind::Agent(id) => config.agents.get(id),
        _ => None,
    };
    let session_key_for_pump = session_key.clone();
    tokio::spawn(async move {
        let mut sub = match backend.subscribe(&key_for_pump).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("backend subscribe {key_for_pump}: {e}");
                return;
            }
        };
        // Per-terminal rolling buffer for state detection — bounded
        // so a long agent run doesn't grow it forever. 4 KiB is
        // enough to span every prompt the agents produce today.
        // Folded inline (no per-chunk `tokio::spawn`) because heavy
        // output would otherwise burn thousands of tiny tasks per
        // second contending on the same mutex; doing the work in
        // the pump task itself is one allocation cheaper per chunk.
        const STATE_BUF_CAP: usize = 4 * 1024;
        let mut state_buf: Vec<u8> = Vec::with_capacity(STATE_BUF_CAP);

        async fn maybe_emit_state_change(
            agent: Option<&std::sync::Arc<dyn pilot_agents::Agent>>,
            buf: &mut Vec<u8>,
            bytes: &[u8],
            states: &std::sync::Arc<
                tokio::sync::Mutex<
                    std::collections::HashMap<TerminalId, pilot_ipc::AgentState>,
                >,
            >,
            bus: &tokio::sync::broadcast::Sender<Event>,
            id: TerminalId,
            session_key: &SessionKey,
        ) {
            const STATE_BUF_CAP: usize = 4 * 1024;
            let Some(agent) = agent else {
                return;
            };
            buf.extend_from_slice(bytes);
            if buf.len() > STATE_BUF_CAP {
                let drop = buf.len() - STATE_BUF_CAP;
                buf.drain(..drop);
            }
            let Some(new_state) = agent.detect_state(buf) else {
                return;
            };
            let mut map = states.lock().await;
            if map.get(&id).copied() == Some(new_state) {
                return;
            }
            map.insert(id, new_state);
            drop(map);
            let _ = bus.send(Event::AgentState {
                session_key: session_key.clone(),
                state: new_state,
            });
        }

        if !sub.replay.is_empty() {
            maybe_emit_state_change(
                agent_for_pump.as_ref(),
                &mut state_buf,
                &sub.replay,
                &agent_states_map,
                &bus,
                id_for_pump,
                &session_key_for_pump,
            )
            .await;
            let _ = bus.send(Event::TerminalOutput {
                terminal_id: id_for_pump,
                bytes: sub.replay.clone(),
                seq: sub.last_seq,
            });
        }
        while let Some(chunk) = sub.live.recv().await {
            maybe_emit_state_change(
                agent_for_pump.as_ref(),
                &mut state_buf,
                &chunk.bytes,
                &agent_states_map,
                &bus,
                id_for_pump,
                &session_key_for_pump,
            )
            .await;
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
        term_sessions_map.lock().await.remove(&id_for_pump);
        agent_states_map.lock().await.remove(&id_for_pump);
        terminal_meta_map.lock().await.remove(&id_for_pump);
        let _ = store_for_pump.delete_kv(&format!("terminal:{key_for_pump}"));
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
) -> anyhow::Result<(PathBuf, SessionId)> {
    let workspace_key = WorkspaceKey::new(session_key.as_str());

    // Spawn against a workspace that isn't (yet) persisted — common
    // in tests and in --test mode, and fine in general: nothing
    // about the wire-side `session_key` requires the workspace to
    // exist on disk. Just root the spawn in the user's cwd. Use a
    // fresh ephemeral session id so terminal_sessions still gets a
    // mapping for the migration freeze.
    let mut workspace = match load_workspace(config, &workspace_key) {
        Ok(w) => w,
        Err(_) => {
            return Ok((
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                SessionId::new(),
            ));
        }
    };

    if let Some(id) = session_id {
        let session = workspace
            .find_session(id)
            .ok_or_else(|| anyhow::anyhow!("session {id:?} not in workspace"))?;
        ensure_worktree_present(config, &workspace, &session.worktree_path).await;
        return Ok((session.worktree_path.clone(), session.id));
    }
    if let Some(session) = workspace.default_session() {
        ensure_worktree_present(config, &workspace, &session.worktree_path).await;
        return Ok((session.worktree_path.clone(), session.id));
    }

    // Workspace exists but has no sessions yet — provision one.
    // Worktree path is human-readable: `<root>/<workspace_slug>` for
    // the first session, `<root>/<workspace_slug>-2` for the second,
    // etc. The slug is derived from the PR (PR-{n}-{title-slug}) or
    // from the user-supplied workspace name when the workspace is
    // pre-PR. `Session.id` stays a UUID for stable internal identity;
    // only the path is human-friendly.
    let kind_for_session = session_kind_from_terminal(kind);
    let path = worktree_path_for_session(&workspace, 0);

    let provisioned = provision_worktree(&workspace, &path).await;
    if let Err(e) = &provisioned {
        // Real-checkout failed (no GH access, branch missing, network
        // hiccup) — fall back to an empty dir so spawn works. Surface
        // a non-fatal error so the user knows their `s` press landed
        // in a bare directory, not the PR's tree.
        tracing::warn!("worktree provisioning failed: {e}");
        let _ = config.bus.send(Event::ProviderError {
            source: "worktree".into(),
            message: format!("git worktree setup failed; using empty dir ({e})"),
            detail: String::new(),
            kind: "retryable".into(),
        });
        ensure_dir_exists(&path).await;
    }

    let session = Session::new(
        workspace_key.clone(),
        kind_for_session,
        path.clone(),
        Utc::now(),
    );
    let new_session_id = session.id;
    workspace.add_session(session.clone());
    persist_and_broadcast(config, &workspace).await?;
    let _ = config.bus.send(Event::SessionCreated(Box::new(session)));
    Ok((path, new_session_id))
}

/// Try to set up a real git worktree at `target` for the workspace's
/// primary task. Returns Ok(()) when a checkout succeeded, Err when
/// we couldn't (caller falls back to a plain mkdir).
async fn provision_worktree(
    workspace: &Workspace,
    target: &std::path::Path,
) -> anyhow::Result<()> {
    let task = workspace
        .primary_task()
        .ok_or_else(|| anyhow::anyhow!("workspace has no primary task"))?;
    let repo = task
        .repo
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("task has no repo"))?;
    let branch = task
        .branch
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("task has no branch"))?;
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("repo '{repo}' is not owner/name"))?;

    let mgr = pilot_git_ops::WorktreeManager::default_base();
    mgr.checkout_at(target, owner, name, branch)
        .await
        .map_err(|e| anyhow::anyhow!("checkout_at failed: {e}"))?;
    Ok(())
}

/// Idempotently create `path` (and parents). Used as the fallback when
/// git checkout can't run, and for re-validation when the persisted
/// session record points at a path that may have been removed by hand.
async fn ensure_dir_exists(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let _ = tokio::fs::create_dir_all(path).await;
}

/// If a stored Session points at a worktree path the user has since
/// removed (manual `rm -rf`, disk wipe, etc.), restore it. Tries a
/// real `git worktree add` first so the recovered tree carries the
/// PR's branch; falls back to a plain mkdir + ProviderError when git
/// can't help (no clone, branch missing, no network).
async fn ensure_worktree_present(
    config: &ServerConfig,
    workspace: &Workspace,
    path: &std::path::Path,
) {
    if path.exists() {
        return;
    }
    tracing::info!("worktree {} missing — re-provisioning", path.display());
    if let Err(e) = provision_worktree(workspace, path).await {
        tracing::warn!("re-provision failed: {e}");
        let _ = config.bus.send(Event::ProviderError {
            source: "worktree".into(),
            message: format!("re-checkout failed; using empty dir ({e})"),
            detail: String::new(),
            kind: "retryable".into(),
        });
        ensure_dir_exists(path).await;
    }
}

/// Look for an existing terminal in `session_key`'s set whose
/// kind has the same singleton identity as `kind`. Returns the
/// wire-side `TerminalId` so the caller can broadcast a focus
/// request. None when nothing matches OR the kind isn't singleton.
async fn find_existing_singleton(
    config: &ServerConfig,
    session_key: &SessionKey,
    kind: &TerminalKind,
) -> Option<TerminalId> {
    let target = kind.singleton_key()?;
    let snapshot = snapshot_terminals(config).await;
    snapshot
        .iter()
        .find(|t| {
            t.session_key == *session_key && t.kind.singleton_key().as_deref() == Some(&target)
        })
        .map(|t| t.terminal_id)
}

/// Freeze every backend session belonging to `session_id`. Returns
/// the keys we froze so the caller can `resume` them after the
/// worktree move. With tmux the freeze detaches clients so the
/// inner shell can't read input mid-rename and print stale `pwd`;
/// other backends no-op cleanly.
///
/// Scoped to one session via the `terminal_sessions` map so an
/// unrelated workspace's runners don't pause for our migration.
async fn freeze_runners_in_session(
    config: &crate::ServerConfig,
    session_id: pilot_core::SessionId,
) -> Vec<String> {
    let owners = config.terminal_sessions.lock().await;
    let term_map = config.terminals.lock().await;
    let keys: Vec<String> = owners
        .iter()
        .filter(|(_, sid)| **sid == session_id)
        .filter_map(|(tid, _)| term_map.get(tid).cloned())
        .collect();
    drop(term_map);
    drop(owners);
    for k in &keys {
        let _ = config.backend.freeze(k).await;
    }
    keys
}

/// PR-attach migration. Walks every session in `workspace`, checks
/// whether its persisted `worktree_path` matches what the current
/// slug would generate, and `git worktree move`s the mismatches.
/// Mutates `workspace` in place — the caller is responsible for
/// persistence + broadcast.
///
/// Running synchronously inside `polling::upsert` (rather than
/// fire-and-forget) closes the race window where consumers could
/// briefly see a stale `worktree_path` between attach + migration.
///
/// Live PTY processes inside the worktree keep their open dir handle
/// across the rename — POSIX `rename(2)` on a directory is atomic
/// and doesn't disturb existing inode references. Their `pwd` will
/// briefly print the old absolute path until they `cd .`. With the
/// tmux backend, `freeze_runners_in_session` detaches clients so
/// the inner shell can't even observe the rename mid-flight.
///
/// Returns whether any session was actually migrated. No-op when
/// every session already lives at the right place (most polls).
pub async fn migrate_session_paths_if_needed(
    config: &crate::ServerConfig,
    workspace: &mut Workspace,
) -> bool {
    let mut moved_any = false;
    // Sort sessions by created_at so the index assignment matches
    // what `worktree_path_for_session` expects (first = no suffix,
    // second = -2, etc.).
    let mut order: Vec<usize> = (0..workspace.sessions.len()).collect();
    order.sort_by_key(|&i| workspace.sessions[i].created_at);

    for (slot, sess_idx) in order.into_iter().enumerate() {
        let expected = worktree_path_for_session(workspace, slot);
        let actual = workspace.sessions[sess_idx].worktree_path.clone();
        if actual == expected {
            continue;
        }
        let actual_exists = tokio::fs::metadata(&actual).await.is_ok();
        if !actual_exists {
            // Path moved by hand or never created. Just update the
            // record — no on-disk move needed.
            workspace.sessions[sess_idx].worktree_path = expected;
            moved_any = true;
            continue;
        }
        // Source dir exists but isn't actually a git worktree —
        // typically a leftover from V1's UUID-named worktree layout.
        // `git worktree move` would fail with "is not a working tree";
        // just update the record and let the next spawn re-provision.
        // We do NOT delete the orphan dir — the user might have
        // unrelated work in there, and earlier deletes have already
        // burned us once.
        let is_worktree = tokio::fs::metadata(actual.join(".git")).await.is_ok();
        if !is_worktree {
            tracing::info!(
                "session {} points at non-worktree {} — updating record only",
                workspace.sessions[sess_idx].id,
                actual.display()
            );
            workspace.sessions[sess_idx].worktree_path = expected;
            moved_any = true;
            continue;
        }
        // Real move via git. Need owner + repo to find the bare clone.
        let Some(task) = workspace.primary_task() else {
            continue;
        };
        let Some(repo) = task.repo.as_deref() else {
            continue;
        };
        let Some((owner, name)) = repo.split_once('/') else {
            continue;
        };
        let mgr = pilot_git_ops::WorktreeManager::default_base();
        let bare = mgr.bare_path(owner, name);
        if let Some(parent) = expected.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }

        // Freeze just this session's backend keys (not every backend
        // session in the process). The narrower scope means a busy
        // workspace's other sessions don't pause for an unrelated
        // migration.
        let session_id = workspace.sessions[sess_idx].id;
        let frozen_keys = freeze_runners_in_session(config, session_id).await;

        let result = mgr.move_worktree(&bare, &actual, &expected).await;

        for k in &frozen_keys {
            let _ = config.backend.resume(k).await;
        }

        match result {
            Ok(()) => {
                tracing::info!(
                    "migrated worktree {} → {}",
                    actual.display(),
                    expected.display()
                );
                workspace.sessions[sess_idx].worktree_path = expected;
                moved_any = true;
            }
            Err(e) => {
                tracing::warn!(
                    "git worktree move {} → {} failed: {e}",
                    actual.display(),
                    expected.display()
                );
                let _ = config.bus.send(pilot_ipc::Event::ProviderError {
                    source: "worktree".into(),
                    message: format!("PR-attach migration failed: {e}"),
                    detail: String::new(),
                    kind: "retryable".into(),
                });
            }
        }
    }

    moved_any
}

/// Root directory for every workspace's worktrees. Sits under the v2
/// state root next to `state.db` so a single `rm -rf ~/.pilot/v2/`
/// wipes everything pilot owns on disk.
pub fn worktree_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".pilot").join("v2").join("worktrees")
}

/// Compose the on-disk path for the Nth session of a workspace.
/// `index = 0` → `<root>/<slug>` (no suffix, cleanest case).
/// `index = N` → `<root>/<slug>-{N+1}` so the second session is
/// `slug-2`, third is `slug-3`, …  Matches the user mental model
/// where session-counter starts at "no number".
fn worktree_path_for_session(workspace: &Workspace, index: usize) -> PathBuf {
    let mut name = workspace.worktree_slug();
    if index > 0 {
        name.push_str(&format!("-{}", index + 1));
    }
    worktree_root().join(name)
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
            detail: String::new(),
            kind: String::new(),
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
            detail: String::new(),
            kind: String::new(),
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
/// Per-survivor we look up the persisted `(session_key, kind)` pairing
/// (saved at spawn time, see `persist_terminal_meta`) so the sidebar
/// reattaches each PTY to its owning workspace. Survivors with no
/// persisted record fall back to a session_key=""/Shell placeholder —
/// rare in practice (only happens after a store wipe + dangling tmux),
/// and the user can clean those up via Shift-X.
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
        let (session_key, kind) = load_terminal_meta(config, &key)
            .await
            .unwrap_or_else(|| (SessionKey::from(""), TerminalKind::Shell));
        let terminal_id = alloc_terminal_id();
        config
            .terminals
            .lock()
            .await
            .insert(terminal_id, key.clone());
        // Populate terminal_meta so snapshot_terminals + the sidebar's
        // badge map see this PTY as belonging to its real workspace.
        // Without this the recovered terminal shows up as orphan and
        // nothing in the UI suggests it exists.
        config
            .terminal_meta
            .lock()
            .await
            .insert(terminal_id, (session_key.clone(), kind.clone()));

        let bus = config.bus.clone();
        let backend = config.backend.clone();
        let terminals_map = config.terminals.clone();
        let terminal_meta_map = config.terminal_meta.clone();
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
            terminal_meta_map.lock().await.remove(&terminal_id);
        });

        let _ = config.bus.send(Event::TerminalSpawned {
            terminal_id,
            session_key,
            kind,
        });
    }
}

/// Persist the `(session_key, kind)` pairing for `backend_key` to the
/// store under `terminal:{backend_key}`. Read back in `recover_sessions`
/// after a pilot restart.
async fn persist_terminal_meta(
    config: &ServerConfig,
    backend_key: &str,
    session_key: &SessionKey,
    kind: &TerminalKind,
) {
    let payload = match serde_json::to_string(&(session_key.as_str(), kind)) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("persist terminal_meta: encode failed: {e}");
            return;
        }
    };
    if let Err(e) = config
        .store
        .set_kv(&format!("terminal:{backend_key}"), &payload)
    {
        tracing::warn!("persist terminal_meta: store write failed: {e}");
    }
}

/// Inverse of `persist_terminal_meta`. Returns None when nothing was
/// previously stored — caller falls back to a placeholder.
async fn load_terminal_meta(
    config: &ServerConfig,
    backend_key: &str,
) -> Option<(SessionKey, TerminalKind)> {
    let raw = config
        .store
        .get_kv(&format!("terminal:{backend_key}"))
        .ok()
        .flatten()?;
    let parsed: (String, TerminalKind) = serde_json::from_str(&raw).ok()?;
    Some((SessionKey::from(parsed.0.as_str()), parsed.1))
}

/// Used by `Subscribe` to seed a new client with what's already
/// running. Reads the parallel `terminal_meta` map populated by
/// `handle_spawn` so each snapshot carries the right session_key
/// and kind, not the empty-string placeholders an earlier version
/// returned.
pub async fn snapshot_terminals(config: &ServerConfig) -> Vec<TerminalSnapshot> {
    let map = config.terminals.lock().await;
    let meta = config.terminal_meta.lock().await;
    let mut out = Vec::with_capacity(map.len());
    for (id, _key) in map.iter() {
        let (session_key, kind) = meta
            .get(id)
            .cloned()
            .unwrap_or_else(|| (SessionKey::from(""), TerminalKind::Shell));
        out.push(TerminalSnapshot {
            terminal_id: *id,
            session_key,
            kind,
            replay: Vec::new(),
            last_seq: 0,
        });
    }
    out
}

/// Walk every persisted workspace's `sessions` and spawn any whose
/// runner isn't already alive. Called once at startup after
/// `recover_sessions` (which reattaches surviving tmux sessions).
///
/// Sessions are persistent **intent**: a record means "the user
/// wants a claude here". Restoring at startup matches the user's
/// mental model — the sidebar shows `▸ claude` for a workspace
/// because there should be a claude running. Without this, a pilot
/// restart leaves a stale-looking sidebar with the terminal stack
/// reading "(no terminals)".
///
/// Per-session, per-pilot-lifetime: we only relaunch sessions that
/// don't currently have a live PTY. If the user explicitly killed
/// one earlier in this run, it stays dead until next restart.
pub async fn restore_persisted_sessions(config: &ServerConfig) {
    let workspaces = match config.store.list_workspaces() {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("restore: list_workspaces failed: {e}");
            return;
        }
    };

    // Snapshot live (session_key, kind) pairs so we can dedupe.
    let live: std::collections::HashSet<(String, String)> = {
        let meta = config.terminal_meta.lock().await;
        meta.values()
            .map(|(sk, k)| (sk.as_str().to_string(), kind_id(k)))
            .collect()
    };

    for record in workspaces {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(workspace) = serde_json::from_str::<Workspace>(&json) else {
            continue;
        };
        let session_key = SessionKey::from(workspace.key.as_str());
        for session in &workspace.sessions {
            let kind = match &session.kind {
                pilot_core::SessionKind::Agent { agent_id } => {
                    TerminalKind::Agent(agent_id.clone())
                }
                pilot_core::SessionKind::Shell => TerminalKind::Shell,
                // Compare / LogTail aren't auto-restored — those
                // are user-initiated transient runners.
                _ => continue,
            };
            let key_pair = (session_key.as_str().to_string(), kind_id(&kind));
            if live.contains(&key_pair) {
                continue;
            }
            tracing::info!(
                "restoring session {:?} in workspace {}",
                kind, workspace.key
            );
            handle_spawn(config, session_key.clone(), Some(session.id), kind, None).await;
        }
    }
}

/// Stable string identity for a `TerminalKind` — used as a hash
/// key in the live-session set during restoration. Mirrors the
/// `singleton_key()` shape but always returns Some.
fn kind_id(kind: &TerminalKind) -> String {
    match kind {
        TerminalKind::Agent(id) => format!("agent:{id}"),
        TerminalKind::Shell => "shell".into(),
        TerminalKind::LogTail { path } => format!("log:{path}"),
    }
}
