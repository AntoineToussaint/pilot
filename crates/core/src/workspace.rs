//! `Workspace` and `Session` — v2's hierarchy.
//!
//! ## The hierarchy (canonical)
//!
//! ```text
//! Repo            owner/name string from the task's provider.
//!  └─ Workspace   one unit of work; one PR + linked issues.
//!      └─ Session = one folder worktree on disk.
//!          └─ Terminal  one PTY rooted in that folder.
//! ```
//!
//! Each layer has a single responsibility; deviations are bugs:
//!
//! - **Repo** isn't a struct — it's the `task.repo` string. Multiple
//!   workspaces can share a repo (different PRs in the same repo).
//! - **Workspace** is the unit of work. Plain serializable record;
//!   no behavior trait — variation lives in providers and backends.
//! - **Session** = **one folder worktree.** A workspace with no
//!   sessions has no worktree (purely tracking). Multiple sessions
//!   per workspace = multiple worktrees for the same PR (review
//!   folder + experiment folder, etc).
//! - **Terminal** is a PTY belonging to a session — never directly to
//!   a workspace. Without a session there's no folder, so there's
//!   nothing for a terminal to root in.
//!
//! ## Why two levels (Workspace vs Session)
//!
//! v1 conflated them: every task was one Session with one terminal.
//! That blocked use cases like running Claude AND Codex on the same
//! PR in parallel, or having a long-running shell next to an agent.
//! v2 separates "the unit of work" (Workspace) from "the running
//! thing" (Session) so the model maps cleanly onto reality.
//!
//! Both persist across pilot restarts: the daemon keeps the worktree
//! and the PTYs alive, the store remembers everything else.
//!
//! ## Coexistence with v1's `Session`
//!
//! v1's `pilot_core::Session` (in `session.rs`) stays exactly as it
//! is — v1 still imports it. The v2 daemon's migration shim converts
//! a v1 Session into a v2 Workspace at upgrade time via
//! `Workspace::from_v1_session`.

use crate::task::{Activity, Task, TaskId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use uuid::Uuid;

/// Stable identifier for a workspace. Human-readable so it survives
/// renames and shows up well in logs / UIs ("fix-auth-2026-04").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceKey(pub String);

impl WorkspaceKey {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WorkspaceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stable identifier for a session within a workspace. UUID so we can
/// allocate one client-side without round-tripping the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceState {
    /// No worktree on disk yet — workspace is just metadata.
    Pending,
    /// Worktree being created (clone, branch checkout).
    CheckingOut,
    /// Active workspace with a worktree the user can run sessions in.
    Active,
    /// User snoozed it.
    Snoozed,
    /// User archived (or PR merged + auto-archive).
    Archived,
}

/// One workspace = one unit of work (PR + linked issues), holding
/// **zero or more sessions**. A session is one folder worktree on
/// disk; without sessions the workspace is purely a tracking row
/// with no on-disk presence.
///
/// Compatibility: `worktree_path` (the v2-pre-session field) is kept
/// only for deserializing older persisted records. Live code reads
/// `sessions[0].worktree_path` instead. `Workspace::from_v1_session`
/// migrates the legacy field into a freshly-allocated session at
/// upgrade time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub key: WorkspaceKey,
    /// Display name. Defaults to the PR title or the first issue's
    /// title when first created; user can rename.
    pub name: String,
    /// Branch this workspace tracks. Required — even a "from scratch"
    /// workspace lives on a branch.
    pub branch: String,
    /// Legacy single-worktree pointer. New code writes
    /// [`Self::sessions`] instead; this field is read-only and
    /// migrated forward on `Workspace::from_v1_session`.
    #[serde(default)]
    pub worktree_path: Option<PathBuf>,
    /// Live runtime sessions. **Each session = one folder worktree.**
    /// Zero sessions = no on-disk presence. Multiple sessions = the
    /// user opened separate worktrees for the same branch (review +
    /// experiment, agent A + agent B, etc.).
    #[serde(default)]
    pub sessions: Vec<Session>,
    /// At most one PR.
    pub pr: Option<Task>,
    pub gh_issues: Vec<Task>,
    pub linear_issues: Vec<Task>,
    /// Merged activity from every linked task, sorted newest-first.
    pub activity: Vec<Activity>,
    pub seen_count: usize,
    #[serde(default)]
    pub read_indices: HashSet<usize>,
    #[serde(default)]
    pub snoozed_until: Option<DateTime<Utc>>,
    pub state: WorkspaceState,
    pub created_at: DateTime<Utc>,
    pub last_viewed_at: Option<DateTime<Utc>>,
}

