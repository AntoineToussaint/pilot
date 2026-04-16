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
    pub updated_at: DateTime<Utc>,
    pub labels: Vec<String>,
    /// Requested reviewers (user logins or team names).
    #[serde(default)]
    pub reviewers: Vec<String>,
    /// Assignees (user logins).
    #[serde(default)]
    pub assignees: Vec<String>,
    /// Whether this PR is in a merge queue / has auto-merge enabled.
    #[serde(default)]
    pub in_merge_queue: bool,
    /// Whether this PR has merge conflicts.
    #[serde(default)]
    pub has_conflicts: bool,
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
