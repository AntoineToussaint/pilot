use tokio::sync::mpsc;

use crate::action::{Action, ShellKind};
use crate::app::{self, App};

pub(crate) const MONITOR_MAX_CI_RETRIES: u32 = 3;

/// Drive the monitor state machine for a session.
pub(crate) fn handle_monitor_tick(
    app: &mut App,
    session_key: &str,
    action_tx: &mpsc::UnboundedSender<Action>,
) {
    let Some(session) = app.sessions.get(session_key) else { return };
    let Some(monitor) = &session.monitor else { return };

    match monitor {
        pilot_core::MonitorState::Idle => {
            if session.primary_task.ci == pilot_core::CiStatus::Failure {
                let display_name = session.display_name.clone();
                let Some(session) = app.sessions.get_mut(session_key) else { return };
                session.monitor = Some(pilot_core::MonitorState::CiFixing { attempt: 1 });
                app.status = format!("Monitor: fixing CI for {display_name}");
                spawn_monitor_claude_fix(app, session_key, action_tx);
            }
        }
        pilot_core::MonitorState::WaitingCi { after_attempt } => {
            let ci = session.primary_task.ci;
            let after_attempt = *after_attempt;
            let display_name = session.display_name.clone();
            match ci {
                pilot_core::CiStatus::Success => {
                    let Some(session) = app.sessions.get_mut(session_key) else { return };
                    session.monitor = Some(pilot_core::MonitorState::Idle);
                    app.status = format!("Monitor: CI passed for {display_name}");
                }
                pilot_core::CiStatus::Failure => {
                    if after_attempt >= MONITOR_MAX_CI_RETRIES {
                        let Some(session) = app.sessions.get_mut(session_key) else { return };
                        session.monitor = Some(pilot_core::MonitorState::Failed {
                            reason: format!("CI still failing after {after_attempt} attempts"),
                        });
                        app.status = format!("Monitor: gave up on {display_name} after {after_attempt} attempts");
                    } else {
                        let Some(session) = app.sessions.get_mut(session_key) else { return };
                        session.monitor = Some(pilot_core::MonitorState::CiFixing { attempt: after_attempt + 1 });
                        app.status = format!("Monitor: retry #{} for {display_name}", after_attempt + 1);
                        spawn_monitor_claude_fix(app, session_key, action_tx);
                    }
                }
                _ => {} // Still pending/running, keep waiting.
            }
        }
        pilot_core::MonitorState::CiFixing { .. } => {
            // Push detected via MCP auto-approve — transition to WaitingCi.
            let attempt = match &session.monitor {
                Some(pilot_core::MonitorState::CiFixing { attempt }) => *attempt,
                _ => 1,
            };
            let Some(session) = app.sessions.get_mut(session_key) else { return };
            session.monitor = Some(pilot_core::MonitorState::WaitingCi { after_attempt: attempt });
            app.status = format!("Monitor: pushed fix, waiting for CI on {}", session.display_name);
        }
        pilot_core::MonitorState::Rebasing => {
            // Rebase completed (async task sent this tick) — wait for CI.
            let Some(session) = app.sessions.get_mut(session_key) else { return };
            session.monitor = Some(pilot_core::MonitorState::WaitingCi { after_attempt: 0 });
            app.status = format!("Monitor: rebased, waiting for CI on {}", session.display_name);
        }
        pilot_core::MonitorState::Failed { .. } => {}
    }
}