impl Workspace {
    /// Empty workspace on `branch` with no linked tasks. Used for the
    /// "create a workspace from scratch" path.
    pub fn empty(key: WorkspaceKey, branch: impl Into<String>, now: DateTime<Utc>) -> Self {
        let branch = branch.into();
        Self {
            name: key.as_str().to_string(),
            key,
            branch,
            worktree_path: None,
            sessions: Vec::new(),
            pr: None,
            gh_issues: Vec::new(),
            linear_issues: Vec::new(),
            activity: Vec::new(),
            seen_count: 0,
            read_indices: HashSet::new(),
            snoozed_until: None,
            state: WorkspaceState::Pending,
            created_at: now,
            last_viewed_at: None,
        }
    }

    /// Append a fresh session and return its id. Sessions own a
    /// worktree path; the workspace becomes "live on disk" only once
    /// at least one session has been added.
    pub fn add_session(&mut self, session: Session) -> SessionId {
        let id = session.id;
        self.sessions.push(session);
        id
    }

    /// Drop the session with `id` if present. Returns `true` if a
    /// session was actually removed. The caller is responsible for
    /// cleaning up the worktree on disk.
    pub fn remove_session(&mut self, id: SessionId) -> bool {
        let before = self.sessions.len();
        self.sessions.retain(|s| s.id != id);
        before != self.sessions.len()
    }

    pub fn find_session(&self, id: SessionId) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// The session pilot should target when the user issues a workspace-
    /// scoped action without picking a specific session. Today: the
    /// most recently created. None when the workspace has no sessions.
    pub fn default_session(&self) -> Option<&Session> {
        self.sessions
            .iter()
            .max_by_key(|s| s.created_at)
    }

    /// Build a workspace from a single task as the seed. PRs become
    /// the workspace's PR slot; issues go into `gh_issues` /
    /// `linear_issues`. Used when the daemon discovers a new task
    /// that isn't yet attached to anything.
    pub fn from_task(task: Task, now: DateTime<Utc>) -> Self {
        let key = WorkspaceKey::new(workspace_key_for(&task));
        let branch = task
            .branch
            .clone()
            .unwrap_or_else(|| key.as_str().to_string());
        let mut ws = Self::empty(key, branch, now);
        ws.name = task.title.clone();
        ws.activity = task.recent_activity.clone();
        ws.attach_task(task);
        ws
    }

    /// Sort the activity list by `created_at` descending. Idempotent.
    pub fn sort_activity(&mut self) {
        self.activity
            .sort_by(|a, b| b.created_at.cmp(&a.created_at));
    }

    /// Attach a task to this workspace. Routing rules:
    /// - PR → the `pr` slot (replaces the existing one if any).
    /// - GitHub issue → `gh_issues` (de-duped by `TaskId`).
    /// - Linear ticket → `linear_issues` (de-duped by `TaskId`).
    /// - Anything else → silently dropped (we don't have a slot).
    ///
    /// Activity from `task.recent_activity` is merged into the
    /// workspace's feed and de-duplicated.
    pub fn attach_task(&mut self, task: Task) {
        match classify(&task) {
            TaskSlot::Pr => self.pr = Some(task.clone()),
            TaskSlot::GhIssue => upsert_by_id(&mut self.gh_issues, task.clone()),
            TaskSlot::LinearIssue => upsert_by_id(&mut self.linear_issues, task.clone()),
            TaskSlot::Unknown => return,
        }
        self.merge_activity(&task.recent_activity);
    }

    /// Detach by id — works on any slot. Silently no-op if missing.
    pub fn detach_task(&mut self, id: &TaskId) {
        if self.pr.as_ref().map(|p| &p.id) == Some(id) {
            self.pr = None;
        }
        self.gh_issues.retain(|t| &t.id != id);
        self.linear_issues.retain(|t| &t.id != id);
    }

    /// Every linked task's id, deduplicated.
    pub fn linked_task_ids(&self) -> Vec<TaskId> {
        let mut out = Vec::new();
        if let Some(pr) = &self.pr {
            out.push(pr.id.clone());
        }
        out.extend(self.gh_issues.iter().map(|t| t.id.clone()));
        out.extend(self.linear_issues.iter().map(|t| t.id.clone()));
        out
    }

