//! Pure logic for the "agent needs my input" signal.
//!
//! When the daemon detects a Claude/Codex/Cursor session has flipped
//! into `Asking` (yes/no prompt, tool approval), the user needs to
//! know — even when pilot isn't the focused window. This module
//! owns the state-transition math behind that signal:
//!
//! - [`apply_agent_state`] mutates a workspace's sessions to match a
//!   new `AgentState` and reports whether the change crossed the
//!   "now needs input / no longer needs input" boundary. The
//!   transition tells the caller whether to fire a desktop
//!   notification (we only want to ring once per Asking-onset, not
//!   on every redraw).
//!
//! - [`next_asking_workspace`] picks the next workspace whose
//!   sessions are currently asking — used by the `!` jump-to-asking
//!   key. Walks in `keys_order` so cycling is deterministic.
//!
//! Both are free functions over plain slices/HashMaps so the cell
//! tests can pin every interesting (state, transition) pair without
//! standing up a `Sidebar`.

use pilot_core::{SessionKey, SessionKind, SessionRunState, Workspace, WorkspaceSession};
use pilot_ipc::AgentState;
use std::collections::HashMap;

/// How the workspace's overall "needs my input" status changed as a
/// result of applying an `AgentState` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionTransition {
    /// At least one agent session crossed Active → Asking.
    /// The caller should fire a one-shot desktop notification.
    NowAsking,
    /// All agent sessions left Asking. The caller might dismiss any
    /// notification UI (no-op for now — we don't track notif IDs).
    NoLongerAsking,
    /// State changed within the same "is anything asking?" bucket,
    /// or no change at all. The caller should NOT re-notify.
    NoChange,
}

/// Apply an `Event::AgentState` to a workspace's sessions.
///
/// Mutates every `Agent`-kind session's state to match `incoming`
/// (mapping `AgentState::Asking` → `SessionRunState::Asking`,
/// `AgentState::Active` → `SessionRunState::Active`). Shell, Compare,
/// and LogTail sessions are left untouched — they don't have an
/// "asking" semantic.
///
/// Returns the [`AttentionTransition`] so the caller can decide
/// whether this warrants a desktop notification.
///
/// **Why not just compare individual sessions?** The signal we
/// surface is workspace-level ("PR #1234 needs input"); the user
/// doesn't care which Agent slot is asking. So the transition is
/// computed over the workspace's `any agent session asking?` state,
/// not per session.
pub fn apply_agent_state(
    sessions: &mut [WorkspaceSession],
    incoming: AgentState,
) -> AttentionTransition {
    let was_asking = sessions
        .iter()
        .any(|s| s.state == SessionRunState::Asking);
    let new_state = match incoming {
        AgentState::Asking => SessionRunState::Asking,
        AgentState::Active => SessionRunState::Active,
    };
    for s in sessions.iter_mut() {
        if matches!(s.kind, SessionKind::Agent { .. }) {
            s.state = new_state;
        }
    }
    let is_asking = sessions
        .iter()
        .any(|s| s.state == SessionRunState::Asking);
    match (was_asking, is_asking) {
        (false, true) => AttentionTransition::NowAsking,
        (true, false) => AttentionTransition::NoLongerAsking,
        _ => AttentionTransition::NoChange,
    }
}

/// True if any session in the workspace is currently `Asking`.
/// Single source of truth for the workspace-level needs-input check
/// (sidebar header counter, row pill, `!` jump predicate).
pub fn workspace_is_asking(workspace: &Workspace) -> bool {
    workspace
        .sessions
        .iter()
        .any(|s| s.state == SessionRunState::Asking)
}

