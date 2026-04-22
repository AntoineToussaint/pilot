use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A unique identifier for a task, scoped by source.
/// e.g. ("github", "owner/repo#123") or ("linear", "ENG-456")
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId {
    /// Which provider created this task (e.g. "github", "linear").
    pub source: String,
    /// Provider-specific unique key.
    pub key: String,
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.source, self.key)
    }
}

/// Why this task is on your radar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskRole {
    /// You authored it (your PR, your issue).
    Author,
    /// You're assigned as a reviewer.
    Reviewer,
    /// You're assigned to work on it.
    Assignee,
    /// You're mentioned or subscribed.
    Mentioned,
}

/// The lifecycle state of a task (source-agnostic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskState {
    Open,
    InProgress,
    InReview,
    Closed,
    Merged,
    Draft,
}

/// CI / build status rolled up from individual checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum CiStatus {
    #[default]
    None,
    Pending,
    Running,
    Success,
    Failure,
    Mixed,
}

/// Review status rolled up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ReviewStatus {
    #[default]
    None,
    Pending,
    Approved,
    ChangesRequested,
}

/// An individual CI check run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRun {
    pub name: String,
    pub status: CiStatus,
    pub url: Option<String>,
}

/// A comment or activity entry on a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Activity {
    pub author: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub kind: ActivityKind,
    /// GitHub node ID for replying to this comment.
    #[serde(default)]
    pub node_id: Option<String>,
    /// File path for inline review comments (e.g. "src/foo.rs").
    #[serde(default)]
    pub path: Option<String>,
    /// Line number for inline review comments.
    #[serde(default)]
    pub line: Option<u32>,
    /// Diff hunk showing the code context for inline review comments.
    #[serde(default)]
    pub diff_hunk: Option<String>,
    /// GraphQL node ID of the review *thread* (used for threaded replies
    /// via `addPullRequestReviewThreadReply` and for `resolveReviewThread`).
    #[serde(default)]
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityKind {
    Comment,
    Review,
    StatusChange,
    CiUpdate,
}

/// A source-agnostic task descriptor. Providers convert their domain objects
/// into this type. The TUI and session system only work with `Task`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub body: Option<String>,
    pub state: TaskState,
    pub role: TaskRole,
    pub ci: CiStatus,
    pub review: ReviewStatus,
    pub checks: Vec<CheckRun>,
    pub unread_count: u32,
    pub url: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    /// Target branch of the PR — usually the repo default (main / master),
    /// but for stacked PRs it's the head branch of another open PR.
    #[serde(default)]
    pub base_branch: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub labels: Vec<String>,
    /// Requested reviewers (user logins or team names).
    #[serde(default)]
    pub reviewers: Vec<String>,
    /// Assignees (user logins).
    #[serde(default)]
    pub assignees: Vec<String>,
    /// Auto-merge is enabled ("merge when ready" armed by someone).
    /// NOTE: this does NOT mean the PR is approved or that anything will
    /// merge right now — only that it will merge once CI + reviews pass.
    #[serde(default, alias = "in_merge_queue")]
    pub auto_merge_enabled: bool,
    /// The PR is actually sitting in GitHub's merge queue right now
    /// (separate from auto-merge; this is the real queued-to-merge state).
    #[serde(default)]
    pub is_in_merge_queue: bool,
    /// Whether this PR has merge conflicts.
    #[serde(default)]
    pub has_conflicts: bool,
    /// Whether the PR branch is behind its base (needs "Update branch").
    /// Derived from GitHub's `mergeStateStatus == BEHIND`.
    #[serde(default)]
    pub is_behind_base: bool,
    /// GraphQL node ID of the PR — required for mutations like
    /// `updatePullRequestBranch`. Populated by providers that fetch it.
    #[serde(default)]
    pub node_id: Option<String>,
    /// Whether the last comment/review on this task is from someone else
    /// (i.e. you need to reply).
    pub needs_reply: bool,
    /// Who left the last comment/review.
    pub last_commenter: Option<String>,
    /// Recent activity (comments, reviews) — newest first.
    /// Populated on fetch, used to seed session activity on first load.
    #[serde(default)]
    pub recent_activity: Vec<Activity>,
    /// Number of lines added in this PR.
    #[serde(default)]
    pub additions: u32,
    /// Number of lines deleted in this PR.
    #[serde(default)]
    pub deletions: u32,
}

