//! Action-to-side-effect resolution.
//!
//! Every user-visible action (Work, Reply, Merge, Adopt, …) is a
//! two-step affair:
//!
//! 1. **Resolve** an `Intent` from the current workspace/pane state
//!    via a pure function in this module. No `&mut self`, no IPC
//!    sending — just `(Workspace, …) -> Intent`. Easy to test: one
//!    line per `(state, action) -> intent` cell.
//! 2. **Execute** the `Intent` in the orchestrator (e.g.,
//!    `Model::execute_intent`). The model holds the side-effect
//!    machinery (IPC client, modal stack, focus); the resolver
//!    doesn't.
//!
//! Why bother: today's `handle_pane_key` mixes both steps in every
//! match arm. The `w`-on-CI-failing-PR bug we shipped a fix for was
//! exactly the kind of thing this split prevents — when "what `w`
//! means" lives in a pure function, the test reads:
//!
//! ```text
//! let intent = resolve_work(Some(&ci_failing_pr), &[], "claude");
//! assert!(matches!(intent, Intent::SpawnAgent { prompt, .. }
//!     if prompt.unwrap().contains("CI is failing")));
//! ```
//!
//! Adding a new action becomes: add a resolver + tests, route it
//! from `handle_pane_key`, extend `execute_intent`. The model
//! itself stays a thin glue layer.
//!
//! Scope today: `Work` is the proof. Reply / Merge / Adopt / Kill /
//! Snooze etc. migrate next.

use std::time::Duration;

use pilot_core::{SessionKey, Workspace, WorkspaceKey};

/// What the model should do in response to an action. Carries the
/// data the side-effect needs (workspace key, prompt text, …) but
/// nothing about *how* to perform it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Spawn an agent (Claude / codex / cursor / …) in the named
    /// workspace, optionally pre-loaded with a prompt.
    SpawnAgent {
        workspace_key: SessionKey,
        agent_id: String,
        prompt: Option<String>,
    },
    /// Spawn a plain shell in the named workspace.
    SpawnShell {
        workspace_key: SessionKey,
    },
    /// Mount the reply textarea targeted at the workspace.
    MountReply {
        workspace_key: WorkspaceKey,
    },
    /// Mount the new-workspace name input.
    MountNewWorkspaceInput,
    /// Mount the adopt-target picker for moving sessions out of
    /// the named source workspace.
    MountAdoptPicker {
        source_key: WorkspaceKey,
    },
    /// Open the focused workspace's worktree in an editor. The
    /// model knows which editor (single → launch directly; multiple
    /// → mount a picker first).
    OpenEditor,
    /// Run the GraphQL `mergePullRequest` mutation for the focused
    /// workspace's PR. Two-press confirm latch is the model's job;
    /// this Intent is the fire-side payload.
    MergePr {
        workspace_key: WorkspaceKey,
    },
    /// Kill every running terminal under the workspace + remove
    /// the row. Two-press confirm at the model layer.
    KillWorkspace {
        session_key: SessionKey,
    },
    /// Snooze the workspace until `now + duration`. Producer is
    /// pure (`resolve_short_snooze` / `resolve_long_snooze`); the
    /// `Shift-Z` confirm latch lives in the model.
    Snooze {
        session_key: SessionKey,
        duration: Duration,
    },
    /// Unsnooze (reset the snoozed-until timestamp). The short-
    /// snooze resolver chooses Snooze vs. Unsnooze based on the
    /// workspace's current state.
    Unsnooze {
        session_key: SessionKey,
    },
    /// Bulk-mark every activity row on the workspace as read.
    MarkAllRead {
        session_key: SessionKey,
    },
    /// Show a transient footer notice. Used when an action fires but
    /// can't do anything meaningful in the current state (e.g.,
    /// "no sessions to adopt").
    Notice(String),
    /// The action is not applicable to the current state. Quiet
    /// no-op — no notice, no command. The matching contextual-footer
    /// hint should already not advertise the key.
    NoOp,
}

