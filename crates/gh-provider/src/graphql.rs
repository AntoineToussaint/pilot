//! GraphQL-based GitHub data fetching. One query gets everything.

use chrono::{DateTime, Utc};
use pilot_core::*;
use serde::Deserialize;

/// The single GraphQL query that fetches all PR data.
const SEARCH_QUERY: &str = r#"
query($query: String!, $first: Int!, $after: String) {
  search(query: $query, type: ISSUE, first: $first, after: $after) {
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on PullRequest {
        id
        number
        title
        body
        url
        updatedAt
        createdAt
        isDraft
        state
        merged
        additions
        deletions
        headRefName
        baseRefName
        mergeable
        mergeStateStatus
        reviewDecision
        autoMergeRequest { enabledAt }
        isInMergeQueue
        author { login }
        commits(last: 1) {
          nodes {
            commit {
              statusCheckRollup {
                state
                contexts(first: 30) {
                  nodes {
                    __typename
                    ... on CheckRun {
                      name
                      conclusion
                      status
                      permalink
                    }
                    ... on StatusContext {
                      context
                      state
                      targetUrl
                    }
                  }
                }
              }
            }
          }
        }
        labels(first: 10) { nodes { name } }
        assignees(first: 10) { nodes { login } }
        reviewRequests(first: 10) {
          nodes {
            requestedReviewer {
              ... on User { login }
              ... on Team { name }
            }
          }
        }
        comments(first: 30) {
          nodes {
            id
            author { login }
            body
            createdAt
          }
        }
        reviews(first: 20) {
          nodes {
            author { login }
            body
            state
            submittedAt
          }
        }
        reviewThreads(first: 50) {
          nodes {
            id
            isResolved
            isOutdated
            path
            line
            originalLine
            comments(first: 10) {
              nodes {
                id
                author { login }
                body
                createdAt
                path
                line
                originalLine
                diffHunk
              }
            }
          }
        }
      }
    }
  }
  rateLimit {
    remaining
    resetAt
  }
}
"#;

// ─── Response types ────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
pub struct GqlResponse {
    pub data: Option<GqlData>,
    pub errors: Option<Vec<GqlError>>,
}

#[derive(Deserialize, Debug)]
pub struct GqlError {
    pub message: String,
    /// GraphQL path to the node that failed (if any).
    #[serde(default)]
    pub path: Option<Vec<serde_json::Value>>,
    /// Error type / extensions (often contains the attribute name).
    #[serde(default)]
    pub extensions: Option<serde_json::Value>,
    /// Source locations (line/col in the query).
    #[serde(default)]
    #[allow(dead_code)] // Captured for debug format — not yet used in messages.
    pub locations: Option<Vec<serde_json::Value>>,
}

impl GqlError {
    /// Human-readable debug line including path + extensions, not just the message.
    pub fn full(&self) -> String {
        let mut s = self.message.clone();
        if let Some(path) = &self.path
            && !path.is_empty()
        {
            let path_str = path
                .iter()
                .filter_map(|v| {
                    v.as_str()
                        .map(String::from)
                        .or_else(|| v.as_u64().map(|n| n.to_string()))
                })
                .collect::<Vec<_>>()
                .join(".");
            s.push_str(&format!(" [at {path_str}]"));
        }
        if let Some(ext) = &self.extensions {
            s.push_str(&format!(" (ext: {ext})"));
        }
        s
    }
}

#[derive(Deserialize, Debug)]
pub struct GqlData {
    pub search: GqlSearch,
    #[serde(rename = "rateLimit")]
    pub rate_limit: Option<GqlRateLimit>,
}

#[derive(Deserialize, Debug)]
pub struct GqlRateLimit {
    pub remaining: u32,
    #[serde(rename = "resetAt")]
    pub reset_at: String,
}

#[derive(Deserialize, Debug)]
pub struct GqlSearch {
    pub nodes: Vec<GqlPr>,
    #[serde(rename = "pageInfo", default)]
    pub page_info: Option<GqlPageInfo>,
}

