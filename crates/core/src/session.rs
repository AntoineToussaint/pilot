use crate::task::{ActionPriority, Activity, Task, TaskId};
use crate::time;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

/// Color assigned to a session for visual identification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionColor {
    Blue,
    Green,
    Yellow,
    Red,
    Magenta,
    Cyan,
    Orange,
    Purple,
}

impl SessionColor {
    /// Rotate through colors based on an index.
    pub fn from_index(i: usize) -> Self {
        const PALETTE: &[SessionColor] = &[
            SessionColor::Blue,
            SessionColor::Green,
            SessionColor::Yellow,
            SessionColor::Magenta,
            SessionColor::Cyan,
            SessionColor::Orange,
            SessionColor::Purple,
            SessionColor::Red,
        ];
        PALETTE[i % PALETTE.len()]
    }

    /// Unicode colored dot for display.
    pub fn dot(&self) -> &'static str {
        match self {
            Self::Blue => "🔵",
            Self::Green => "🟢",
            Self::Yellow => "🟡",
            Self::Red => "🔴",
            Self::Magenta => "🟣",
            Self::Cyan => "🔵",
            Self::Orange => "🟠",
            Self::Purple => "🟣",
        }
    }
}

/// The type of an item attached to a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ItemKind {
    PullRequest,
    Issue,
    LinearTicket,
    Discussion,
    Other,
}

/// A workspace/session. Groups related items (PRs, issues, tickets) under
/// one logical unit with a shared worktree, terminal, and activity feed.
///
/// Sidebar structure: Repo → Session → Items + Messages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Unique session id (from the primary task).
    pub task_id: TaskId,
    /// Display name (e.g. "Fix migration bug").
    pub display_name: String,
    /// The primary task (the one that created this session).
    pub primary_task: Task,
    /// Additional linked items (related issues, tickets, etc.).
    pub linked_items: Vec<Task>,
    /// Repository this session belongs to (for grouping).
    pub repo: String,
    /// Color for visual identification.
    pub color: SessionColor,
    /// Item kind for the primary task.
    pub kind: ItemKind,
    /// Path to the git worktree, if checked out.
    pub worktree_path: Option<PathBuf>,
    /// Session state.
    pub state: SessionState,
    /// If set, this session is being automatically monitored.
    #[serde(default)]
    pub monitor: Option<MonitorState>,
    /// Whether Claude Code has been opened in this session (for --continue on reopen).
    #[serde(default)]
    pub had_claude: bool,
    /// When this session was created.
    pub created_at: DateTime<Utc>,
    /// Activity items (comments, reviews, CI updates) — newest first.
    pub activity: Vec<Activity>,
    /// How many activity items have been seen (read).
    pub seen_count: usize,
    /// Set of activity indices that have been individually read.
    /// Used in addition to seen_count for fine-grained tracking.
    #[serde(default)]
    pub read_indices: HashSet<usize>,
    /// If set, this session is snoozed until this time.
    #[serde(default)]
    pub snoozed_until: Option<DateTime<Utc>>,
    /// Timestamp of the last time the user viewed this session.
    pub last_viewed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    Watching,
    CheckingOut,
    Active,
    Working,
    Archived,
}

/// State machine for automatic PR monitoring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MonitorState {
    /// Watching for problems. No action in progress.
    Idle,
    /// CI failed, Claude is working on a fix.
    CiFixing { attempt: u32 },
    /// Merge conflict detected, running rebase.
    Rebasing,
    /// Pushed a fix or rebased, waiting for CI to report back.
    WaitingCi { after_attempt: u32 },
    /// Gave up after too many retries.
    Failed { reason: String },
}

