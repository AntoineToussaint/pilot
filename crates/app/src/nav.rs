use chrono::{DateTime, Utc};
use tokio::sync::mpsc;

use crate::action::Action;
use crate::app::App;
use pilot_core::{ActionPriority, CiStatus, Session, TaskRole, TaskState};

/// An item in the sidebar navigation list — either a repo header or a session.
#[derive(Debug, Clone)]
pub enum NavItem {
    Repo(String),
    Session(String), // session key
}

/// Inputs for the sidebar visibility predicate. Extracted from `App` so the
/// predicate can be unit-tested without constructing a whole app.
pub struct VisibilityCtx<'a> {
    pub username: &'a str,
    pub hide_approved_by_me: bool,
    /// If set, hide sessions whose primary task hasn't been updated since this.
    pub activity_cutoff: Option<DateTime<Utc>>,
    /// The reference "now" used for snooze/staleness decisions. Injected so
    /// the predicate is pure and testable.
    pub now: DateTime<Utc>,
}

/// Pure predicate: is this session currently visible in the sidebar?
///
/// Encapsulates the filter rules — merged/closed hidden, snoozed hidden,
/// "done my part as a reviewer" hidden, old sessions hidden via time cutoff.
pub fn is_session_visible(session: &Session, ctx: &VisibilityCtx) -> bool {
    if let Some(cutoff) = ctx.activity_cutoff
        && session.primary_task.updated_at < cutoff {
            return false;
        }
    if matches!(session.primary_task.state, TaskState::Merged | TaskState::Closed) {
        return false;
    }
    if session.is_snoozed(ctx.now) {
        return false;
    }
    if ctx.hide_approved_by_me
        && session.primary_task.role == TaskRole::Mentioned
        && session.unread_count() == 0
    {
        return false;
    }
    let priority = session.action_priority(ctx.username, ctx.now);
    if session.primary_task.role == TaskRole::Mentioned
        && session.unread_count() == 0
        && matches!(priority, ActionPriority::WaitingOnOthers | ActionPriority::Stale)
    {
        return false;
    }
    true
}

/// Pure predicate: does a session match a free-form search query?
///
/// Supports `needs:reply`, `ci:failed|passed|pending`, `is:unread|read|open|draft|merged|conflict`,
/// `role:author|reviewer|assignee`, `repo:foo`, and free text matched against
/// title / body / activity author / activity body. All tokens must match (AND).
pub fn session_matches_query(session: &Session, query: &str) -> bool {
    let task = &session.primary_task;
    for token in query.split_whitespace() {
        let matches = if let Some(val) = token.strip_prefix("needs:") {
            match val {
                "reply" => task.needs_reply,
                "review" => task.role == TaskRole::Reviewer,
                _ => true,
            }
        } else if let Some(val) = token.strip_prefix("ci:") {
            match val {
                "failed" | "fail" => task.ci == CiStatus::Failure,
                "passed" | "pass" | "ok" => task.ci == CiStatus::Success,
                "pending" => task.ci == CiStatus::Pending || task.ci == CiStatus::Running,
                _ => true,
            }
        } else if let Some(val) = token.strip_prefix("is:") {
            match val {
                "unread" => session.unread_count() > 0,
                "read" => session.unread_count() == 0,
                "open" => task.state == TaskState::Open,
                "draft" => task.state == TaskState::Draft,
                "merged" => task.state == TaskState::Merged,
                "conflict" | "conflicting" => task.has_conflicts,
                _ => true,
            }
        } else if let Some(val) = token.strip_prefix("role:") {
            match val {
                "author" => task.role == TaskRole::Author,
                "reviewer" | "review" => task.role == TaskRole::Reviewer,
                "assignee" => task.role == TaskRole::Assignee,
                _ => true,
            }
        } else if let Some(val) = token.strip_prefix("repo:") {
            session.repo.to_lowercase().contains(val)
        } else {
            let t = token.to_lowercase();
            let in_title = task.title.to_lowercase().contains(&t);
            let in_body = task.body.as_ref().map(|b| b.to_lowercase().contains(&t)).unwrap_or(false);
            let in_activity = session.activity.iter().any(|a| {
                a.author.to_lowercase().contains(&t) || a.body.to_lowercase().contains(&t)
            });
            in_title || in_body || in_activity
        };
        if !matches {
            return false;
        }
    }
    true
}