#[derive(Deserialize, Debug, Default)]
pub struct GqlPageInfo {
    #[serde(rename = "hasNextPage", default)]
    pub has_next_page: bool,
    #[serde(rename = "endCursor", default)]
    pub end_cursor: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct GqlPr {
    /// GraphQL node ID — needed for mutations like `updatePullRequestBranch`.
    #[serde(default)]
    pub id: Option<String>,
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub url: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
    #[serde(rename = "isDraft")]
    pub is_draft: bool,
    pub state: String, // OPEN, CLOSED, MERGED
    pub merged: bool,
    #[serde(default)]
    pub additions: u32,
    #[serde(default)]
    pub deletions: u32,
    #[serde(rename = "headRefName")]
    pub head_ref_name: String,
    #[serde(rename = "baseRefName", default)]
    pub base_ref_name: String,
    /// MERGEABLE, CONFLICTING, or UNKNOWN.
    #[serde(default)]
    pub mergeable: Option<String>,
    /// BEHIND, BLOCKED, CLEAN, DIRTY, DRAFT, HAS_HOOKS, UNKNOWN, UNSTABLE.
    /// `BEHIND` is what drives the "Update branch" button on github.com.
    #[serde(default, rename = "mergeStateStatus")]
    pub merge_state_status: Option<String>,
    #[serde(rename = "reviewDecision")]
    pub review_decision: Option<String>, // APPROVED, CHANGES_REQUESTED, REVIEW_REQUIRED
    #[serde(rename = "autoMergeRequest")]
    pub auto_merge_request: Option<GqlAutoMerge>,
    #[serde(default, rename = "isInMergeQueue")]
    pub is_in_merge_queue: bool,
    pub author: Option<GqlAuthor>,
    pub labels: GqlLabels,
    pub assignees: GqlAssignees,
    #[serde(rename = "reviewRequests")]
    pub review_requests: GqlReviewRequests,
    pub comments: GqlComments,
    pub reviews: GqlReviews,
    #[serde(rename = "reviewThreads")]
    pub review_threads: GqlReviewThreads,
    pub commits: GqlCommits,
}

#[derive(Deserialize, Debug)]
pub struct GqlCommits {
    pub nodes: Vec<GqlCommitNode>,
}

#[derive(Deserialize, Debug)]
pub struct GqlCommitNode {
    pub commit: GqlCommit,
}

#[derive(Deserialize, Debug)]
pub struct GqlCommit {
    #[serde(rename = "statusCheckRollup")]
    pub status_check_rollup: Option<GqlStatusRollup>,
}

#[derive(Deserialize, Debug)]
pub struct GqlStatusRollup {
    /// SUCCESS, FAILURE, ERROR, PENDING, EXPECTED
    pub state: String,
    pub contexts: GqlCheckContexts,
}

#[derive(Deserialize, Debug)]
pub struct GqlCheckContexts {
    pub nodes: Vec<GqlCheckContext>,
}

/// Check context — polymorphic (CheckRun | StatusContext).
#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum GqlCheckContext {
    CheckRun {
        name: String,
        conclusion: Option<String>, // SUCCESS, FAILURE, NEUTRAL, CANCELLED, TIMED_OUT, ACTION_REQUIRED, SKIPPED
        #[allow(dead_code)]
        status: Option<String>, // QUEUED, IN_PROGRESS, COMPLETED, WAITING, PENDING, REQUESTED
        #[allow(dead_code)]
        permalink: Option<String>,
    },
    StatusContext {
        context: String,
        state: String, // EXPECTED, ERROR, FAILURE, PENDING, SUCCESS
        #[allow(dead_code)]
        #[serde(rename = "targetUrl")]
        target_url: Option<String>,
    },
}

#[derive(Deserialize, Debug)]
pub struct GqlAuthor {
    pub login: String,
}

#[derive(Deserialize, Debug)]
pub struct GqlLabels {
    pub nodes: Vec<GqlLabel>,
}

#[derive(Deserialize, Debug)]
pub struct GqlLabel {
    pub name: String,
}

