//! `Workspace` and `Session` — pilot's hierarchy.
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
//! Separating "the unit of work" (Workspace) from "the running thing"
//! (Session) lets a single workspace host parallel agents and shells —
//! e.g. Claude AND Codex on the same PR, or a long-running shell next
//! to an agent. Both persist across pilot restarts: the daemon keeps
//! the worktree and the PTYs alive, the store remembers everything else.

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

/// One workspace = one unit of work (PR + linked issues), holding
/// **zero or more sessions**. A session is one folder worktree on
/// disk; without sessions the workspace is purely a tracking row
/// with no on-disk presence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub key: WorkspaceKey,
    /// Display name. Defaults to the PR title or the first issue's
    /// title when first created; user can rename.
    pub name: String,
    /// Branch this workspace tracks. Required — even a "from scratch"
    /// workspace lives on a branch.
    pub branch: String,
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
            sessions: Vec::new(),
            pr: None,
            gh_issues: Vec::new(),
            linear_issues: Vec::new(),
            activity: Vec::new(),
            seen_count: 0,
            read_indices: HashSet::new(),
            snoozed_until: None,
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

    /// Mark exactly one activity as read. Used by the auto-mark-on-
    /// hover feature — landing the cursor on an unread row arms a
    /// short timer; on expiry the App calls this. Idempotent: marking
    /// an already-read index is a no-op.
    pub fn mark_activity_read(&mut self, index: usize) {
        if index < self.activity.len() {
            self.read_indices.insert(index);
        }
    }

    /// Reverse of `mark_activity_read`. Bound to the `z` undo key —
    /// pulls the index back into the unread set without disturbing
    /// other read state. No-op if the index wasn't in the set.
    pub fn unmark_activity_read(&mut self, index: usize) {
        self.read_indices.remove(&index);
        // Also reduce seen_count if this index was inside the auto-
        // seen tail (`activity.len() - seen_count`). Without this, an
        // undo immediately after a snapshot-driven seen bump would
        // not restore the unread state.
        let auto_seen_threshold = self.activity.len().saturating_sub(self.seen_count);
        if index >= auto_seen_threshold {
            self.seen_count = self.activity.len().saturating_sub(index + 1);
        }
    }

    pub fn is_snoozed(&self, now: DateTime<Utc>) -> bool {
        match self.snoozed_until {
            Some(until) => until > now,
            None => false,
        }
    }

    /// On-disk identifier for this workspace's worktrees. Human-
    /// readable so a shell prompt sitting in the worktree is
    /// instantly recognisable.
    ///
    /// Resolution order:
    /// - PR attached → `PR-{number}-{slug-of-title}` (capped at 8
    ///   words so it stays scannable).
    /// - No PR but a custom workspace name → slug of `name`.
    /// - Both empty → fall back to a stable `workspace_{key-suffix}`
    ///   placeholder so the path is always non-empty.
    pub fn worktree_slug(&self) -> String {
        if let Some(pr) = self.pr.as_ref()
            && let Some((_, num_str)) = pr.id.key.rsplit_once('#')
            && let Ok(num) = num_str.parse::<u64>()
        {
            return crate::slug::pr_slug(num, &pr.title);
        }
        let name_slug = crate::slug::slugify(&self.name);
        if !name_slug.is_empty() {
            return name_slug;
        }
        // Fall-back: avoid empty paths even on a fully-anonymous
        // workspace. The key's tail keeps it unique across siblings.
        let suffix = self
            .key
            .as_str()
            .chars()
            .rev()
            .take(8)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        format!("workspace-{suffix}")
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

/// How runners are arranged inside a session's surface area.
///
/// Default `Tabs` is what shipped first: one runner full-pane with a
/// tab strip on top, switch with the next-tab key. `Splits` is the
/// tile-manager variant: a tree of horizontal/vertical splits with
/// runners at the leaves, mirroring tmux panes.
///
/// The `Splits` variant is wired through persistence + IPC but the
/// renderer + key bindings still default to `Tabs`. Migration path:
/// the App reads `Session.layout`, picks Tabs rendering until the
/// tile renderer is wired, at which point the same data model works
/// without a schema change.
// External tagging (the serde default) is what bincode supports —
// internally-tagged enums fail `bincode::deserialize` because the
// format isn't self-describing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SessionLayout {
    Tabs {
        /// Index into `Session.runners`. Clamped on save.
        active: usize,
    },
    Splits {
        tree: TileTree,
        /// Path through `tree` to the focused leaf (0 = first child
        /// at each level, 1 = second). Empty when the tree is just
        /// a leaf.
        focused: Vec<u8>,
    },
}