/// Build the flat navigation list from pure State. This is the testable core.
pub(crate) fn nav_items_from_state(state: &crate::state::State) -> Vec<NavItem> {
    let groups = build_repo_groups_from_state(state);
    let mut items = Vec::new();
    for (repo, session_keys) in &groups {
        items.push(NavItem::Repo(repo.clone()));
        if !state.collapsed_repos.contains(repo) {
            for key in session_keys {
                items.push(NavItem::Session(key.clone()));
            }
        }
    }
    items
}

/// App-based convenience that delegates to the State-based version.
pub(crate) fn nav_items(app: &App) -> Vec<NavItem> {
    nav_items_from_state(&app.state)
}

/// Get the currently selected session key (if cursor is on a session).
pub(crate) fn selected_session_from_nav(app: &App) -> Option<String> {
    let items = nav_items(app);
    match items.get(app.state.selected) {
        Some(NavItem::Session(key)) => Some(key.clone()),
        _ => None,
    }
}

/// Same as `selected_nav_item` but takes `State` directly — used by reduce.
pub(crate) fn selected_nav_item_from_state(state: &crate::state::State) -> Option<NavItem> {
    nav_items_from_state(state).get(state.selected).cloned()
}

/// Build repo → session_keys grouping. Pure (reads from State only).
pub(crate) fn build_repo_groups_from_state(
    state: &crate::state::State,
) -> Vec<(String, Vec<String>)> {
    // Used by both render-time callers and reduce. Reads `Utc::now()` at
    // this boundary only — the pure `VisibilityCtx` carries it inward.
    let now = Utc::now();
    let ctx = VisibilityCtx {
        username: &state.username,
        hide_approved_by_me: state.config.display.hide_approved_by_me,
        activity_cutoff: if state.activity_days_filter > 0 {
            Some(now - chrono::Duration::days(state.activity_days_filter as i64))
        } else {
            None
        },
        now,
    };
    build_repo_groups_inner(state, &ctx)
}

/// App-based convenience.
pub(crate) fn build_repo_groups(app: &App) -> Vec<(String, Vec<String>)> {
    build_repo_groups_from_state(&app.state)
}

fn build_repo_groups_inner(
    state: &crate::state::State,
    ctx: &VisibilityCtx,
) -> Vec<(String, Vec<String>)> {
    let mut repos: Vec<(String, Vec<String>)> = Vec::new();
    let mut repo_map: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    let order = state.sessions.order().to_vec();
    let keys = state.filtered_keys.as_ref().unwrap_or(&order);
    let mut seen_keys = std::collections::HashSet::new();
    for key in keys {
        if !seen_keys.insert(key.clone()) { continue; }
        let Some(session) = state.sessions.get(key) else { continue };
        if !is_session_visible(session, ctx) { continue }
        let repo = session.repo.clone();
        if let Some(&idx) = repo_map.get(&repo) {
            repos[idx].1.push(key.clone());
        } else {
            repo_map.insert(repo.clone(), repos.len());
            repos.push((repo, vec![key.clone()]));
        }
    }

    // Remove repos with no visible sessions.
    repos.retain(|(_, sessions)| !sessions.is_empty());

    // Sort repos by most recently updated session.
    repos.sort_by(|a, b| {
        let latest_a = a.1.iter()
            .filter_map(|k| state.sessions.get(k))
            .map(|s| s.primary_task.updated_at)
            .max();
        let latest_b = b.1.iter()
            .filter_map(|k| state.sessions.get(k))
            .map(|s| s.primary_task.updated_at)
            .max();
        latest_b.cmp(&latest_a)
    });

    repos
}