/// Resolve `w` ("work on this") for a workspace + selected-comment
/// indices. Single source of truth for the priority chain that
/// used to live in `Sidebar::work_target_for_cursor` AND the right
/// pane's `w` handler — now both sites call here.
///
/// Priority:
/// 1. Comments selected → `AddressComments` agent spawn.
/// 2. PR with CI failing → `FixCi` agent spawn.
/// 3. Issue-only workspace → `ImplementIssue` agent spawn.
/// 4. Anything else → `NoOp`.
pub fn resolve_work(
    workspace: Option<&Workspace>,
    selected_comments: &[usize],
    agent_id: &str,
) -> Intent {
    let Some(ws) = workspace else {
        return Intent::NoOp;
    };
    if !selected_comments.is_empty() {
        let prompt = build_address_comments_prompt(ws, selected_comments);
        return Intent::SpawnAgent {
            workspace_key: SessionKey::from(&ws.key),
            agent_id: agent_id.to_string(),
            prompt: Some(prompt),
        };
    }
    if let Some((session_key, prompt)) = crate::components::sidebar::build_fix_ci_prompt(ws) {
        return Intent::SpawnAgent {
            workspace_key: session_key,
            agent_id: agent_id.to_string(),
            prompt: Some(prompt),
        };
    }
    // Issue-only path: no PR slot but a gh_issue is present.
    if ws.pr.is_none()
        && let Some(issue) = ws.gh_issues.first()
    {
        let prompt = build_implement_issue_prompt(issue);
        return Intent::SpawnAgent {
            workspace_key: SessionKey::from(&ws.key),
            agent_id: agent_id.to_string(),
            prompt: Some(prompt),
        };
    }
    Intent::NoOp
}

/// Resolve `r` (reply). No state-dependent variation — either we
/// have a workspace to reply to or we don't. Kept as a resolver
/// anyway for uniformity: every action has exactly one place its
/// behaviour is defined.
pub fn resolve_reply(workspace: Option<&Workspace>) -> Intent {
    workspace
        .map(|w| Intent::MountReply {
            workspace_key: w.key.clone(),
        })
        .unwrap_or(Intent::NoOp)
}

/// Resolve `e` (open editor). Mirrors `resolve_reply`: present-or-
/// not. The model decides which editor to launch (single → direct,
/// multiple → picker); the resolver just signals "open whatever's
/// configured."
pub fn resolve_open_editor(workspace: Option<&Workspace>) -> Intent {
    if workspace.is_some() {
        Intent::OpenEditor
    } else {
        Intent::NoOp
    }
}

/// Resolve `n` (new workspace). Available regardless of focused row.
pub fn resolve_new_workspace() -> Intent {
    Intent::MountNewWorkspaceInput
}

/// Resolve `Shift-A` (adopt sessions). Workspace must have at least
/// one session to adopt; otherwise we surface a hint via `Notice`.
pub fn resolve_adopt(workspace: Option<&Workspace>) -> Intent {
    let Some(ws) = workspace else {
        return Intent::NoOp;
    };
    if ws.sessions.is_empty() {
        return Intent::Notice("no sessions on the focused workspace to adopt".into());
    }
    Intent::MountAdoptPicker {
        source_key: ws.key.clone(),
    }
}

/// Resolve `Shift-M` (merge). Same READY-gating the contextual
/// footer uses — the resolver and the hint share one predicate.
pub fn resolve_merge(workspace: Option<&Workspace>) -> Intent {
    let Some(ws) = workspace else {
        return Intent::NoOp;
    };
    let Some(pr) = ws.pr.as_ref() else {
        return Intent::NoOp;
    };
    if !matches!(
        pr.state,
        pilot_core::TaskState::Open | pilot_core::TaskState::InReview
    ) {
        return Intent::NoOp;
    }
    if !matches!(pr.review, pilot_core::ReviewStatus::Approved) {
        return Intent::NoOp;
    }
    if !matches!(
        pr.ci,
        pilot_core::CiStatus::Success | pilot_core::CiStatus::None
    ) {
        return Intent::NoOp;
    }
    if pr.has_conflicts {
        return Intent::NoOp;
    }
    Intent::MergePr {
        workspace_key: ws.key.clone(),
    }
}

/// Resolve `Shift-X` (kill workspace). Always available when a
/// workspace is focused — the model's two-press latch handles the
/// "are you sure" affordance.
pub fn resolve_kill(workspace: Option<&Workspace>) -> Intent {
    workspace
        .map(|w| Intent::KillWorkspace {
            session_key: SessionKey::from(&w.key),
        })
        .unwrap_or(Intent::NoOp)
}

/// Resolve `z` (short snooze). Toggle: if the workspace is already
/// snoozed, unsnooze; otherwise snooze for `duration`. Returns the
/// concrete `Snooze` / `Unsnooze` Intent.
pub fn resolve_short_snooze(
    workspace: Option<&Workspace>,
    now: chrono::DateTime<chrono::Utc>,
    duration: Duration,
) -> Intent {
    let Some(ws) = workspace else {
        return Intent::NoOp;
    };
    let session_key = SessionKey::from(&ws.key);
    if ws.is_snoozed(now) {
        Intent::Unsnooze { session_key }
    } else {
        Intent::Snooze {
            session_key,
            duration,
        }
    }
}