/// Pick the next workspace that needs the user's attention, starting
/// after `current` in `keys_order`. Wraps around. Returns `None`
/// when no workspace is asking.
///
/// The `keys_order` argument is the visible order from the sidebar
/// (so `!` follows the user's current sort/filter) — not the
/// underlying `HashMap` iteration order, which is non-deterministic.
///
/// "Starting after `current`" means: if the user is already focused
/// on an Asking workspace, `!` skips to the next one rather than
/// re-selecting the same row. When `current` is None (no selection)
/// we start from the top of `keys_order`.
pub fn next_asking_workspace(
    workspaces: &HashMap<SessionKey, Workspace>,
    keys_order: &[SessionKey],
    current: Option<&SessionKey>,
) -> Option<SessionKey> {
    if keys_order.is_empty() {
        return None;
    }
    let start_idx = current
        .and_then(|c| keys_order.iter().position(|k| k == c))
        .map(|i| i + 1)
        .unwrap_or(0);
    // Walk `keys_order` once starting at `start_idx`, wrapping. This
    // is a single O(n) sweep — fine even for hundreds of workspaces.
    for offset in 0..keys_order.len() {
        let idx = (start_idx + offset) % keys_order.len();
        let key = &keys_order[idx];
        if let Some(ws) = workspaces.get(key) {
            if workspace_is_asking(ws) {
                return Some(key.clone());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilot_core::{SessionId, WorkspaceKey};
    use std::path::PathBuf;

    fn workspace_key() -> WorkspaceKey {
        WorkspaceKey::new("owner/repo#1")
    }

    fn agent_session(state: SessionRunState) -> WorkspaceSession {
        WorkspaceSession {
            id: SessionId::new(),
            workspace_key: workspace_key(),
            name: "claude".into(),
            kind: SessionKind::Agent {
                agent_id: "claude".into(),
            },
            state,
            worktree_path: PathBuf::from("/tmp/x"),
            created_at: chrono::Utc::now(),
            last_output_at: None,
            layout: Default::default(),
        }
    }

    fn shell_session(state: SessionRunState) -> WorkspaceSession {
        WorkspaceSession {
            id: SessionId::new(),
            workspace_key: workspace_key(),
            name: "shell".into(),
            kind: SessionKind::Shell,
            state,
            worktree_path: PathBuf::from("/tmp/x"),
            created_at: chrono::Utc::now(),
            last_output_at: None,
            layout: Default::default(),
        }
    }

    // ── apply_agent_state ─────────────────────────────────────────

    #[test]
    fn empty_sessions_is_no_change() {
        let mut sessions: Vec<WorkspaceSession> = vec![];
        let t = apply_agent_state(&mut sessions, AgentState::Asking);
        assert_eq!(t, AttentionTransition::NoChange);
    }

    #[test]
    fn active_to_asking_reports_now_asking() {
        let mut sessions = vec![agent_session(SessionRunState::Active)];
        let t = apply_agent_state(&mut sessions, AgentState::Asking);
        assert_eq!(t, AttentionTransition::NowAsking);
        assert_eq!(sessions[0].state, SessionRunState::Asking);
    }

    #[test]
    fn asking_to_active_reports_no_longer_asking() {
        let mut sessions = vec![agent_session(SessionRunState::Asking)];
        let t = apply_agent_state(&mut sessions, AgentState::Active);
        assert_eq!(t, AttentionTransition::NoLongerAsking);
        assert_eq!(sessions[0].state, SessionRunState::Active);
    }

    #[test]
    fn asking_to_asking_is_no_change() {
        // Repeat broadcast of the same state — common when the daemon
        // re-emits on every chunk. Must not re-notify.
        let mut sessions = vec![agent_session(SessionRunState::Asking)];
        let t = apply_agent_state(&mut sessions, AgentState::Asking);
        assert_eq!(t, AttentionTransition::NoChange);
    }

    #[test]
    fn active_to_active_is_no_change() {
        let mut sessions = vec![agent_session(SessionRunState::Active)];
        let t = apply_agent_state(&mut sessions, AgentState::Active);
        assert_eq!(t, AttentionTransition::NoChange);
    }

    #[test]
    fn shell_sessions_are_not_mutated() {
        // Shell terminals never enter the Asking state — the agent
        // detector doesn't run on them. Make sure we don't trample
        // their Active state by accident.
        let mut sessions = vec![shell_session(SessionRunState::Active)];
        let t = apply_agent_state(&mut sessions, AgentState::Asking);
        assert_eq!(t, AttentionTransition::NoChange);
        assert_eq!(sessions[0].state, SessionRunState::Active);
    }

    #[test]
    fn mixed_sessions_only_agent_changes() {
        // One Claude session + one shell. The Asking event applies
        // only to the agent slot; the shell stays Active.
        let mut sessions = vec![
            agent_session(SessionRunState::Active),
            shell_session(SessionRunState::Active),
        ];
        let t = apply_agent_state(&mut sessions, AgentState::Asking);
        assert_eq!(t, AttentionTransition::NowAsking);
        assert_eq!(sessions[0].state, SessionRunState::Asking);
        assert_eq!(sessions[1].state, SessionRunState::Active);
    }

    #[test]
    fn workspace_is_asking_predicate() {
        let mut ws = sample_workspace();
        ws.sessions = vec![agent_session(SessionRunState::Active)];
        assert!(!workspace_is_asking(&ws));
        ws.sessions[0].state = SessionRunState::Asking;
        assert!(workspace_is_asking(&ws));
    }

    // ── next_asking_workspace ─────────────────────────────────────

    fn sample_workspace() -> Workspace {
        Workspace::empty(workspace_key(), "main", chrono::Utc::now())
    }

    fn sample_workspace_n(n: u32) -> Workspace {
        Workspace::empty(
            WorkspaceKey::new(format!("owner/repo#{n}")),
            "main",
            chrono::Utc::now(),
        )
    }

    fn ws_key(n: u32) -> SessionKey {
        SessionKey::from(&WorkspaceKey::new(format!("owner/repo#{n}")))
    }

    #[test]
    fn empty_keys_order_returns_none() {
        let map: HashMap<SessionKey, Workspace> = HashMap::new();
        assert_eq!(next_asking_workspace(&map, &[], None), None);
    }

    #[test]
    fn returns_none_when_nothing_is_asking() {
        let mut map = HashMap::new();
        let k = ws_key(1);
        let mut ws = sample_workspace();
        ws.sessions = vec![agent_session(SessionRunState::Active)];
        map.insert(k.clone(), ws);
        assert_eq!(next_asking_workspace(&map, &[k], None), None);
    }

    #[test]
    fn returns_only_asking_workspace() {
        let mut map = HashMap::new();
        let k = ws_key(1);
        let mut ws = sample_workspace();
        ws.sessions = vec![agent_session(SessionRunState::Asking)];
        map.insert(k.clone(), ws);
        assert_eq!(next_asking_workspace(&map, &[k.clone()], None), Some(k));
    }

    #[test]
    fn skips_past_current_to_next_asking() {
        // Two asking workspaces. With cursor on #1, `!` jumps to #2.
        let mut map = HashMap::new();
        for n in 1..=2u32 {
            let mut ws = sample_workspace_n(n);
            ws.sessions = vec![agent_session(SessionRunState::Asking)];
            map.insert(ws_key(n), ws);
        }
        let keys = vec![ws_key(1), ws_key(2)];
        assert_eq!(
            next_asking_workspace(&map, &keys, Some(&ws_key(1))),
            Some(ws_key(2))
        );
    }

    #[test]
    fn wraps_around_to_first_asking() {
        // Three workspaces — #1 asking, #2 not asking, #3 not asking.
        // From cursor on #2, the next-asking sweep wraps back to #1.
        let mut map = HashMap::new();
        for (n, state) in [
            (1u32, SessionRunState::Asking),
            (2, SessionRunState::Active),
            (3, SessionRunState::Active),
        ] {
            let mut ws = sample_workspace_n(n);
            ws.sessions = vec![agent_session(state)];
            map.insert(ws_key(n), ws);
        }
        let keys = vec![ws_key(1), ws_key(2), ws_key(3)];
        assert_eq!(
            next_asking_workspace(&map, &keys, Some(&ws_key(2))),
            Some(ws_key(1))
        );
    }

    #[test]
    fn from_none_starts_at_first_key() {
        // No current cursor — pick the first asking workspace in
        // `keys_order`. (#1 is not asking, #2 is.)
        let mut map = HashMap::new();
        for (n, state) in [
            (1u32, SessionRunState::Active),
            (2, SessionRunState::Asking),
        ] {
            let mut ws = sample_workspace_n(n);
            ws.sessions = vec![agent_session(state)];
            map.insert(ws_key(n), ws);
        }
        let keys = vec![ws_key(1), ws_key(2)];
        assert_eq!(next_asking_workspace(&map, &keys, None), Some(ws_key(2)));
    }
}