/// Re-sort session_order by priority, preserving selected key.
pub(crate) fn resort_sessions(app: &mut App) {
    let prev_key = app.selected_session_key();

    app.state.sessions.sort_by_updated();

    // Restore selection in nav_items (not session_order — they differ!).
    if let Some(ref key) = prev_key {
        let items = nav_items(app);
        if let Some(idx) = items.iter().position(|i| matches!(i, NavItem::Session(k) if k == key)) {
            app.state.selected = idx;
        }
    }
    clamp_selected(app);
}

/// Keep `app.state.selected` in range of `nav_items`. Call after any mutation that
/// may have shortened the visible list (filter change, session removal, repo
/// collapse, etc.). This is the single source of truth for clamping — prefer
/// it to ad-hoc `app.state.selected = nav_count - 1` patterns.
pub(crate) fn clamp_selected(app: &mut App) {
    let nav_count = nav_items(app).len();
    if nav_count == 0 {
        app.state.selected = 0;
    } else if app.state.selected >= nav_count {
        app.state.selected = nav_count - 1;
    }
}

/// Handle a click on the sidebar tree. Row is relative to the inbox pane inner area.
pub(crate) fn handle_sidebar_click(app: &mut App, row: usize, action_tx: &mpsc::UnboundedSender<Action>) {
    // Build the same tree structure the UI renders, mapping rows to actions.
    let mut current_row = 1usize; // skip border
    let repos = build_repo_groups(app);

    for (repo, session_keys) in &repos {
        if current_row == row {
            // Clicked on repo header — toggle collapse.
            let _ = action_tx.send(Action::ToggleRepo(repo.clone()));
            return;
        }
        current_row += 1;

        if app.state.collapsed_repos.contains(repo) {
            continue;
        }

        for key in session_keys {
            if current_row == row {
                // Clicked on session — select it using nav index.
                let nav = nav_items(app);
                if let Some(idx) = nav.iter().position(|i| matches!(i, NavItem::Session(k) if k == key)) {
                    app.state.selected = idx;
                    crate::app::reset_detail_state(app);
                }
                return;
            }
            current_row += 1;

            if !app.state.collapsed_sessions.contains(key)
                && let Some(session) = app.state.sessions.get(key) {
                    // Count visible activity lines (up to 3).
                    let msg_count = session.activity.len().min(3);
                    for _i in 0..msg_count {
                        if current_row == row {
                            // Clicked on a message — select the session and toggle messages.
                            if let Some(idx) = app.state.sessions.order().iter().position(|k2| k2 == key) {
                                app.state.selected = idx;
                                crate::app::reset_detail_state(app);
                            }
                            return;
                        }
                        current_row += 1;
                    }
                }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilot_core::{
        Activity, ActivityKind, CiStatus, ReviewStatus, Session, Task, TaskId, TaskRole, TaskState,
    };
    use std::collections::HashSet;

    fn base_task() -> Task {
        Task {
            id: TaskId { source: "github".into(), key: "o/r#1".into() },
            title: "Implement feature".into(),
            body: Some("Body text with keyword foo".into()),
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "https://github.com/o/r/pull/1".into(),
            repo: Some("o/r".into()),
            branch: Some("f".into()),
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

    fn ctx() -> VisibilityCtx<'static> {
        VisibilityCtx {
            username: "me",
            hide_approved_by_me: true,
            activity_cutoff: None,
            now: Utc::now(),
        }
    }

    #[test]
    fn visibility_hides_merged_and_closed() {
        let mut s = Session::new_at(base_task(), chrono::Utc::now());
        s.primary_task.state = TaskState::Merged;
        assert!(!is_session_visible(&s, &ctx()));
        s.primary_task.state = TaskState::Closed;
        assert!(!is_session_visible(&s, &ctx()));
    }

    #[test]
    fn visibility_hides_snoozed() {
        let mut s = Session::new_at(base_task(), chrono::Utc::now());
        s.snoozed_until = Some(Utc::now() + chrono::Duration::hours(1));
        assert!(!is_session_visible(&s, &ctx()));
    }

    #[test]
    fn visibility_hides_approved_by_me_when_configured() {
        let mut s = Session::new_at(base_task(), chrono::Utc::now());
        s.primary_task.role = TaskRole::Mentioned;
        // No unread, mentioned → hidden when hide_approved_by_me=true.
        assert!(!is_session_visible(&s, &ctx()));
        // Flip config → visible again.
        let c = VisibilityCtx {
            username: "me",
            hide_approved_by_me: false,
            activity_cutoff: None,
            now: Utc::now(),
        };
        // Still mentioned + waiting-on-others → hidden by the other rule.
        assert!(!is_session_visible(&s, &c));
    }

    #[test]
    fn visibility_respects_time_cutoff() {
        let mut s = Session::new_at(base_task(), chrono::Utc::now());
        s.primary_task.updated_at = Utc::now() - chrono::Duration::days(30);
        let c = VisibilityCtx {
            username: "me",
            hide_approved_by_me: false,
            activity_cutoff: Some(Utc::now() - chrono::Duration::days(7)),
            now: Utc::now(),
        };
        assert!(!is_session_visible(&s, &c));
    }

    #[test]
    fn visibility_shows_open_author_pr() {
        let s = Session::new_at(base_task(), chrono::Utc::now());
        assert!(is_session_visible(&s, &ctx()));
    }

    fn make_activity(author: &str, body: &str) -> Activity {
        Activity {
            author: author.into(), body: body.into(),
            created_at: Utc::now(), kind: ActivityKind::Comment,
            node_id: None, path: None, line: None, diff_hunk: None, thread_id: None,
        }
    }

    #[test]
    fn query_free_text_matches_title_body_activity() {
        let mut s = Session::new_at(base_task(), chrono::Utc::now());
        assert!(session_matches_query(&s, "feature"));   // title
        assert!(session_matches_query(&s, "foo"));       // body
        s.activity.push(make_activity("alice", "hello"));
        assert!(session_matches_query(&s, "alice"));     // activity author
        assert!(session_matches_query(&s, "hello"));     // activity body
        assert!(!session_matches_query(&s, "nonexistent"));
    }

    #[test]
    fn query_smart_filters() {
        let mut s = Session::new_at(base_task(), chrono::Utc::now());
        s.primary_task.needs_reply = true;
        assert!(session_matches_query(&s, "needs:reply"));
        s.primary_task.ci = CiStatus::Failure;
        assert!(session_matches_query(&s, "ci:failed"));
        assert!(!session_matches_query(&s, "ci:passed"));
        s.primary_task.has_conflicts = true;
        assert!(session_matches_query(&s, "is:conflict"));
        assert!(session_matches_query(&s, "role:author"));
        assert!(!session_matches_query(&s, "role:reviewer"));
        assert!(session_matches_query(&s, "repo:o/r"));
    }

    #[test]
    fn query_all_tokens_must_match() {
        let mut s = Session::new_at(base_task(), chrono::Utc::now());
        s.primary_task.ci = CiStatus::Failure;
        // Title matches "feature" AND ci matches — both required, both pass.
        assert!(session_matches_query(&s, "feature ci:failed"));
        // One token fails → no match.
        assert!(!session_matches_query(&s, "feature ci:passed"));
    }

    #[test]
    fn query_unread_filters() {
        let mut s = Session::new_at(base_task(), chrono::Utc::now());
        s.push_activity(make_activity("a", "hi"));
        assert!(session_matches_query(&s, "is:unread"));
        assert!(!session_matches_query(&s, "is:read"));
        s.mark_read(Utc::now());
        assert!(session_matches_query(&s, "is:read"));
    }

    #[test]
    fn query_empty_matches_everything() {
        let s = Session::new_at(base_task(), chrono::Utc::now());
        assert!(session_matches_query(&s, ""));
    }

    // ── Silence unused imports in tests — these are used transitively. ──
    #[allow(dead_code)]
    fn _assertions() {
        let _: HashSet<usize> = HashSet::new();
    }
}