impl Default for SessionLayout {
    fn default() -> Self {
        Self::Tabs { active: 0 }
    }
}

/// One node in the per-session tile tree. Leaves point to a runner
/// by terminal id (numeric, daemon-allocated). Splits hold a 0-100
/// `ratio` for the first child's share of the available space.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TileTree {
    Leaf {
        terminal_id: u64,
    },
    HSplit {
        left: Box<TileTree>,
        right: Box<TileTree>,
        ratio: u8,
    },
    VSplit {
        top: Box<TileTree>,
        bottom: Box<TileTree>,
        ratio: u8,
    },
}

/// Direction for spatial navigation between tiles. Maps onto vim
/// `Ctrl-w h/j/k/l`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileDirection {
    Left,
    Right,
    Up,
    Down,
}

impl TileTree {
    /// Every leaf's terminal id, in pre-order. Stable ordering — the
    /// renderer relies on this for the focused-tile highlight.
    pub fn leaves(&self) -> Vec<u64> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<u64>) {
        match self {
            TileTree::Leaf { terminal_id } => out.push(*terminal_id),
            TileTree::HSplit { left, right, .. } => {
                left.collect_leaves(out);
                right.collect_leaves(out);
            }
            TileTree::VSplit { top, bottom, .. } => {
                top.collect_leaves(out);
                bottom.collect_leaves(out);
            }
        }
    }

    /// Path through the tree to the leaf carrying `terminal_id`.
    /// Returns the steps as 0/1 (left-or-top vs. right-or-bottom).
    pub fn path_to(&self, terminal_id: u64) -> Option<Vec<u8>> {
        let mut path = Vec::new();
        if self.find_path(terminal_id, &mut path) {
            Some(path)
        } else {
            None
        }
    }

    fn find_path(&self, terminal_id: u64, path: &mut Vec<u8>) -> bool {
        match self {
            TileTree::Leaf { terminal_id: id } => *id == terminal_id,
            TileTree::HSplit { left, right, .. } | TileTree::VSplit { top: left, bottom: right, .. } => {
                path.push(0);
                if left.find_path(terminal_id, path) {
                    return true;
                }
                path.pop();
                path.push(1);
                if right.find_path(terminal_id, path) {
                    return true;
                }
                path.pop();
                false
            }
        }
    }

    /// Replace the leaf at `path` with `new` and return the previous
    /// subtree there. Used by split operations: take the focused
    /// leaf, wrap it in a Split with a new sibling.
    pub fn replace_at(&mut self, path: &[u8], new: TileTree) -> Option<TileTree> {
        if path.is_empty() {
            return Some(std::mem::replace(self, new));
        }
        let head = path[0];
        let rest = &path[1..];
        let next = match self {
            TileTree::HSplit { left, right, .. } | TileTree::VSplit { top: left, bottom: right, .. } => {
                if head == 0 { left.as_mut() } else { right.as_mut() }
            }
            TileTree::Leaf { .. } => return None,
        };
        next.replace_at(rest, new)
    }

    /// Remove the leaf at `path`, collapsing its parent split into
    /// the surviving sibling. Returns Ok with the new path of focus
    /// (the sibling's path) on success. Errors when the path points
    /// at the root (can't collapse the only tile) or doesn't exist.
    pub fn remove_at(&mut self, path: &[u8]) -> Result<Vec<u8>, ()> {
        if path.is_empty() {
            // Caller is trying to delete the only tile. Refuse — the
            // session needs at least one runner visible.
            return Err(());
        }
        if path.len() == 1 {
            // Collapse the parent (which is `self`) into the sibling.
            let head = path[0];
            let new_root = match self {
                TileTree::HSplit { left, right, .. } | TileTree::VSplit { top: left, bottom: right, .. } => {
                    if head == 0 {
                        std::mem::replace(right.as_mut(), TileTree::Leaf { terminal_id: 0 })
                    } else {
                        std::mem::replace(left.as_mut(), TileTree::Leaf { terminal_id: 0 })
                    }
                }
                TileTree::Leaf { .. } => return Err(()),
            };
            *self = new_root;
            // After collapse, focus lands at the new root (no path).
            return Ok(Vec::new());
        }
        let head = path[0];
        let rest = &path[1..];
        let next = match self {
            TileTree::HSplit { left, right, .. } | TileTree::VSplit { top: left, bottom: right, .. } => {
                if head == 0 { left.as_mut() } else { right.as_mut() }
            }
            TileTree::Leaf { .. } => return Err(()),
        };
        let mut sub_path = next.remove_at(rest)?;
        // Prefix the parent step so the returned focus path is full.
        sub_path.insert(0, head);
        Ok(sub_path)
    }

    /// Spatial neighbor of the leaf at `path` in the given direction.
    /// Returns the path to that neighbor leaf, or None if nothing
    /// lies in that direction (e.g. moving Left from the leftmost
    /// tile). Walks up to find an ancestor split that goes against
    /// the requested axis, then descends.
    pub fn neighbor(&self, path: &[u8], dir: TileDirection) -> Option<Vec<u8>> {
        // Walk up from the leaf until we find a split whose axis
        // matches `dir` AND we came from the "wrong" side (so we can
        // jump to the other side).
        let want_horizontal = matches!(dir, TileDirection::Left | TileDirection::Right);
        let want_first = matches!(dir, TileDirection::Left | TileDirection::Up);
        for i in (0..path.len()).rev() {
            let prefix = &path[..i];
            let step = path[i];
            let node = self.subtree_at(prefix)?;
            let split_is_horizontal = matches!(node, TileTree::HSplit { .. });
            if split_is_horizontal != want_horizontal {
                continue;
            }
            // We're inside a split whose axis matches the request.
            // Did we come from the "near" side (so `dir` would jump
            // us across), or from the "far" side (no neighbor here,
            // keep walking)?
            let came_from_near = (step == 1) == want_first;
            if !came_from_near {
                continue;
            }
            let mut new_path = prefix.to_vec();
            new_path.push(if want_first { 0 } else { 1 });
            // Descend into the chosen side's deepest leaf along the
            // SAME axis (so the cursor lands on a visible leaf).
            return Some(self.descend_to_leaf(&mut new_path));
        }
        None
    }

    fn subtree_at(&self, path: &[u8]) -> Option<&TileTree> {
        let mut node = self;
        for &step in path {
            node = match node {
                TileTree::HSplit { left, right, .. } | TileTree::VSplit { top: left, bottom: right, .. } => {
                    if step == 0 { left.as_ref() } else { right.as_ref() }
                }
                TileTree::Leaf { .. } => return None,
            };
        }
        Some(node)
    }

    /// From the subtree at `path`, walk down to its first leaf along
    /// the natural pre-order traversal. Mutates `path` in place,
    /// extending it. Returns the extended path.
    fn descend_to_leaf(&self, path: &mut Vec<u8>) -> Vec<u8> {
        let mut node = self.subtree_at(path);
        while let Some(n) = node {
            match n {
                TileTree::Leaf { .. } => break,
                TileTree::HSplit { .. } | TileTree::VSplit { .. } => {
                    path.push(0);
                    node = self.subtree_at(path);
                }
            }
        }
        path.clone()
    }
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
    /// Tile/tab arrangement for this session. Defaults to Tabs.
    /// Persisted so the user's layout survives restart.
    #[serde(default)]
    pub layout: SessionLayout,
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
            layout: SessionLayout::default(),
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
mod tile_tree_tests {
    use super::*;

