use tokio::sync::mpsc;

use crate::action::Action;
use crate::app::App;

/// An item in the sidebar navigation list — either a repo header or a session.
#[derive(Debug, Clone)]
pub enum NavItem {
    Repo(String),
    Session(String), // session key
}

/// Build the flat navigation list (repo headers + sessions, respecting collapse).
pub(crate) fn nav_items(app: &App) -> Vec<NavItem> {
    let groups = build_repo_groups(app);
    let mut items = Vec::new();
    for (repo, session_keys) in &groups {
        items.push(NavItem::Repo(repo.clone()));
        if !app.collapsed_repos.contains(repo) {
            for key in session_keys {
                items.push(NavItem::Session(key.clone()));
            }
        }
    }
    items
}

/// Get the currently selected session key (if cursor is on a session).
pub(crate) fn selected_session_from_nav(app: &App) -> Option<String> {
    let items = nav_items(app);
    match items.get(app.selected) {
        Some(NavItem::Session(key)) => Some(key.clone()),
        _ => None,
    }
}

/// Get the currently selected nav item.
pub(crate) fn selected_nav_item(app: &App) -> Option<NavItem> {
    nav_items(app).get(app.selected).cloned()
}

/// Build repo → session_keys grouping for the sidebar tree.
/// Uses filtered_keys if a search filter is active.
pub(crate) fn build_repo_groups(app: &App) -> Vec<(String, Vec<String>)> {
    let mut repos: Vec<(String, Vec<String>)> = Vec::new();
    let mut repo_map: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    let cutoff = if app.activity_days_filter > 0 {
        Some(chrono::Utc::now() - chrono::Duration::days(app.activity_days_filter as i64))
    } else {
        None
    };

    let keys = app.filtered_keys.as_ref().unwrap_or(&app.session_order);
    for key in keys {
        if let Some(session) = app.sessions.get(key) {
            // Apply time filter.
            if let Some(cutoff) = &cutoff {
                if session.primary_task.updated_at < *cutoff {
                    continue;
                }
            }

            // Hide merged and closed PRs.
            if matches!(
                session.primary_task.state,
                pilot_core::TaskState::Merged | pilot_core::TaskState::Closed
            ) {
                continue;
            }

            // Hide snoozed sessions.
            if session.is_snoozed() {
                continue;
            }

            // Hide sessions where I've done my part (approved as reviewer, all read).
            let priority = session.action_priority(&app.username);
            if session.primary_task.role == pilot_core::TaskRole::Mentioned
                && session.unread_count() == 0
                && matches!(priority, pilot_core::ActionPriority::WaitingOnOthers | pilot_core::ActionPriority::Stale)
            {
                continue;
            }

            let repo = session.repo.clone();
            if let Some(&idx) = repo_map.get(&repo) {
                repos[idx].1.push(key.clone());
            } else {
                repo_map.insert(repo.clone(), repos.len());
                repos.push((repo, vec![key.clone()]));
            }
        }
    }

    // Remove repos with no visible sessions.
    repos.retain(|(_, sessions)| !sessions.is_empty());

    // Sort repos by most recently updated session.
    repos.sort_by(|a, b| {
        let latest_a = a.1.iter()
            .filter_map(|k| app.sessions.get(k))
            .map(|s| s.primary_task.updated_at)
            .max();
        let latest_b = b.1.iter()
            .filter_map(|k| app.sessions.get(k))
            .map(|s| s.primary_task.updated_at)
            .max();
        latest_b.cmp(&latest_a)
    });

    repos
}