    /// Merge a slice of activity items into the feed, de-duping by
    /// (author, body, created_at) and re-sorting. Cheap to call
    /// repeatedly — provider polls produce overlapping feeds.
    pub fn merge_activity(&mut self, incoming: &[Activity]) {
        for act in incoming {
            let already = self.activity.iter().any(|a| {
                a.author == act.author && a.body == act.body && a.created_at == act.created_at
            });
            if !already {
                self.activity.push(act.clone());
            }
        }
        self.sort_activity();
    }

    /// The "headline" task for this workspace — the one components
    /// like the sidebar row and the right-pane header render. PRs
    /// always win over issues; among issues we pick the first GitHub
    /// issue, then the first Linear issue. None only when the
    /// workspace was created empty (`Workspace::empty`) and nothing
    /// has been attached yet.
    pub fn primary_task(&self) -> Option<&Task> {
        self.pr
            .as_ref()
            .or_else(|| self.gh_issues.first())
            .or_else(|| self.linear_issues.first())
    }

    /// Number of activity items the user hasn't seen.
    pub fn unread_count(&self) -> usize {
        (0..self.activity.len().saturating_sub(self.seen_count))
            .filter(|i| !self.read_indices.contains(i))
            .count()
    }

    /// Whether the activity at `index` is currently unread.
    /// Mirrors v1 `Session::is_activity_unread` so the right pane can
    /// surface unread markers next to each row.
    pub fn is_activity_unread(&self, index: usize) -> bool {
        index < self.activity.len().saturating_sub(self.seen_count)
            && !self.read_indices.contains(&index)
    }

    /// Mark every currently-known activity item as read. Called when
    /// the user opens the workspace and all items become "seen".
    pub fn mark_read_all(&mut self) {
        self.seen_count = self.activity.len();
        self.read_indices.clear();
    }

    pub fn is_snoozed(&self, now: DateTime<Utc>) -> bool {
        match self.snoozed_until {
            Some(until) => until > now,
            None => false,
        }
    }