#[derive(Deserialize, Debug)]
pub struct GqlAutoMerge {
    #[allow(dead_code)]
    #[serde(rename = "enabledAt")]
    pub enabled_at: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct GqlAssignees {
    pub nodes: Vec<GqlAuthor>,
}

#[derive(Deserialize, Debug)]
pub struct GqlReviewRequests {
    pub nodes: Vec<GqlReviewRequest>,
}

#[derive(Deserialize, Debug)]
pub struct GqlReviewRequest {
    #[serde(rename = "requestedReviewer")]
    pub requested_reviewer: Option<GqlRequestedReviewer>,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum GqlRequestedReviewer {
    User { login: String },
    Team { name: String },
}

#[derive(Deserialize, Debug)]
pub struct GqlComments {
    pub nodes: Vec<GqlComment>,
}

#[derive(Deserialize, Debug)]
pub struct GqlComment {
    #[serde(default)]
    pub id: Option<String>,
    pub author: Option<GqlAuthor>,
    pub body: String,
    #[serde(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default, rename = "originalLine")]
    pub original_line: Option<u32>,
    #[serde(default, rename = "diffHunk")]
    pub diff_hunk: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct GqlReviews {
    pub nodes: Vec<GqlReview>,
}

#[derive(Deserialize, Debug)]
pub struct GqlReview {
    pub author: Option<GqlAuthor>,
    pub body: Option<String>,
    pub state: String, // APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED, PENDING
    #[serde(rename = "submittedAt")]
    pub submitted_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize, Debug)]
pub struct GqlReviewThreads {
    pub nodes: Vec<GqlReviewThread>,
}

#[derive(Deserialize, Debug)]
pub struct GqlReviewThread {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "isResolved")]
    pub is_resolved: bool,
    #[serde(rename = "isOutdated")]
    pub is_outdated: bool,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default, rename = "originalLine")]
    pub original_line: Option<u32>,
    pub comments: GqlComments,
}

// ─── Conversion ────────────────────────────────────────────────────────────

pub fn build_query(username: &str, filters: &[String]) -> String {
    let mut parts = vec![
        "is:open".to_string(),
        "is:pr".to_string(),
        format!("involves:{username}"),
        "archived:false".to_string(),
    ];
    parts.extend(filters.iter().cloned());
    parts.join(" ")
}

pub fn query_body(search_query: &str) -> serde_json::Value {
    query_body_after(search_query, None)
}

pub fn query_body_after(search_query: &str, after: Option<&str>) -> serde_json::Value {
    // Omit `after` entirely when None — sending `"after": null` in the
    // variables block trips GitHub's GraphQL with a misleading
    // "A query attribute must be specified and must be a string" error.
    // With the variable absent, the query's `$after: String` parameter
    // defaults to null on GitHub's side, which is what we want.
    let variables = match after {
        Some(cursor) => serde_json::json!({
            "query": search_query,
            "first": 100,
            "after": cursor,
        }),
        None => serde_json::json!({
            "query": search_query,
            "first": 100,
        }),
    };
    serde_json::json!({
        "query": SEARCH_QUERY,
        "variables": variables,
    })
}

/// GraphQL mutation that merges the base branch into the PR head — same
/// effect as clicking "Update branch" on github.com. Default method is MERGE;
/// pass REBASE if the repo prefers it.
const UPDATE_BRANCH_MUTATION: &str = r#"
mutation($id: ID!) {
  updatePullRequestBranch(input: { pullRequestId: $id }) {
    pullRequest { id }
  }
}
"#;

pub fn update_branch_body(pull_request_node_id: &str) -> serde_json::Value {
    serde_json::json!({
        "query": UPDATE_BRANCH_MUTATION,
        "variables": { "id": pull_request_node_id },
    })
}