/// Resolve `Shift-Z` (long snooze, ~1 year). No toggle behaviour —
/// just snooze for `duration`. The model's `long_snooze_pending`
/// latch handles confirmation.
pub fn resolve_long_snooze(
    workspace: Option<&Workspace>,
    duration: Duration,
) -> Intent {
    workspace
        .map(|w| Intent::Snooze {
            session_key: SessionKey::from(&w.key),
            duration,
        })
        .unwrap_or(Intent::NoOp)
}

/// Resolve `m` (mark all read). One-shot.
pub fn resolve_mark_read(workspace: Option<&Workspace>) -> Intent {
    workspace
        .map(|w| Intent::MarkAllRead {
            session_key: SessionKey::from(&w.key),
        })
        .unwrap_or(Intent::NoOp)
}

/// Build the "address these comments" agent prompt. Lifted from
/// `right_pane.rs` so the resolver can call it without the right
/// pane depending on its own internals.
fn build_address_comments_prompt(workspace: &Workspace, indices: &[usize]) -> String {
    let pr_summary = workspace
        .pr
        .as_ref()
        .map(|pr| {
            let n = pr
                .id
                .key
                .rsplit_once('#')
                .map(|(_, n)| n)
                .unwrap_or(&pr.id.key);
            let repo = pr.repo.as_deref().unwrap_or("unknown");
            let branch = pr.branch.as_deref().unwrap_or("unknown");
            format!("PR #{n} in {repo} (branch `{branch}`)")
        })
        .unwrap_or_else(|| format!("workspace {}", workspace.key));

    let mut comments = String::new();
    for (i, idx) in indices.iter().enumerate() {
        let Some(act) = workspace.activity.get(*idx) else {
            continue;
        };
        comments.push_str(&format!("\n[{}] {} on {}:\n", i + 1, act.author, act.created_at));
        if let Some(path) = &act.path {
            if let Some(line) = act.line {
                comments.push_str(&format!("    file: {path}:{line}\n"));
            } else {
                comments.push_str(&format!("    file: {path}\n"));
            }
        }
        for line in act.body.lines() {
            comments.push_str(&format!("    {line}\n"));
        }
    }
    format!(
        "Address the following review comments on {pr_summary}:{comments}\n\n\
         For each comment: investigate, fix the code (or push back with a clear \
         technical rationale), then commit. When all the comments are addressed and \
         local checks pass, push the branch. After the push lands, reply to each \
         comment with the commit SHA and a one-line explanation of the change."
    )
}