    fn leaf(id: u64) -> TileTree {
        TileTree::Leaf { terminal_id: id }
    }
    fn hsplit(left: TileTree, right: TileTree) -> TileTree {
        TileTree::HSplit {
            left: Box::new(left),
            right: Box::new(right),
            ratio: 50,
        }
    }
    fn vsplit(top: TileTree, bottom: TileTree) -> TileTree {
        TileTree::VSplit {
            top: Box::new(top),
            bottom: Box::new(bottom),
            ratio: 50,
        }
    }

    #[test]
    fn leaves_traverses_in_preorder() {
        // Tree: H(L=1, V(T=2, B=3))
        let t = hsplit(leaf(1), vsplit(leaf(2), leaf(3)));
        assert_eq!(t.leaves(), vec![1, 2, 3]);
    }

    #[test]
    fn path_to_finds_each_leaf() {
        let t = hsplit(leaf(1), vsplit(leaf(2), leaf(3)));
        assert_eq!(t.path_to(1), Some(vec![0]));
        assert_eq!(t.path_to(2), Some(vec![1, 0]));
        assert_eq!(t.path_to(3), Some(vec![1, 1]));
        assert_eq!(t.path_to(99), None);
    }

    #[test]
    fn replace_at_swaps_leaf_for_split() {
        let mut t = leaf(1);
        // Wrap leaf 1 in HSplit(1, 2).
        let prev = t.replace_at(&[], hsplit(leaf(1), leaf(2))).unwrap();
        assert_eq!(prev, leaf(1));
        assert_eq!(t.leaves(), vec![1, 2]);
    }