/// Convert GraphQL PR data to our Task type.
pub fn pr_to_task(pr: &GqlPr, my_username: &str) -> Task {
    let repo = extract_repo_from_url(&pr.url);

    // Determine role.
    let is_author = pr
        .author
        .as_ref()
        .map(|a| a.login == my_username)
        .unwrap_or(false);
    // Did I already approve this PR?
    let i_approved = pr.reviews.nodes.iter().any(|r| {
        r.state == "APPROVED"
            && r.author
                .as_ref()
                .map(|a| a.login == my_username)
                .unwrap_or(false)
    });
    let role = if is_author {
        TaskRole::Author
    } else if i_approved {
        // I've approved — treat as Mentioned (low priority, done my part).
        TaskRole::Mentioned
    } else {
        TaskRole::Reviewer
    };

    // State.
    let state = if pr.merged {
        TaskState::Merged
    } else if pr.state == "CLOSED" {
        TaskState::Closed
    } else if pr.is_draft {
        TaskState::Draft
    } else {
        TaskState::Open
    };

    // Review status from reviewDecision.
    let review = match pr.review_decision.as_deref() {
        Some("APPROVED") => ReviewStatus::Approved,
        Some("CHANGES_REQUESTED") => ReviewStatus::ChangesRequested,
        Some("REVIEW_REQUIRED") => ReviewStatus::Pending,
        _ => ReviewStatus::None,
    };

    // Build activity from all sources, sorted by time.
    let mut activities: Vec<Activity> = Vec::new();

    // Issue comments.
    for c in &pr.comments.nodes {
        if c.body.trim().is_empty() {
            continue;
        }
        activities.push(Activity {
            author: c
                .author
                .as_ref()
                .map(|a| a.login.clone())
                .unwrap_or_else(|| "?".into()),
            body: c.body.clone(),
            created_at: c.created_at,
            kind: ActivityKind::Comment,
            node_id: c.id.clone(),
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        });
    }

    // Review threads (with resolution + outdated status).
    for thread in &pr.review_threads.nodes {
        // Thread-level path/line fall back to the first comment's values.
        let thread_path = thread
            .path
            .clone()
            .or_else(|| thread.comments.nodes.first().and_then(|c| c.path.clone()));
        let thread_line = thread.line.or(thread.original_line).or_else(|| {
            thread
                .comments
                .nodes
                .first()
                .and_then(|c| c.line.or(c.original_line))
        });

        for (i, c) in thread.comments.nodes.iter().enumerate() {
            let author = c
                .author
                .as_ref()
                .map(|a| a.login.clone())
                .unwrap_or_else(|| "?".into());
            if c.body.trim().is_empty() {
                continue;
            }
            let mut body = c.body.clone();

            // Prefix with context.
            if thread.is_resolved {
                body = format!("✅ {body}");
            } else if thread.is_outdated {
                body = format!("📌 outdated: {body}");
            }
            if i > 0 {
                body = format!("↳ {body}");
            }

            // Only the first comment in a thread carries the diff hunk;
            // replies inherit file/line for display context.
            let (path, line, diff_hunk) = if i == 0 {
                (
                    c.path.clone().or_else(|| thread_path.clone()),
                    c.line.or(c.original_line).or(thread_line),
                    c.diff_hunk.clone(),
                )
            } else {
                (thread_path.clone(), thread_line, None)
            };

            activities.push(Activity {
                author,
                body,
                created_at: c.created_at,
                kind: ActivityKind::Review,
                node_id: c.id.clone(),
                path,
                line,
                diff_hunk,
                thread_id: thread.id.clone(),
            });
        }
    }

    // Reviews (approve/changes requested — only with meaningful content).
    for r in &pr.reviews.nodes {
        let body = r.body.as_deref().unwrap_or("");
        if body.is_empty() && r.state != "APPROVED" && r.state != "CHANGES_REQUESTED" {
            continue;
        }
        let display = if !body.is_empty() {
            body.to_string()
        } else {
            format!("✓ {}", r.state)
        };
        activities.push(Activity {
            author: r
                .author
                .as_ref()
                .map(|a| a.login.clone())
                .unwrap_or_else(|| "?".into()),
            body: display,
            created_at: r.submitted_at.unwrap_or(pr.updated_at),
            kind: ActivityKind::Review,
            node_id: None, // Reviews don't have reply-to IDs.
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        });
    }

    activities.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    // Needs reply: check three signals.
    let needs_reply = needs_reply_check(pr, my_username);

    let last_commenter = activities
        .first()
        .filter(|a| a.author != my_username)
        .map(|a| a.author.clone());

    let unread_count = activities.len() as u32;

    Task {
        id: TaskId {
            source: "github".into(),
            key: format!("{repo}#{}", pr.number),
        },
        title: pr.title.clone(),
        body: pr.body.clone(),
        state,
        role,
        ci: extract_ci_status(pr),
        review,
        checks: extract_check_runs(pr),
        unread_count,
        url: pr.url.clone(),
        repo: Some(repo),
        branch: Some(pr.head_ref_name.clone()),
        base_branch: if pr.base_ref_name.is_empty() {
            None
        } else {
            Some(pr.base_ref_name.clone())
        },
        updated_at: pr.updated_at,
        labels: pr.labels.nodes.iter().map(|l| l.name.clone()).collect(),
        reviewers: pr
            .review_requests
            .nodes
            .iter()
            .filter_map(|rr| rr.requested_reviewer.as_ref())
            .map(|rr| match rr {
                GqlRequestedReviewer::User { login } => login.clone(),
                GqlRequestedReviewer::Team { name } => format!("team/{name}"),
            })
            .collect(),
        assignees: pr.assignees.nodes.iter().map(|a| a.login.clone()).collect(),
        auto_merge_enabled: pr.auto_merge_request.is_some(),
        is_in_merge_queue: pr.is_in_merge_queue,
        has_conflicts: pr.mergeable.as_deref() == Some("CONFLICTING"),
        is_behind_base: pr.merge_state_status.as_deref() == Some("BEHIND"),
        node_id: pr.id.clone(),
        needs_reply,
        last_commenter,
        recent_activity: activities,
        additions: pr.additions,
        deletions: pr.deletions,
    }
}