    /// Convert a v1 `Session` into a v2 `Workspace`. Used by the
    /// migration shim on first v2 launch.
    pub fn from_v1_session(s: crate::session::Session) -> Self {
        let key = WorkspaceKey::new(workspace_key_for(&s.primary_task));
        let branch = s
            .primary_task
            .branch
            .clone()
            .unwrap_or_else(|| key.as_str().to_string());
        let mut ws = Self {
            name: s.display_name,
            key: key.clone(),
            branch,
            // Old field stays for backwards-compat reads. Live code
            // looks at `sessions` instead.
            worktree_path: s.worktree_path.clone(),
            sessions: Vec::new(),
            pr: None,
            gh_issues: Vec::new(),
            linear_issues: Vec::new(),
            activity: s.activity.clone(),
            seen_count: s.seen_count,
            read_indices: s.read_indices,
            snoozed_until: s.snoozed_until,
            state: match s.state {
                crate::session::SessionState::Watching => WorkspaceState::Pending,
                crate::session::SessionState::CheckingOut => WorkspaceState::CheckingOut,
                crate::session::SessionState::Active | crate::session::SessionState::Working => {
                    WorkspaceState::Active
                }
                crate::session::SessionState::Archived => WorkspaceState::Archived,
            },
            created_at: s.created_at,
            last_viewed_at: s.last_viewed_at,
        };
        // Distribute primary_task + linked_items into the right slots.
        ws.attach_task(s.primary_task);
        for t in s.linked_items {
            ws.attach_task(t);
        }
        ws.sort_activity();
        // Migrate the v1 worktree_path into a freshly-allocated
        // session so v2 code that reads `workspace.sessions[..]`
        // sees the user's existing folder. v1 only ever held one
        // worktree per session, so a single Session covers it.
        if let Some(path) = s.worktree_path {
            ws.sessions.push(Session::new(
                key,
                SessionKind::Shell,
                path,
                ws.created_at,
            ));
        }
        ws
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskSlot {
    Pr,
    GhIssue,
    LinearIssue,
    Unknown,
}

fn classify(task: &Task) -> TaskSlot {
    if task.id.source == "linear" {
        return TaskSlot::LinearIssue;
    }
    if task.url.contains("/pull/") {
        return TaskSlot::Pr;
    }
    if task.url.contains("/issues/") || task.id.source == "github" {
        return TaskSlot::GhIssue;
    }
    TaskSlot::Unknown
}

fn upsert_by_id(list: &mut Vec<Task>, task: Task) {
    if let Some(slot) = list.iter_mut().find(|t| t.id == task.id) {
        *slot = task;
    } else {
        list.push(task);
    }
}

/// Stable per-task workspace key generator. PR `o/r#123` → "o/r-123".
/// Used so that "the workspace for this PR" resolves predictably even
/// before the user gives the workspace a custom name.
pub fn workspace_key_for(task: &Task) -> String {
    sanitize_key(&format!("{}-{}", task.id.source, task.id.key))
}

fn sanitize_key(raw: &str) -> String {
    raw.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '-',
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

// ─────────────────────────────────────────────────────────────────────
// Sessions
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionKind {
    /// `claude`, `codex`, `cursor`, etc. The agent registry resolves
    /// `agent_id` to argv at spawn time.
    Agent { agent_id: String },
    /// Plain login shell (bash/zsh).
    Shell,
    /// A view that compares the live output of two or more other
    /// sessions in the SAME workspace. Implemented as a real process
    /// the daemon spawns; survives restart like any other session.
    Compare { source_sessions: Vec<SessionId> },
    /// Tail a file (build log, test output) inside the worktree.
    LogTail { path: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRunState {
    /// Process is running and producing output.
    Active,
    /// Process exists but is idle (no recent output).
    Idle,
    /// Agent waiting on the user (Claude's "Are you sure?" prompts).
    Asking,
    /// Process exited.
    Stopped,
}

/// One running thing inside a workspace.
///
/// **A session IS a folder worktree.** It must point at a directory
/// on disk where its agent / shell / log-tail process runs. Without
/// a session there's no folder, so a workspace with `sessions = []`
/// is a pure tracking row with no on-disk presence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub workspace_key: WorkspaceKey,
    /// User-visible name. Defaults to the agent id ("claude") or
    /// "shell" / "compare" / "log: build.log".
    pub name: String,
    pub kind: SessionKind,
    pub state: SessionRunState,
    /// On-disk worktree this session lives in. Required: every
    /// session has a folder. Created lazily by the daemon's worktree
    /// manager when the session is first spawned and reused on
    /// subsequent agent runs in the same session.
    pub worktree_path: PathBuf,
    pub created_at: DateTime<Utc>,
    /// When the daemon last saw output from this session's PTY. None
    /// for compare/log sessions whose state model is different.
    #[serde(default)]
    pub last_output_at: Option<DateTime<Utc>>,
}

impl Session {
    pub fn new(
        workspace_key: WorkspaceKey,
        kind: SessionKind,
        worktree_path: PathBuf,
        now: DateTime<Utc>,
    ) -> Self {
        let name = default_name_for(&kind);
        Self {
            id: SessionId::new(),
            workspace_key,
            name,
            kind,
            state: SessionRunState::Active,
            worktree_path,
            created_at: now,
            last_output_at: None,
        }
    }
}

fn default_name_for(kind: &SessionKind) -> String {
    match kind {
        SessionKind::Agent { agent_id } => agent_id.clone(),
        SessionKind::Shell => "shell".into(),
        SessionKind::Compare { .. } => "compare".into(),
        SessionKind::LogTail { path } => format!("log: {path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{
        Activity, ActivityKind, CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState,
    };

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-04-28T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn pr(key: &str) -> Task {
        Task {
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: format!("PR {key}"),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Reviewer,
            ci: CiStatus::Success,
            review: ReviewStatus::Pending,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/{key}").replace('#', "/pull/"),
            repo: Some("o/r".into()),
            branch: Some("feature/x".into()),
            base_branch: Some("main".into()),
            updated_at: now(),
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
        }
    }

    fn issue(source: &str, key: &str) -> Task {
        let mut t = pr(key);
        t.id.source = source.into();
        t.url = if source == "linear" {
            format!("https://linear.app/team/issue/{key}")
        } else {
            format!("https://github.com/{key}").replace("/pull/", "/issues/")
        };
        t
    }

    fn activity_at(seconds: i64, body: &str) -> Activity {
        Activity {
            author: "alice".into(),
            body: body.into(),
            created_at: now() + chrono::Duration::seconds(seconds),
            kind: ActivityKind::Comment,
            node_id: None,
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        }
    }

    #[test]
    fn from_task_makes_pr_workspace() {
        let ws = Workspace::from_task(pr("o/r#1"), now());
        assert!(ws.pr.is_some());
        assert!(ws.gh_issues.is_empty());
        assert!(ws.linear_issues.is_empty());
    }

    #[test]
    fn from_task_makes_issue_workspace() {
        let ws = Workspace::from_task(issue("github", "o/r#42"), now());
        assert!(ws.pr.is_none());
        assert_eq!(ws.gh_issues.len(), 1);
    }

    #[test]
    fn attach_pr_replaces_existing_pr() {
        let mut ws = Workspace::from_task(pr("o/r#1"), now());
        ws.attach_task(pr("o/r#2"));
        assert_eq!(
            ws.pr.as_ref().unwrap().id.key,
            "o/r#2",
            "second PR replaces first"
        );
    }

    #[test]
    fn attach_routes_each_task_to_its_slot() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.attach_task(pr("o/r#1"));
        ws.attach_task(issue("github", "o/r#42"));
        ws.attach_task(issue("linear", "ENG-7"));
        assert!(ws.pr.is_some());
        assert_eq!(ws.gh_issues.len(), 1);
        assert_eq!(ws.linear_issues.len(), 1);
    }

    #[test]
    fn attaching_same_issue_twice_dedupes_by_id() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.attach_task(issue("github", "o/r#42"));
        ws.attach_task(issue("github", "o/r#42"));
        assert_eq!(
            ws.gh_issues.len(),
            1,
            "duplicate attaches replace, not append"
        );
    }

    #[test]
    fn detach_removes_from_any_slot() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.attach_task(pr("o/r#1"));
        ws.attach_task(issue("linear", "ENG-7"));
        let pr_id = TaskId {
            source: "github".into(),
            key: "o/r#1".into(),
        };
        let lin_id = TaskId {
            source: "linear".into(),
            key: "ENG-7".into(),
        };
        ws.detach_task(&pr_id);
        ws.detach_task(&lin_id);
        assert!(ws.pr.is_none());
        assert!(ws.linear_issues.is_empty());
    }

    #[test]
    fn merge_activity_dedupes_and_sorts_newest_first() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.merge_activity(&[activity_at(10, "second"), activity_at(0, "first")]);
        ws.merge_activity(&[activity_at(0, "first"), activity_at(20, "third")]);
        assert_eq!(ws.activity.len(), 3);
        assert_eq!(ws.activity[0].body, "third");
        assert_eq!(ws.activity[1].body, "second");
        assert_eq!(ws.activity[2].body, "first");
    }

    #[test]
    fn linked_task_ids_reports_every_attached_task() {
        let mut ws = Workspace::empty(WorkspaceKey::new("ws-1"), "main", now());
        ws.attach_task(pr("o/r#1"));
        ws.attach_task(issue("github", "o/r#42"));
        ws.attach_task(issue("linear", "ENG-7"));
        let ids = ws.linked_task_ids();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn workspace_key_for_a_pr_is_stable_and_filesystem_safe() {
        let task = pr("owner/repo#123");
        let key = workspace_key_for(&task);
        assert!(!key.contains('#'));
        assert!(!key.contains('/'));
        // Same task → same key.
        assert_eq!(workspace_key_for(&task), key);
    }

    #[test]
    fn from_v1_session_preserves_read_state_and_attaches_primary_task() {
        let task = pr("o/r#1");
        let mut s = crate::session::Session::new_at(task, now());
        s.seen_count = 7;
        s.last_viewed_at = Some(now());
        let ws = Workspace::from_v1_session(s);
        assert!(ws.pr.is_some());
        assert_eq!(ws.seen_count, 7);
        assert!(ws.last_viewed_at.is_some());
    }

    #[test]
    fn session_id_is_unique_per_call() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn session_default_name_matches_kind() {
        assert_eq!(
            default_name_for(&SessionKind::Agent {
                agent_id: "claude".into()
            }),
            "claude"
        );
        assert_eq!(default_name_for(&SessionKind::Shell), "shell");
        assert_eq!(
            default_name_for(&SessionKind::Compare {
                source_sessions: vec![]
            }),
            "compare"
        );
        assert_eq!(
            default_name_for(&SessionKind::LogTail {
                path: "build.log".into()
            }),
            "log: build.log"
        );
    }
}