    #[test]
    fn remove_at_collapses_parent_split() {
        let mut t = hsplit(leaf(1), vsplit(leaf(2), leaf(3)));
        // Remove leaf 2 — VSplit collapses to leaf 3.
        let new_focus = t.remove_at(&[1, 0]).unwrap();
        assert_eq!(t.leaves(), vec![1, 3]);
        assert_eq!(new_focus, vec![1]);
    }

    #[test]
    fn remove_at_root_path_errors() {
        let mut t = leaf(1);
        assert!(t.remove_at(&[]).is_err(), "can't remove the only tile");
    }

    #[test]
    fn neighbor_right_jumps_to_sibling() {
        // H(1, 2): from 1, Right → 2.
        let t = hsplit(leaf(1), leaf(2));
        let path1 = t.path_to(1).unwrap();
        let n = t.neighbor(&path1, TileDirection::Right);
        assert_eq!(n, Some(vec![1]));
    }

    #[test]
    fn neighbor_left_at_leftmost_returns_none() {
        let t = hsplit(leaf(1), leaf(2));
        let path1 = t.path_to(1).unwrap();
        assert_eq!(t.neighbor(&path1, TileDirection::Left), None);
    }

    #[test]
    fn neighbor_up_in_vsplit() {
        // V(1, 2): from 2, Up → 1.
        let t = vsplit(leaf(1), leaf(2));
        let path2 = t.path_to(2).unwrap();
        assert_eq!(t.neighbor(&path2, TileDirection::Up), Some(vec![0]));
    }

    #[test]
    fn neighbor_walks_up_through_unrelated_split() {
        // H(1, V(2, 3)): from 1, Right should land on the deepest
        // first-leaf of the right subtree (= 2).
        let t = hsplit(leaf(1), vsplit(leaf(2), leaf(3)));
        let path1 = t.path_to(1).unwrap();
        let n = t.neighbor(&path1, TileDirection::Right);
        assert_eq!(n, Some(vec![1, 0]));
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