/// Comprehensive needs_reply: check unresolved threads, latest issue comment,
/// and latest review. If the most recent communication on any channel is from
/// someone else and you haven't responded, you owe a reply.
fn needs_reply_check(pr: &GqlPr, my_username: &str) -> bool {
    // 1. Unresolved review threads where the LAST comment is from someone else.
    for t in &pr.review_threads.nodes {
        if t.is_resolved || t.is_outdated {
            continue;
        }
        if let Some(last) = t.comments.nodes.last()
            && let Some(author) = &last.author
            && author.login != my_username
        {
            return true;
        }
    }

    // 2. Latest issue comment is from someone else (and after our last response).
    let my_last_comment = pr
        .comments
        .nodes
        .iter()
        .filter(|c| {
            c.author
                .as_ref()
                .map(|a| a.login == my_username)
                .unwrap_or(false)
        })
        .map(|c| c.created_at)
        .max();
    let last_others_comment = pr
        .comments
        .nodes
        .iter()
        .filter(|c| {
            c.author
                .as_ref()
                .map(|a| a.login != my_username)
                .unwrap_or(false)
        })
        .map(|c| c.created_at)
        .max();
    if let Some(other) = last_others_comment
        && my_last_comment.map(|m| other > m).unwrap_or(true)
    {
        return true;
    }

    // 3. Latest review with body from someone else (after our last review/comment).
    let last_others_review = pr
        .reviews
        .nodes
        .iter()
        .filter(|r| {
            r.author
                .as_ref()
                .map(|a| a.login != my_username)
                .unwrap_or(false)
                && r.body.as_deref().map(|b| !b.is_empty()).unwrap_or(false)
        })
        .filter_map(|r| r.submitted_at)
        .max();
    if let Some(other) = last_others_review {
        let my_latest = my_last_comment;
        if my_latest.map(|m| other > m).unwrap_or(true) {
            return true;
        }
    }

    false
}

fn extract_ci_status(pr: &GqlPr) -> CiStatus {
    let Some(commit_node) = pr.commits.nodes.first() else {
        return CiStatus::None;
    };
    let Some(rollup) = &commit_node.commit.status_check_rollup else {
        return CiStatus::None;
    };
    match rollup.state.as_str() {
        "SUCCESS" => CiStatus::Success,
        "FAILURE" | "ERROR" => CiStatus::Failure,
        "PENDING" => CiStatus::Pending,
        "EXPECTED" => CiStatus::Pending,
        _ => CiStatus::None,
    }
}

fn extract_check_runs(pr: &GqlPr) -> Vec<CheckRun> {
    let Some(commit_node) = pr.commits.nodes.first() else {
        return vec![];
    };
    let Some(rollup) = &commit_node.commit.status_check_rollup else {
        return vec![];
    };
    rollup
        .contexts
        .nodes
        .iter()
        .map(|ctx| match ctx {
            GqlCheckContext::CheckRun {
                name,
                conclusion,
                permalink,
                ..
            } => CheckRun {
                name: name.clone(),
                status: match conclusion.as_deref() {
                    Some("SUCCESS") => CiStatus::Success,
                    Some("FAILURE") | Some("ACTION_REQUIRED") | Some("TIMED_OUT") => {
                        CiStatus::Failure
                    }
                    Some("CANCELLED") => CiStatus::Failure,
                    Some(_) => CiStatus::None,
                    None => CiStatus::Running,
                },
                url: permalink.clone(),
            },
            GqlCheckContext::StatusContext {
                context,
                state,
                target_url,
            } => CheckRun {
                name: context.clone(),
                status: match state.as_str() {
                    "SUCCESS" => CiStatus::Success,
                    "FAILURE" | "ERROR" => CiStatus::Failure,
                    "PENDING" | "EXPECTED" => CiStatus::Pending,
                    _ => CiStatus::None,
                },
                url: target_url.clone(),
            },
        })
        .collect()
}