/// Computed urgency level for a session. Used for inbox sorting.
/// Ordered from most urgent (0) to least urgent (7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ActionPriority {
    /// Someone commented on your PR and you haven't responded.
    NeedsReply = 0,
    /// CI failed on a PR you authored.
    CiFailed = 1,
    /// A reviewer requested changes on your PR.
    ChangesRequested = 2,
    /// You're requested as a reviewer — you're blocking someone.
    NeedsYourReview = 3,
    /// Your PR is approved and ready to merge.
    ApprovedReadyToMerge = 4,
    /// New unread activity (comments, CI updates, etc.).
    NewActivity = 5,
    /// No action needed — waiting on others.
    WaitingOnOthers = 6,
    /// Old, no recent activity.
    Stale = 7,
}

/// Status badge displayed on a PR row — derived purely from task fields.
/// Priority order matters: the first matching variant wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTag {
    /// Has merge conflicts with the base branch.
    Conflict,
    /// CI is failing.
    CiFailed,
    /// A reviewer requested changes.
    ChangesRequested,
    /// Actually sitting in GitHub's merge queue.
    Queued,
    /// Approved + CI green — ready to merge now.
    Ready,
    /// Auto-merge is armed (will merge when reviews + CI pass). Not the same
    /// as Queued — the PR may still be un-approved.
    AutoMerge,
    /// Awaiting review.
    ReviewPending,
    /// CI is still running.
    CiRunning,
    /// PR is a draft.
    Draft,
    /// Nothing interesting — no badge.
    None,
}

impl StatusTag {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Conflict => "CONFLICT",
            Self::CiFailed => "CI FAIL",
            Self::ChangesRequested => "CHANGES",
            Self::Queued => "QUEUED",
            Self::Ready => "READY",
            Self::AutoMerge => "AUTO",
            Self::ReviewPending => "REVIEW",
            Self::CiRunning => "CI...",
            Self::Draft => "DRAFT",
            Self::None => "",
        }
    }

    /// Derive the status tag for a task. Pure — unit-testable.
    ///
    /// Priority (first match wins): conflict → CI fail → changes requested
    /// → actually-queued → ready → auto-merge armed → review pending →
    /// CI running → draft → none.
    pub fn for_task(task: &Task) -> Self {
        if task.has_conflicts {
            Self::Conflict
        } else if task.ci == CiStatus::Failure {
            Self::CiFailed
        } else if task.review == ReviewStatus::ChangesRequested {
            Self::ChangesRequested
        } else if task.is_in_merge_queue {
            Self::Queued
        } else if task.review == ReviewStatus::Approved
            && matches!(task.ci, CiStatus::Success | CiStatus::None)
        {
            Self::Ready
        } else if task.auto_merge_enabled {
            Self::AutoMerge
        } else if task.review == ReviewStatus::Pending {
            Self::ReviewPending
        } else if matches!(task.ci, CiStatus::Running | CiStatus::Pending) {
            Self::CiRunning
        } else if matches!(task.state, TaskState::Draft) {
            Self::Draft
        } else {
            Self::None
        }
    }
}

impl ActionPriority {
    /// Human-readable label for section headers.
    pub fn label(&self) -> &'static str {
        match self {
            Self::NeedsReply => "Needs Your Reply",
            Self::CiFailed => "CI Failed",
            Self::ChangesRequested => "Changes Requested",
            Self::NeedsYourReview => "Needs Your Review",
            Self::ApprovedReadyToMerge => "Ready to Merge",
            Self::NewActivity => "New Activity",
            Self::WaitingOnOthers => "Waiting on Others",
            Self::Stale => "Stale",
        }
    }
}

#[cfg(test)]
mod status_tag_tests {
    use super::*;
    use chrono::Utc;