/// Build the "implement this issue" prompt. Mirrors
/// `sidebar::build_implement_issue_prompt` (private over there)
/// so the resolver owns the logic.
fn build_implement_issue_prompt(issue: &pilot_core::Task) -> String {
    let issue_number = issue
        .id
        .key
        .rsplit_once('#')
        .map(|(_, n)| n)
        .unwrap_or(&issue.id.key);
    let repo = issue.repo.as_deref().unwrap_or("the repository");
    let body_block = match issue.body.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(body) => format!("\n\nIssue body:\n{body}\n"),
        None => String::new(),
    };
    format!(
        "Implement GitHub issue #{issue_number} in {repo}: {title}.\
         {body_block}\
         \nWalk through it: create a fresh branch from the repo's default base, \
         implement the change end-to-end (code + tests), run the project's local \
         checks until they pass, then `gh pr create` with a body that includes \
         `Closes #{issue_number}` so this issue and the resulting PR collapse to \
         a single row in pilot. Reply with the PR URL when it's open.",
        title = issue.title,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use pilot_core::{
        CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace,
        WorkspaceKey,
    };

    fn pr(key: &str, ci: CiStatus, review: ReviewStatus) -> Workspace {
        let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
        let task = Task {
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: format!("PR {key}"),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci,
            review,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/{path}/pull/{num}"),
            repo: Some("o/r".into()),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: Utc::now(),
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            has_conflicts: false,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: vec![],
        };
        Workspace::from_task(task, Utc::now())
    }

    fn issue(key: &str) -> Workspace {
        let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
        let mut t = pr(key, CiStatus::None, ReviewStatus::None);
        // Convert to issue: clear pr, attach as gh_issue.
        let mut task = t.pr.take().unwrap();
        task.url = format!("https://github.com/{path}/issues/{num}");
        t.attach_task(task);
        t
    }

    fn empty() -> Workspace {
        Workspace::empty(WorkspaceKey::new("k"), "main", Utc::now())
    }

    #[test]
    fn work_with_no_workspace_is_noop() {
        assert_eq!(resolve_work(None, &[], "claude"), Intent::NoOp);
    }

    #[test]
    fn work_on_ci_failing_pr_returns_fix_ci_agent() {
        let ws = pr("o/r#1", CiStatus::Failure, ReviewStatus::Pending);
        let intent = resolve_work(Some(&ws), &[], "claude");
        match intent {
            Intent::SpawnAgent { agent_id, prompt, .. } => {
                assert_eq!(agent_id, "claude");
                let prompt = prompt.expect("fix-CI carries a prompt");
                assert!(
                    prompt.contains("CI is failing"),
                    "{prompt}",
                );
            }
            other => panic!("expected SpawnAgent, got {other:?}"),
        }
    }

    #[test]
    fn work_on_healthy_pr_is_noop() {
        let ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Pending);
        assert_eq!(resolve_work(Some(&ws), &[], "claude"), Intent::NoOp);
    }

    #[test]
    fn work_on_ready_pr_is_noop() {
        // READY (approved + green) has its own action — Merge. `w`
        // should NOT also fire here.
        let ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Approved);
        assert_eq!(resolve_work(Some(&ws), &[], "claude"), Intent::NoOp);
    }

    #[test]
    fn work_on_issue_returns_implement_agent() {
        let ws = issue("o/r#42");
        let intent = resolve_work(Some(&ws), &[], "claude");
        match intent {
            Intent::SpawnAgent { prompt, .. } => {
                let prompt = prompt.expect("implement carries a prompt");
                assert!(
                    prompt.contains("Implement GitHub issue #42"),
                    "{prompt}",
                );
            }
            other => panic!("expected SpawnAgent, got {other:?}"),
        }
    }

    #[test]
    fn work_on_empty_workspace_is_noop() {
        assert_eq!(resolve_work(Some(&empty()), &[], "claude"), Intent::NoOp);
    }

    #[test]
    fn selected_comments_beat_ci_failure() {
        // Comments-selected path wins even when CI is red — the user
        // explicitly chose what to address.
        let mut ws = pr("o/r#1", CiStatus::Failure, ReviewStatus::Pending);
        ws.activity.push(pilot_core::Activity {
            author: "alice".into(),
            body: "needs more tests".into(),
            created_at: Utc::now(),
            kind: pilot_core::ActivityKind::Comment,
            node_id: None,
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        });
        let intent = resolve_work(Some(&ws), &[0], "claude");
        match intent {
            Intent::SpawnAgent { prompt, .. } => {
                let prompt = prompt.expect("carries prompt");
                assert!(
                    prompt.contains("Address the following review comments"),
                    "selected comments must beat fix-CI; got:\n{prompt}",
                );
            }
            other => panic!("expected SpawnAgent, got {other:?}"),
        }
    }

    // ── Reply ────────────────────────────────────────────────────

    #[test]
    fn reply_with_no_workspace_is_noop() {
        assert_eq!(resolve_reply(None), Intent::NoOp);
    }

    #[test]
    fn reply_with_workspace_mounts_reply() {
        let ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        match resolve_reply(Some(&ws)) {
            Intent::MountReply { workspace_key } => assert_eq!(workspace_key, ws.key),
            other => panic!("expected MountReply, got {other:?}"),
        }
    }

    // ── Open editor ──────────────────────────────────────────────

    #[test]
    fn open_editor_with_no_workspace_is_noop() {
        assert_eq!(resolve_open_editor(None), Intent::NoOp);
    }

    #[test]
    fn open_editor_with_workspace_returns_open_editor() {
        let ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        assert_eq!(resolve_open_editor(Some(&ws)), Intent::OpenEditor);
    }

    // ── New workspace ────────────────────────────────────────────

    #[test]
    fn new_workspace_is_always_available() {
        assert_eq!(resolve_new_workspace(), Intent::MountNewWorkspaceInput);
    }

    // ── Adopt sessions ───────────────────────────────────────────

    #[test]
    fn adopt_with_no_workspace_is_noop() {
        assert_eq!(resolve_adopt(None), Intent::NoOp);
    }

    #[test]
    fn adopt_with_empty_workspace_surfaces_notice() {
        let ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        match resolve_adopt(Some(&ws)) {
            Intent::Notice(msg) => assert!(msg.contains("no sessions"), "{msg}"),
            other => panic!("expected Notice, got {other:?}"),
        }
    }

    #[test]
    fn adopt_with_sessions_mounts_picker() {
        let mut ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        ws.add_session(pilot_core::WorkspaceSession::new(
            ws.key.clone(),
            pilot_core::SessionKind::Shell,
            std::path::PathBuf::from("/tmp"),
            Utc::now(),
        ));
        match resolve_adopt(Some(&ws)) {
            Intent::MountAdoptPicker { source_key } => assert_eq!(source_key, ws.key),
            other => panic!("expected MountAdoptPicker, got {other:?}"),
        }
    }

    // ── Merge ────────────────────────────────────────────────────

    #[test]
    fn merge_on_ready_pr_returns_merge_intent() {
        let ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Approved);
        match resolve_merge(Some(&ws)) {
            Intent::MergePr { workspace_key } => assert_eq!(workspace_key, ws.key),
            other => panic!("expected MergePr, got {other:?}"),
        }
    }

    #[test]
    fn merge_without_approval_is_noop() {
        let ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Pending);
        assert_eq!(resolve_merge(Some(&ws)), Intent::NoOp);
    }

    #[test]
    fn merge_with_red_ci_is_noop() {
        let ws = pr("o/r#1", CiStatus::Failure, ReviewStatus::Approved);
        assert_eq!(resolve_merge(Some(&ws)), Intent::NoOp);
    }

    #[test]
    fn merge_on_issue_is_noop() {
        let ws = issue("o/r#42");
        assert_eq!(resolve_merge(Some(&ws)), Intent::NoOp);
    }

    // ── Kill ─────────────────────────────────────────────────────

    #[test]
    fn kill_with_no_workspace_is_noop() {
        assert_eq!(resolve_kill(None), Intent::NoOp);
    }

    #[test]
    fn kill_with_workspace_returns_kill_intent() {
        let ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        match resolve_kill(Some(&ws)) {
            Intent::KillWorkspace { session_key } => {
                assert_eq!(session_key.as_str(), ws.key.as_str());
            }
            other => panic!("expected KillWorkspace, got {other:?}"),
        }
    }

    // ── Snooze (short) ───────────────────────────────────────────

    #[test]
    fn short_snooze_with_no_workspace_is_noop() {
        assert_eq!(
            resolve_short_snooze(None, Utc::now(), Duration::from_secs(4 * 3600)),
            Intent::NoOp
        );
    }

    #[test]
    fn short_snooze_on_fresh_workspace_snoozes() {
        let ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        let d = Duration::from_secs(4 * 3600);
        match resolve_short_snooze(Some(&ws), Utc::now(), d) {
            Intent::Snooze { duration, .. } => assert_eq!(duration, d),
            other => panic!("expected Snooze, got {other:?}"),
        }
    }

    #[test]
    fn short_snooze_on_already_snoozed_workspace_unsnoozes() {
        // Toggle behavior: pressing `z` twice undoes the snooze.
        let mut ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        ws.snoozed_until = Some(Utc::now() + chrono::Duration::hours(1));
        match resolve_short_snooze(Some(&ws), Utc::now(), Duration::from_secs(60)) {
            Intent::Unsnooze { .. } => {}
            other => panic!("expected Unsnooze, got {other:?}"),
        }
    }

    // ── Snooze (long) ────────────────────────────────────────────

    #[test]
    fn long_snooze_always_snoozes() {
        // Unlike short-snooze, the long-snooze does NOT toggle —
        // pressing Shift-Z twice (after the confirm latch) snoozes
        // for another year. That's the model's contract; pin it.
        let mut ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        ws.snoozed_until = Some(Utc::now() + chrono::Duration::hours(1));
        let d = Duration::from_secs(365 * 24 * 3600);
        match resolve_long_snooze(Some(&ws), d) {
            Intent::Snooze { duration, .. } => assert_eq!(duration, d),
            other => panic!("expected Snooze, got {other:?}"),
        }
    }

    // ── Mark read ────────────────────────────────────────────────

    #[test]
    fn mark_read_with_workspace_returns_mark_all_read() {
        let ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        match resolve_mark_read(Some(&ws)) {
            Intent::MarkAllRead { session_key } => {
                assert_eq!(session_key.as_str(), ws.key.as_str());
            }
            other => panic!("expected MarkAllRead, got {other:?}"),
        }
    }

    #[test]
    fn agent_id_is_honored() {
        let ws = pr("o/r#1", CiStatus::Failure, ReviewStatus::Pending);
        let intent = resolve_work(Some(&ws), &[], "codex");
        match intent {
            Intent::SpawnAgent { agent_id, .. } => assert_eq!(agent_id, "codex"),
            other => panic!("expected SpawnAgent, got {other:?}"),
        }
    }
}