fn extract_repo_from_url(url: &str) -> String {
    // https://github.com/owner/repo/pull/123
    let parts: Vec<&str> = url.trim_end_matches('/').split('/').collect();
    if parts.len() >= 5 {
        format!("{}/{}", parts[3], parts[4])
    } else {
        "unknown/unknown".into()
    }
}

// ─── GitHub Issues ─────────────────────────────────────────────────────
//
// v2 additive extension: Issues get fetched alongside PRs by a separate
// GraphQL query. Issues are strictly a subset of PR fields (no branches,
// no CI, no reviewers) so the query is simpler. v1 doesn't call any of
// the following functions; they're only used by the v2 daemon.

const ISSUES_QUERY: &str = r#"
query($query: String!, $first: Int!, $after: String) {
  search(query: $query, type: ISSUE, first: $first, after: $after) {
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on Issue {
        id
        number
        title
        body
        url
        updatedAt
        createdAt
        state
        author { login }
        labels(first: 10) { nodes { name } }
        assignees(first: 10) { nodes { login } }
        comments(first: 30) {
          nodes {
            id
            author { login }
            body
            createdAt
          }
        }
        repository {
          nameWithOwner
        }
      }
    }
  }
  rateLimit {
    remaining
    resetAt
  }
}
"#;

#[derive(Deserialize, Debug)]
pub struct GqlIssue {
    #[serde(default)]
    pub id: Option<String>,
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub url: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
    pub state: String, // OPEN, CLOSED
    pub author: Option<GqlAuthor>,
    pub labels: GqlLabels,
    pub assignees: GqlAssignees,
    pub comments: GqlComments,
    #[serde(default)]
    pub repository: Option<GqlIssueRepo>,
}

#[derive(Deserialize, Debug)]
pub struct GqlIssueRepo {
    #[serde(rename = "nameWithOwner")]
    pub name_with_owner: String,
}

#[derive(Deserialize, Debug)]
pub struct GqlIssueSearch {
    pub nodes: Vec<GqlIssue>,
    #[serde(rename = "pageInfo", default)]
    pub page_info: Option<GqlPageInfo>,
}

#[derive(Deserialize, Debug)]
pub struct GqlIssueData {
    pub search: GqlIssueSearch,
    #[serde(rename = "rateLimit")]
    pub rate_limit: Option<GqlRateLimit>,
}

#[derive(Deserialize, Debug)]
pub struct GqlIssueResponse {
    pub data: Option<GqlIssueData>,
    pub errors: Option<Vec<GqlError>>,
}

pub fn build_issues_query(username: &str, filters: &[String]) -> String {
    let mut parts = vec![
        "is:open".to_string(),
        "is:issue".to_string(),
        format!("involves:{username}"),
        "archived:false".to_string(),
    ];
    parts.extend(filters.iter().cloned());
    parts.join(" ")
}

pub fn issues_query_body(search_query: &str, after: Option<&str>) -> serde_json::Value {
    let variables = match after {
        Some(cursor) => serde_json::json!({
            "query": search_query,
            "first": 100,
            "after": cursor,
        }),
        None => serde_json::json!({
            "query": search_query,
            "first": 100,
        }),
    };
    serde_json::json!({
        "query": ISSUES_QUERY,
        "variables": variables,
    })
}