    fn base() -> Task {
        Task {
            id: TaskId { source: "gh".into(), key: "o/r#1".into() },
            title: "t".into(), body: None,
            state: TaskState::Open, role: TaskRole::Author,
            ci: CiStatus::None, review: ReviewStatus::None,
            checks: vec![], unread_count: 0,
            url: "u".into(), repo: Some("o/r".into()),
            branch: Some("b".into()), base_branch: None, updated_at: Utc::now(),
            labels: vec![], reviewers: vec![], assignees: vec![],
            auto_merge_enabled: false, is_in_merge_queue: false,
            has_conflicts: false, is_behind_base: false, node_id: None,
            needs_reply: false,
            last_commenter: None, recent_activity: vec![],
            additions: 0, deletions: 0,
        }
    }

    #[test]
    fn auto_merge_armed_is_not_queued() {
        // REGRESSION: auto_merge_enabled used to render as "QUEUED" even
        // though the PR was neither approved nor actually in the queue.
        // Now it renders as AutoMerge (distinct from Queued).
        let mut t = base();
        t.auto_merge_enabled = true;
        t.review = ReviewStatus::None;
        t.is_in_merge_queue = false;
        assert_eq!(StatusTag::for_task(&t), StatusTag::AutoMerge);
        assert_ne!(StatusTag::for_task(&t), StatusTag::Queued);
    }

    #[test]
    fn actually_in_merge_queue_is_queued() {
        let mut t = base();
        t.is_in_merge_queue = true;
        assert_eq!(StatusTag::for_task(&t), StatusTag::Queued);
    }

    #[test]
    fn approved_plus_green_ci_is_ready() {
        let mut t = base();
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::Success;
        assert_eq!(StatusTag::for_task(&t), StatusTag::Ready);
    }

    #[test]
    fn conflict_trumps_everything() {
        let mut t = base();
        t.has_conflicts = true;
        t.ci = CiStatus::Failure;
        t.review = ReviewStatus::ChangesRequested;
        t.is_in_merge_queue = true;
        assert_eq!(StatusTag::for_task(&t), StatusTag::Conflict);
    }

    #[test]
    fn ci_failed_beats_everything_but_conflict() {
        let mut t = base();
        t.ci = CiStatus::Failure;
        t.review = ReviewStatus::Approved;
        t.is_in_merge_queue = true;
        assert_eq!(StatusTag::for_task(&t), StatusTag::CiFailed);
    }

    #[test]
    fn changes_requested_shown_before_queue_hints() {
        let mut t = base();
        t.review = ReviewStatus::ChangesRequested;
        t.auto_merge_enabled = true;
        assert_eq!(StatusTag::for_task(&t), StatusTag::ChangesRequested);
    }

    #[test]
    fn queue_beats_ready_when_both_apply() {
        // If it's actually in the queue, that's the more specific fact.
        let mut t = base();
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::Success;
        t.is_in_merge_queue = true;
        assert_eq!(StatusTag::for_task(&t), StatusTag::Queued);
    }

    #[test]
    fn draft_only_shows_when_nothing_else_applies() {
        let mut t = base();
        t.state = TaskState::Draft;
        assert_eq!(StatusTag::for_task(&t), StatusTag::Draft);
        // But CI failure still wins.
        t.ci = CiStatus::Failure;
        assert_eq!(StatusTag::for_task(&t), StatusTag::CiFailed);
    }

    #[test]
    fn no_tag_for_plain_open_pr() {
        assert_eq!(StatusTag::for_task(&base()), StatusTag::None);
    }

    #[test]
    fn labels_match_expectations() {
        assert_eq!(StatusTag::Conflict.label(), "CONFLICT");
        assert_eq!(StatusTag::CiFailed.label(), "CI FAIL");
        assert_eq!(StatusTag::Queued.label(), "QUEUED");
        assert_eq!(StatusTag::AutoMerge.label(), "AUTO");
        assert_eq!(StatusTag::Ready.label(), "READY");
        assert_eq!(StatusTag::None.label(), "");
    }
}
