//! Claude Code state detection via the official hooks system.
//!
//! We install a `.claude/settings.local.json` in each worktree before
//! spawning Claude. Its hooks write one-line JSON state transitions to
//! `~/.pilot/ipc/<session>.state` whenever Claude starts working, asks
//! a question, or finishes. The Tick handler reads these files back
//! and drives the sidebar / terminal-title indicator.
//!
//! Why hooks over output parsing: the TUI spinner glyphs, prompt
//! formatting, and permission UI all shift between releases and can be
//! user-customized. Hooks are documented lifecycle events that fire
//! deterministically in interactive mode.
//!
//! See https://code.claude.com/docs/en/hooks for the reference.

use std::fs;
use std::path::{Path, PathBuf};

/// Directory holding per-session hook-state files.
pub fn ipc_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    home.join(".pilot").join("ipc")
}

/// Sanitize a session key (`"github:owner/repo#123"`) into a filename
/// chunk. Must match the naming used in state_file_for.
fn safe_name(session_key: &str) -> String {
    session_key.replace([':', '/'], "_")
}

/// Path to the state file for a given session key.
pub fn state_file_for(session_key: &str) -> PathBuf {
    ipc_dir().join(format!("{}.state", safe_name(session_key)))
}

/// Hook-reported Claude state. Mirrors `AgentState` but kept separate so
/// transport format changes don't ripple.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookState {
    Working,
    Asking,
    Idle,
    /// Session ended. We don't expose this to UI — the terminal exits
    /// on its own and agent_states is cleaned by enforce_invariants.
    Stopped,
}

/// Read the latest state transition written by Claude's hooks for
/// this session. Returns `(state, age_secs_since_ts)` if present.
pub fn read_state(session_key: &str) -> Option<(HookState, u64)> {
    let path = state_file_for(session_key);
    let content = fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(content.trim()).ok()?;
    let label = v.get("state")?.as_str()?;
    let ts = v.get("ts")?.as_u64()?;
    let state = match label {
        "working" => HookState::Working,
        "asking" => HookState::Asking,
        "idle" => HookState::Idle,
        "stopped" => HookState::Stopped,
        _ => return None,
    };
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let age = now_secs.saturating_sub(ts);
    Some((state, age))
}

/// Remove any lingering state file for this session — called on close
/// so a restarted Claude doesn't pick up the previous run's last state.
pub fn clear_state(session_key: &str) {
    let _ = fs::remove_file(state_file_for(session_key));
}

/// Write a `.claude/settings.local.json` in the worktree registering
/// the lifecycle hooks that emit state transitions. Overwrites any
/// existing file — pilot owns the `.claude/` directory for its
/// worktrees. Returns Ok(()) even if setup partially fails (we don't
/// want a hook-setup hiccup to block Claude from launching).
pub fn install_hooks(worktree: &Path, session_key: &str) -> std::io::Result<()> {
    fs::create_dir_all(ipc_dir())?;
    let state_path = state_file_for(session_key);
    let claude_dir = worktree.join(".claude");
    fs::create_dir_all(&claude_dir)?;

    // Build a shell command that atomically overwrites the state file
    // with one line of JSON: {"state":"<label>","ts":<unix_secs>}.
    // `date +%s` is universally available; %N (nanoseconds) is GNU-only
    // so we stick to whole seconds — plenty of granularity for UI.
    let state_path_str = state_path.to_string_lossy();
    let cmd = |label: &str| -> String {
        // Quote the path in case it contains spaces. The `printf`/
        // redirect pair is atomic enough for our needs (single write).
        format!(
            "printf '{{\"state\":\"{label}\",\"ts\":%s}}\\n' \"$(date +%s)\" > \"{state_path_str}\""
        )
    };

    // The hook schema is a map of event name → list of groups, each
    // group having an optional `matcher` and a list of `hooks` (commands
    // to run). We use one group per event with a single command each.
    let settings = serde_json::json!({
        "hooks": {
            "UserPromptSubmit": [{
                "hooks": [{ "type": "command", "command": cmd("working") }]
            }],
            "PreToolUse": [{
                "hooks": [{ "type": "command", "command": cmd("working") }]
            }],
            "PostToolUse": [{
                "hooks": [{ "type": "command", "command": cmd("working") }]
            }],
            // Notification covers permission_prompt + elicitation_dialog
            // + idle_prompt. We treat all three as "needs attention".
            "Notification": [{
                "hooks": [{ "type": "command", "command": cmd("asking") }]
            }],
            // Newer (>=v2.1) dedicated hooks for permission / elicitation.
            // Harmless on older versions — Claude ignores unknown events.
            "PermissionRequest": [{
                "hooks": [{ "type": "command", "command": cmd("asking") }]
            }],
            "Elicitation": [{
                "hooks": [{ "type": "command", "command": cmd("asking") }]
            }],
            "Stop": [{
                "hooks": [{ "type": "command", "command": cmd("idle") }]
            }],
            "SessionEnd": [{
                "hooks": [{ "type": "command", "command": cmd("stopped") }]
            }],
        }
    });

    let path = claude_dir.join("settings.local.json");
    fs::write(&path, serde_json::to_string_pretty(&settings)?)?;
    tracing::info!("Installed Claude hooks for {session_key} at {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_name_replaces_separators() {
        assert_eq!(safe_name("github:owner/repo#123"), "github_owner_repo#123");
    }

    #[test]
    fn state_file_path_uses_safe_name() {
        let p = state_file_for("github:o/r#1");
        assert!(p.to_string_lossy().ends_with("github_o_r#1.state"));
    }

    #[test]
    fn parse_state_line() {
        // Exercise the parser without touching the real ipc_dir.
        let line = r#"{"state":"working","ts":1700000000}"#;
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["state"], "working");
        assert_eq!(v["ts"], 1700000000);
    }
}