static COLOR_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Session {
    /// Construct a fresh session from a task at a specific instant.
    /// Prefer this so callers (reducer, tests) pin creation time.
    pub fn new_at(task: Task, now: DateTime<Utc>) -> Self {
        let display_name = task.title.clone();
        let task_id = task.id.clone();
        let repo = task.repo.clone().unwrap_or_else(|| "unknown".to_string());
        let kind = if task.url.contains("/pull/") {
            ItemKind::PullRequest
        } else if task.url.contains("/issues/") {
            ItemKind::Issue
        } else {
            ItemKind::Other
        };
        let color_idx = COLOR_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        Self {
            task_id,
            display_name,
            primary_task: task,
            linked_items: Vec::new(),
            repo,
            color: SessionColor::from_index(color_idx),
            kind,
            worktree_path: None,
            state: SessionState::Watching,
            monitor: None,
            had_claude: false,
            created_at: now,
            activity: Vec::new(),
            seen_count: 0,
            read_indices: HashSet::new(),
            snoozed_until: None,
            last_viewed_at: None,
        }
    }


    /// Number of unread activity items.
    pub fn unread_count(&self) -> usize {
        (0..self.activity.len().saturating_sub(self.seen_count))
            .filter(|i| !self.read_indices.contains(i))
            .count()
    }

    /// Mark all current activity as read. `now` stamps `last_viewed_at`.
    pub fn mark_read(&mut self, now: DateTime<Utc>) {
        self.seen_count = self.activity.len();
        self.read_indices.clear();
        self.last_viewed_at = Some(now);
    }

    /// Push a new activity item (inserted at front = newest first).
    ///
    /// Shifts `read_indices` by +1 because every existing item moves one slot
    /// back. Without this shift, previously-read items would appear unread
    /// (and vice versa) after new activity arrives — since `read_indices`
    /// stores positions, not item identity.
    pub fn push_activity(&mut self, activity: Activity) {
        if !self.read_indices.is_empty() {
            self.read_indices = self.read_indices.iter().map(|i| i + 1).collect();
        }
        self.activity.insert(0, activity);
    }

    /// Get unread activity items.
    pub fn unread_activity(&self) -> &[Activity] {
        if self.activity.len() > self.seen_count {
            &self.activity[..self.activity.len() - self.seen_count]
        } else {
            &[]
        }
    }

    /// Mark a specific activity index as individually read.
    pub fn mark_activity_read(&mut self, index: usize) {
        self.read_indices.insert(index);
    }

    /// Check if a specific activity is unread.
    pub fn is_activity_unread(&self, index: usize) -> bool {
        index < self.activity.len().saturating_sub(self.seen_count)
            && !self.read_indices.contains(&index)
    }

    /// Whether this session is currently snoozed at `now`.
    pub fn is_snoozed(&self, now: DateTime<Utc>) -> bool {
        self.snoozed_until.is_some_and(|t| now < t)
    }

    /// Link an additional item to this session.
    pub fn link_item(&mut self, task: Task) {
        if !self.linked_items.iter().any(|t| t.id == task.id) {
            self.linked_items.push(task);
        }
    }

    /// All items (primary + linked).
    pub fn all_items(&self) -> impl Iterator<Item = &Task> {
        std::iter::once(&self.primary_task).chain(self.linked_items.iter())
    }

    /// Best priority across all items.
    pub fn action_priority(&self, my_username: &str, now: DateTime<Utc>) -> ActionPriority {
        let task = &self.primary_task;
        let _ = my_username;

        // Approved + CI green = ready to merge (highest positive signal, check first).
        if task.review == crate::ReviewStatus::Approved && task.role == crate::TaskRole::Author {
            return ActionPriority::ApprovedReadyToMerge;
        }
        // CI failed on your PR — you're blocking yourself.
        if task.ci == crate::CiStatus::Failure && task.role == crate::TaskRole::Author {
            return ActionPriority::CiFailed;
        }
        // Changes requested — reviewer wants changes.
        if task.review == crate::ReviewStatus::ChangesRequested
            && task.role == crate::TaskRole::Author
        {
            return ActionPriority::ChangesRequested;
        }
        // Someone commented and you haven't responded.
        if task.needs_reply && task.role == crate::TaskRole::Author {
            return ActionPriority::NeedsReply;
        }
        // You're a reviewer — you're blocking someone.
        if task.role == crate::TaskRole::Reviewer {
            return ActionPriority::NeedsYourReview;
        }
        if self.unread_count() > 0 {
            return ActionPriority::NewActivity;
        }
        if let time::Staleness::Stale { .. } | time::Staleness::Abandoned { .. } =
            time::staleness(&task.updated_at, &task.updated_at, now)
        {
            return ActionPriority::Stale;
        }
        ActionPriority::WaitingOnOthers
    }

    /// Whether this session is actively monitored (not failed/stopped).
    pub fn is_monitored(&self) -> bool {
        matches!(
            self.monitor,
            Some(MonitorState::Idle)
                | Some(MonitorState::CiFixing { .. })
                | Some(MonitorState::Rebasing)
                | Some(MonitorState::WaitingCi { .. })
        )
    }

    /// Short label for the current monitor state.
    pub fn monitor_label(&self) -> Option<&'static str> {
        match &self.monitor {
            Some(MonitorState::Idle) => Some("watching"),
            Some(MonitorState::CiFixing { .. }) => Some("fixing CI"),
            Some(MonitorState::Rebasing) => Some("rebasing"),
            Some(MonitorState::WaitingCi { .. }) => Some("waiting CI"),
            Some(MonitorState::Failed { .. }) => Some("failed"),
            None => None,
        }
    }

    /// Short kind label.
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            ItemKind::PullRequest => "PR",
            ItemKind::Issue => "Issue",
            ItemKind::LinearTicket => "Linear",
            ItemKind::Discussion => "Discussion",
            ItemKind::Other => "Item",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::*;
    use chrono::Utc;

    fn make_task(title: &str) -> Task {
        Task {
            id: TaskId { source: "test".into(), key: format!("test/repo#{title}") },
            title: title.into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "https://github.com/test/repo/pull/1".into(),
            repo: Some("test/repo".into()),
            branch: Some("feature".into()),
            base_branch: None,
            updated_at: Utc::now(),
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            has_conflicts: false,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
        }
    }

    fn make_activity(author: &str, body: &str) -> Activity {
        Activity {
            author: author.into(),
            body: body.into(),
            created_at: Utc::now(),
            kind: ActivityKind::Comment,
            node_id: None,
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        }
    }

    #[test]
    fn test_session_defaults() {
        let s = Session::new_at(make_task("Fix bug"), chrono::Utc::now());
        assert_eq!(s.display_name, "Fix bug");
        assert_eq!(s.repo, "test/repo");
        assert_eq!(s.kind, ItemKind::PullRequest);
        assert_eq!(s.state, SessionState::Watching);
        assert!(s.monitor.is_none());
        assert!(!s.had_claude);
        assert_eq!(s.unread_count(), 0);
    }

    #[test]
    fn test_activity_and_unread() {
        let mut s = Session::new_at(make_task("PR"), chrono::Utc::now());
        s.push_activity(make_activity("alice", "looks good"));
        assert_eq!(s.unread_count(), 1);

        s.push_activity(make_activity("bob", "needs changes"));
        assert_eq!(s.unread_count(), 2);

        s.mark_read(chrono::Utc::now());
        assert_eq!(s.unread_count(), 0);

        s.push_activity(make_activity("carol", "fixed"));
        assert_eq!(s.unread_count(), 1);
    }

    #[test]
    fn test_unread_activity_slice() {
        let mut s = Session::new_at(make_task("PR"), chrono::Utc::now());
        s.push_activity(make_activity("a", "1"));
        s.push_activity(make_activity("b", "2"));
        s.mark_read(chrono::Utc::now());
        s.push_activity(make_activity("c", "3"));

        let unread = s.unread_activity();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].author, "c");
    }

    #[test]
    fn test_monitor_states() {
        let mut s = Session::new_at(make_task("PR"), chrono::Utc::now());
        assert!(!s.is_monitored());
        assert!(s.monitor_label().is_none());

        s.monitor = Some(MonitorState::Idle);
        assert!(s.is_monitored());
        assert_eq!(s.monitor_label(), Some("watching"));

        s.monitor = Some(MonitorState::CiFixing { attempt: 2 });
        assert!(s.is_monitored());
        assert_eq!(s.monitor_label(), Some("fixing CI"));

        s.monitor = Some(MonitorState::Failed { reason: "too many".into() });
        assert!(!s.is_monitored());
        assert_eq!(s.monitor_label(), Some("failed"));
    }

    #[test]
    fn test_priority_ci_failed() {
        let mut t = make_task("PR");
        t.ci = CiStatus::Failure;
        let s = Session::new_at(t, chrono::Utc::now());
        assert_eq!(s.action_priority("me", chrono::Utc::now()), ActionPriority::CiFailed);
    }

    #[test]
    fn test_priority_approved() {
        let mut t = make_task("PR");
        t.review = ReviewStatus::Approved;
        let s = Session::new_at(t, chrono::Utc::now());
        assert_eq!(s.action_priority("me", chrono::Utc::now()), ActionPriority::ApprovedReadyToMerge);
    }

    #[test]
    fn test_priority_needs_review() {
        let mut t = make_task("PR");
        t.role = TaskRole::Reviewer;
        let s = Session::new_at(t, chrono::Utc::now());
        assert_eq!(s.action_priority("me", chrono::Utc::now()), ActionPriority::NeedsYourReview);
    }

    #[test]
    fn test_read_indices_shift_on_push() {
        // Regression: read_indices stored positions, not identities — so
        // inserting a new activity at the front without shifting read_indices
        // would mark the wrong items as read.
        let mut s = Session::new_at(make_task("PR"), chrono::Utc::now());
        s.push_activity(make_activity("a", "oldest"));  // index 0
        s.push_activity(make_activity("b", "middle"));  // b=0, a=1
        s.push_activity(make_activity("c", "newest"));  // c=0, b=1, a=2

        // Mark the middle item (b, currently at index 1) as read.
        s.mark_activity_read(1);
        assert!(s.is_activity_unread(0));   // c unread
        assert!(!s.is_activity_unread(1));  // b read
        assert!(s.is_activity_unread(2));   // a unread

        // New activity arrives — everything shifts. b must still be marked read.
        s.push_activity(make_activity("d", "brand new"));
        // Now indices: d=0, c=1, b=2, a=3
        assert!(s.is_activity_unread(0));   // d unread
        assert!(s.is_activity_unread(1));   // c unread
        assert!(!s.is_activity_unread(2));  // b still read (shifted from 1 to 2)
        assert!(s.is_activity_unread(3));   // a unread

        // And unread_count should be 3 (d, c, a).
        assert_eq!(s.unread_count(), 3);
    }

    #[test]
    fn test_kind_detection() {
        let mut t = make_task("issue");
        t.url = "https://github.com/a/b/issues/1".into();
        let s = Session::new_at(t, chrono::Utc::now());
        assert_eq!(s.kind, ItemKind::Issue);
    }
}