/// Convert a GraphQL Issue into our `Task` type. Issues have no
/// branch/CI/reviewers — those fields stay empty / None / Default.
/// Role is determined from author + assignees against `my_username`.
pub fn issue_to_task(issue: &GqlIssue, my_username: &str) -> Task {
    let repo = issue
        .repository
        .as_ref()
        .map(|r| r.name_with_owner.clone())
        .unwrap_or_else(|| extract_repo_from_url(&issue.url));

    let is_author = issue
        .author
        .as_ref()
        .map(|a| a.login == my_username)
        .unwrap_or(false);
    let is_assignee = issue.assignees.nodes.iter().any(|a| a.login == my_username);
    let role = if is_author {
        TaskRole::Author
    } else if is_assignee {
        TaskRole::Assignee
    } else {
        TaskRole::Mentioned
    };

    let state = match issue.state.as_str() {
        "OPEN" => TaskState::Open,
        "CLOSED" => TaskState::Closed,
        _ => TaskState::Open,
    };

    let comments: Vec<Activity> = issue
        .comments
        .nodes
        .iter()
        .filter(|c| c.author.is_some())
        .map(|c| Activity {
            author: c.author.as_ref().unwrap().login.clone(),
            body: c.body.clone(),
            created_at: c.created_at,
            kind: ActivityKind::Comment,
            node_id: c.id.clone(),
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        })
        .collect();

    let needs_reply = comments
        .last()
        .map(|c| c.author != my_username)
        .unwrap_or(false);
    let last_commenter = comments
        .iter()
        .filter(|a| a.author != my_username)
        .last()
        .map(|a| a.author.clone());

    Task {
        id: TaskId {
            source: "github".into(),
            key: format!("{repo}#{}", issue.number),
        },
        title: issue.title.clone(),
        body: issue.body.clone(),
        state,
        role,
        ci: CiStatus::None,
        review: ReviewStatus::None,
        checks: vec![],
        unread_count: comments.len() as u32,
        url: issue.url.clone(),
        repo: Some(repo),
        branch: None,
        base_branch: None,
        updated_at: issue.updated_at,
        labels: issue.labels.nodes.iter().map(|l| l.name.clone()).collect(),
        reviewers: vec![],
        assignees: issue
            .assignees
            .nodes
            .iter()
            .map(|a| a.login.clone())
            .collect(),
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        has_conflicts: false,
        is_behind_base: false,
        node_id: issue.id.clone(),
        needs_reply,
        last_commenter,
        recent_activity: comments,
        additions: 0,
        deletions: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_repo_from_url() {
        assert_eq!(
            extract_repo_from_url("https://github.com/owner/repo/pull/123"),
            "owner/repo"
        );
        assert_eq!(
            extract_repo_from_url("https://github.com/my-org/my-repo/pull/1"),
            "my-org/my-repo"
        );
    }

    #[test]
    fn test_extract_repo_malformed() {
        assert_eq!(extract_repo_from_url("not-a-url"), "unknown/unknown");
        assert_eq!(extract_repo_from_url(""), "unknown/unknown");
    }

    #[test]
    fn test_build_query() {
        let q = build_query("alice", &[]);
        assert!(q.contains("involves:alice"));
        assert!(q.contains("is:open"));
        assert!(q.contains("is:pr"));
    }

    #[test]
    fn test_build_query_with_filters() {
        let q = build_query("bob", &["org:myorg".into()]);
        assert!(q.contains("org:myorg"));
        assert!(q.contains("involves:bob"));
    }

    // ── Issues ────────────────────────────────────────────────────────

    #[test]
    fn issues_query_filters_to_issues() {
        let q = build_issues_query("alice", &[]);
        assert!(q.contains("is:open"));
        assert!(q.contains("is:issue"));
        assert!(!q.contains("is:pr"));
        assert!(q.contains("involves:alice"));
    }

    fn make_issue(number: u64, title: &str, author: Option<&str>, assignees: &[&str]) -> GqlIssue {
        GqlIssue {
            id: Some(format!("I_{number}")),
            number,
            title: title.into(),
            body: None,
            url: format!("https://github.com/o/r/issues/{number}"),
            updated_at: chrono::Utc::now(),
            state: "OPEN".into(),
            author: author.map(|login| GqlAuthor {
                login: login.into(),
            }),
            labels: GqlLabels { nodes: vec![] },
            assignees: GqlAssignees {
                nodes: assignees
                    .iter()
                    .map(|l| GqlAuthor { login: (*l).into() })
                    .collect(),
            },
            comments: GqlComments { nodes: vec![] },
            repository: Some(GqlIssueRepo {
                name_with_owner: "o/r".into(),
            }),
        }
    }

    #[test]
    fn issue_to_task_author_role() {
        let issue = make_issue(1, "something", Some("alice"), &[]);
        let task = issue_to_task(&issue, "alice");
        assert_eq!(task.role, TaskRole::Author);
        assert_eq!(task.state, TaskState::Open);
        assert_eq!(task.branch, None, "issues have no branch");
        assert_eq!(task.ci, CiStatus::None);
        assert_eq!(task.review, ReviewStatus::None);
        assert_eq!(task.id.key, "o/r#1");
        assert_eq!(task.id.source, "github");
    }

    #[test]
    fn issue_to_task_assignee_role() {
        let issue = make_issue(2, "t", Some("someone-else"), &["alice"]);
        let task = issue_to_task(&issue, "alice");
        assert_eq!(task.role, TaskRole::Assignee);
        assert!(task.assignees.contains(&"alice".to_string()));
    }

    #[test]
    fn issue_to_task_mentioned_role_when_neither() {
        let issue = make_issue(3, "t", Some("other"), &["another"]);
        let task = issue_to_task(&issue, "alice");
        assert_eq!(task.role, TaskRole::Mentioned);
    }

    #[test]
    fn issue_to_task_closed_state() {
        let mut issue = make_issue(4, "done", Some("alice"), &[]);
        issue.state = "CLOSED".into();
        let task = issue_to_task(&issue, "alice");
        assert_eq!(task.state, TaskState::Closed);
    }

    #[test]
    fn issue_to_task_uses_repository_field_when_present() {
        let mut issue = make_issue(5, "t", None, &[]);
        issue.repository = Some(GqlIssueRepo {
            name_with_owner: "other-org/other-repo".into(),
        });
        let task = issue_to_task(&issue, "alice");
        assert_eq!(task.repo.as_deref(), Some("other-org/other-repo"));
    }

    #[test]
    fn issue_to_task_falls_back_to_url_parsing() {
        let mut issue = make_issue(6, "t", None, &[]);
        issue.repository = None;
        issue.url = "https://github.com/owner/repo/issues/6".into();
        let task = issue_to_task(&issue, "alice");
        assert_eq!(task.repo.as_deref(), Some("owner/repo"));
    }

    #[test]
    fn issue_to_task_ingests_comments_as_activities() {
        let mut issue = make_issue(7, "t", Some("alice"), &[]);
        issue.comments = GqlComments {
            nodes: vec![
                GqlComment {
                    id: Some("c1".into()),
                    author: Some(GqlAuthor {
                        login: "bob".into(),
                    }),
                    body: "first".into(),
                    created_at: chrono::Utc::now(),
                    path: None,
                    line: None,
                    original_line: None,
                    diff_hunk: None,
                },
                GqlComment {
                    id: Some("c2".into()),
                    author: Some(GqlAuthor {
                        login: "alice".into(),
                    }),
                    body: "reply".into(),
                    created_at: chrono::Utc::now(),
                    path: None,
                    line: None,
                    original_line: None,
                    diff_hunk: None,
                },
            ],
        };
        let task = issue_to_task(&issue, "alice");
        assert_eq!(task.recent_activity.len(), 2);
        assert_eq!(task.recent_activity[0].author, "bob");
        assert_eq!(task.recent_activity[0].kind, ActivityKind::Comment);
        // Last comment is me — so no reply needed.
        assert!(!task.needs_reply);
    }

    #[test]
    fn issue_to_task_needs_reply_when_last_comment_is_from_other() {
        let mut issue = make_issue(8, "t", Some("alice"), &[]);
        issue.comments = GqlComments {
            nodes: vec![GqlComment {
                id: Some("c1".into()),
                author: Some(GqlAuthor {
                    login: "bob".into(),
                }),
                body: "question".into(),
                created_at: chrono::Utc::now(),
                path: None,
                line: None,
                original_line: None,
                diff_hunk: None,
            }],
        };
        let task = issue_to_task(&issue, "alice");
        assert!(task.needs_reply);
        assert_eq!(task.last_commenter.as_deref(), Some("bob"));
    }

    #[test]
    fn issues_query_body_omits_after_when_none() {
        let body = issues_query_body("test", None);
        let vars = &body["variables"];
        assert!(vars.get("after").is_none(), "after must be omitted");
        assert_eq!(vars["first"], 100);
        assert_eq!(vars["query"], "test");
    }

    #[test]
    fn issues_query_body_includes_after_when_set() {
        let body = issues_query_body("test", Some("cursor-abc"));
        assert_eq!(body["variables"]["after"], "cursor-abc");
    }
}