/// Spawn Claude Code to fix CI failures for a monitored session.
pub(crate) fn spawn_monitor_claude_fix(
    app: &mut App,
    session_key: &str,
    _action_tx: &mpsc::UnboundedSender<Action>,
) {
    // If no terminal, spawn one.
    if !app.terminals.contains_key(session_key) {
        let worktree_path = app.sessions.get(session_key)
            .and_then(|s| s.worktree_path.clone());
        if let Some(path) = worktree_path {
            app::spawn_terminal(app, session_key, path, ShellKind::Claude);
        } else {
            tracing::warn!("Monitor: no worktree for {session_key}, can't spawn Claude");
            if let Some(session) = app.sessions.get_mut(session_key) {
                session.monitor = Some(pilot_core::MonitorState::Failed {
                    reason: "No worktree available".into(),
                });
            }
            return;
        }
    } else if !app.terminal_kinds.get(session_key)
        .map(|k| matches!(k, ShellKind::Claude))
        .unwrap_or(false)
    {
        // Terminal exists but it's a shell, not Claude — can't inject a Claude prompt.
        tracing::warn!("Monitor: terminal for {session_key} is a shell, not Claude");
        if let Some(session) = app.sessions.get_mut(session_key) {
            session.monitor = Some(pilot_core::MonitorState::Failed {
                reason: "Shell terminal running, not Claude".into(),
            });
        }
        return;
    }

    let Some(session) = app.sessions.get(session_key) else { return };
    let task = &session.primary_task;

    // Build CI-focused prompt.
    let mut prompt = String::new();
    prompt.push_str("# Task: Fix CI failures\n\n");
    prompt.push_str("## PR\n\n");
    prompt.push_str(&format!("- **Title:** {}\n", task.title));
    prompt.push_str(&format!("- **URL:** {}\n", task.url));
    if let Some(ref branch) = task.branch {
        prompt.push_str(&format!("- **Branch:** `{branch}`\n"));
    }
    prompt.push_str(&format!("- **CI:** {:?}\n", task.ci));

    prompt.push_str("\n## Failed CI Checks\n\n");
    let mut has_failures = false;
    for check in &task.checks {
        if check.status == pilot_core::CiStatus::Failure {
            has_failures = true;
            prompt.push_str(&format!("- **{}**", check.name));
            if let Some(ref url) = check.url {
                prompt.push_str(&format!(" — [logs]({url})"));
            }
            prompt.push_str("\n");
        }
    }
    if !has_failures {
        prompt.push_str("- (no individual check details available — investigate via `pilot_get_pr_state`)\n");
    }

    prompt.push_str("\n## Instructions\n\n");
    prompt.push_str("CI is failing on this PR. Please:\n\n");
    prompt.push_str("1. Use `pilot_get_pr_state` to check current CI status and details\n");
    prompt.push_str("2. Investigate the failing checks — read logs, reproduce locally\n");
    prompt.push_str("3. Make the necessary code changes to fix them\n");
    prompt.push_str("4. Use `pilot_push` to push your fix (NOT `git push`)\n\n");
    prompt.push_str("**IMPORTANT:** You have access to MCP tools provided by pilot. ");
    prompt.push_str("Use these instead of raw `git` or `gh` commands:\n\n");
    prompt.push_str("- `pilot_get_pr_state` — fetch live PR state (CI, reviews)\n");
    prompt.push_str("- `pilot_push` — push commits (auto-approved in monitor mode)\n");
    prompt.push_str("- `pilot_reply` — post a comment\n\n");

    // Write context file.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let context_dir = std::path::PathBuf::from(&home).join(".pilot").join("context");
    let _ = std::fs::create_dir_all(&context_dir);
    let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
    let safe_key = session_key.replace(':', "_").replace('/', "_");
    let context_file = context_dir.join(format!("{safe_key}_monitor_{timestamp}.md"));
    if let Err(e) = std::fs::write(&context_file, &prompt) {
        tracing::error!("Failed to write monitor context: {e}");
        return;
    }
    // Update the stable latest link for pilot_get_context.
    let latest = context_dir.join(format!("{safe_key}.md"));
    let _ = std::fs::remove_file(&latest);
    let _ = std::fs::copy(&context_file, &latest);

    // Queue the prompt — it will be injected when Claude is idle.
    app.pending_prompts.insert(session_key.to_string(), prompt);

    app.last_claude_send = Some(std::time::Instant::now());
}

/// Check if a PR has merge conflicts via `gh pr view`.
pub(crate) async fn check_needs_rebase(repo: &str, pr_number: &str) -> bool {
    let output = tokio::process::Command::new("gh")
        .args(["pr", "view", pr_number, "--repo", repo, "--json", "mergeable"])
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => {
            let json: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap_or_default();
            json.get("mergeable")
                .and_then(|v| v.as_str())
                == Some("CONFLICTING")
        }
        _ => false,
    }
}

/// Run git rebase in a worktree, then force-push.
pub(crate) async fn run_rebase(
    wt_path: &std::path::Path,
    tx: mpsc::UnboundedSender<Action>,
    session_key: String,
    default_branch: &str,
) {
    let fetch = tokio::process::Command::new("git")
        .current_dir(wt_path)
        .args(["fetch", "origin", default_branch])
        .output()
        .await;
    if !fetch.map(|o| o.status.success()).unwrap_or(false) {
        tracing::error!("Monitor: git fetch failed for {session_key}");
        return;
    }

    let rebase_target = format!("origin/{default_branch}");
    let rebase = tokio::process::Command::new("git")
        .current_dir(wt_path)
        .args(["rebase", &rebase_target])
        .output()
        .await;

    match rebase {
        Ok(o) if o.status.success() => {
            tracing::info!("Monitor: rebase succeeded for {session_key}");
            let push = tokio::process::Command::new("git")
                .current_dir(wt_path)
                .args(["push", "--force-with-lease"])
                .output()
                .await;
            match push {
                Ok(o) if o.status.success() => {
                    tracing::info!("Monitor: force-pushed rebased branch for {session_key}");
                    let _ = tx.send(Action::MonitorTick { session_key });
                }
                Ok(o) => {
                    let err = String::from_utf8_lossy(&o.stderr);
                    tracing::error!("Monitor: push after rebase failed for {session_key}: {err}");
                }
                Err(e) => {
                    tracing::error!("Monitor: push error for {session_key}: {e}");
                }
            }
        }
        _ => {
            tracing::warn!("Monitor: rebase failed for {session_key}, aborting");
            let _ = tokio::process::Command::new("git")
                .current_dir(wt_path)
                .args(["rebase", "--abort"])
                .output()
                .await;
        }
    }
}