/// Re-sort session_order by priority, preserving selected key.
pub(crate) fn resort_sessions(app: &mut App) {
    let prev_key = app.selected_session_key();

    app.session_order.sort_by(|a, b| {
        let sa = app.sessions.get(a);
        let sb = app.sessions.get(b);
        match (sa, sb) {
            (Some(sa), Some(sb)) => sb.primary_task.updated_at.cmp(&sa.primary_task.updated_at),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });

    // Restore selection in nav_items (not session_order — they differ!).
    if let Some(ref key) = prev_key {
        let items = nav_items(app);
        if let Some(idx) = items.iter().position(|i| matches!(i, NavItem::Session(k) if k == key)) {
            app.selected = idx;
        }
    }
    // Clamp.
    let nav_count = nav_items(app).len();
    if app.selected >= nav_count && nav_count > 0 {
        app.selected = nav_count - 1;
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

        if app.collapsed_repos.contains(repo) {
            continue;
        }

        for key in session_keys {
            if current_row == row {
                // Clicked on session — select it using nav index.
                let nav = nav_items(app);
                if let Some(idx) = nav.iter().position(|i| matches!(i, NavItem::Session(k) if k == key)) {
                    app.selected = idx;
                    app.detail_scroll = 0;
                    crate::app::update_detail_pane(app);
                }
                return;
            }
            current_row += 1;

            if !app.collapsed_sessions.contains(key) {
                if let Some(session) = app.sessions.get(key) {
                    // Count visible activity lines (up to 3).
                    let msg_count = session.activity.len().min(3);
                    for _i in 0..msg_count {
                        if current_row == row {
                            // Clicked on a message — select the session and toggle messages.
                            if let Some(idx) = app.session_order.iter().position(|k2| k2 == key) {
                                app.selected = idx;
                                crate::app::update_detail_pane(app);
                            }
                            return;
                        }
                        current_row += 1;
                    }
                }
            }
        }
    }
}

/// Apply search filter to session list. Supports smart filters:
/// - Free text: matches title, body, author
/// - `needs:reply` / `needs:review`
/// - `ci:failed` / `ci:passed`
/// - `is:unread` / `is:read`
/// - `role:author` / `role:reviewer`
pub(crate) fn apply_search_filter(app: &mut App) {
    let query = app.search_query.trim().to_lowercase();
    if query.is_empty() {
        app.filtered_keys = None;
        return;
    }

    let filtered: Vec<String> = app
        .session_order
        .iter()
        .filter(|key| {
            let Some(session) = app.sessions.get(*key) else {
                return false;
            };
            let task = &session.primary_task;

            for token in query.split_whitespace() {
                let matches = if let Some(val) = token.strip_prefix("needs:") {
                    match val {
                        "reply" => task.needs_reply,
                        "review" => task.role == pilot_core::TaskRole::Reviewer,
                        _ => true,
                    }
                } else if let Some(val) = token.strip_prefix("ci:") {
                    match val {
                        "failed" | "fail" => task.ci == pilot_core::CiStatus::Failure,
                        "passed" | "pass" | "ok" => task.ci == pilot_core::CiStatus::Success,
                        "pending" => {
                            task.ci == pilot_core::CiStatus::Pending
                                || task.ci == pilot_core::CiStatus::Running
                        }
                        _ => true,
                    }
                } else if let Some(val) = token.strip_prefix("is:") {
                    match val {
                        "unread" => session.unread_count() > 0,
                        "read" => session.unread_count() == 0,
                        "open" => task.state == pilot_core::TaskState::Open,
                        "draft" => task.state == pilot_core::TaskState::Draft,
                        "merged" => task.state == pilot_core::TaskState::Merged,
                        _ => true,
                    }
                } else if let Some(val) = token.strip_prefix("role:") {
                    match val {
                        "author" => task.role == pilot_core::TaskRole::Author,
                        "reviewer" | "review" => task.role == pilot_core::TaskRole::Reviewer,
                        "assignee" => task.role == pilot_core::TaskRole::Assignee,
                        _ => true,
                    }
                } else if let Some(val) = token.strip_prefix("repo:") {
                    session.repo.to_lowercase().contains(val)
                } else {
                    // Free text: match title, body, or activity authors
                    let in_title = task.title.to_lowercase().contains(token);
                    let in_body = task
                        .body
                        .as_ref()
                        .map(|b| b.to_lowercase().contains(token))
                        .unwrap_or(false);
                    let in_activity = session
                        .activity
                        .iter()
                        .any(|a| a.author.to_lowercase().contains(token) || a.body.to_lowercase().contains(token));
                    in_title || in_body || in_activity
                };

                if !matches {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect();

    app.filtered_keys = Some(filtered);
    app.selected = 0;
}
