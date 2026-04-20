use tokio::sync::mpsc;

use crate::action::Action;
use crate::app::{App, determine_mode};
use crate::input::{InputMode, TextInputKind};
use crate::nav::resort_sessions;

pub(crate) fn handle_merge(app: &mut App, action_tx: &mpsc::UnboundedSender<Action>) {
    if let Some(key) = app.selected_session_key() {
        // Extract values before borrowing mutably.
        let pr_info = app.sessions.get(&key).map(|session| {
            let task = &session.primary_task;
            let repo = task.repo.clone().unwrap_or_default();
            let pr_num = task.id.key.rsplit_once('#')
                .map(|(_, n)| n.to_string())
                .unwrap_or_default();
            let review = format!("{:?}", task.review);
            (repo, pr_num, review)
        });

        if let Some((repo, pr_num, review)) = pr_info {
            if repo.is_empty() || pr_num.is_empty() {
                app.status = "Cannot merge: no PR info".into();
                return;
            }

            if app.merge_pending.as_deref() == Some(key.as_str()) {
                // Second M — execute merge.
                app.merge_pending = None;
                app.status = format!("Merging {repo}#{pr_num}…");
                set_mode(app, InputMode::Normal);
                // Optimistic update — mark as merged immediately so the
                // nav filter hides it. If the merge fails, the next poll
                // will correct the state back to Open.
                if let Some(session) = app.sessions.get_mut(&key) {
                    session.primary_task.state = pilot_core::TaskState::Merged;
                }
                resort_sessions(app);
                let repo = repo.to_string();
                let pr = pr_num.clone();
                let session_key = key.clone();
                let tx = action_tx.clone();
                tokio::spawn(async move {
                    let output = tokio::process::Command::new("gh")
                        .args(["pr", "merge", &pr, "--squash", "--repo", &repo])
                        .output()
                        .await;
                    match output {
                        Ok(o) if o.status.success() => {
                            tracing::info!("Merged {repo}#{pr}");
                            let _ = tx.send(Action::MergeCompleted {
                                session_key,
                            });
                            let _ = tx.send(Action::StatusMessage(
                                format!("Merged {repo}#{pr}"),
                            ));
                        }
                        Ok(o) => {
                            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                            tracing::error!("Merge failed: {err}");
                            let _ = tx.send(Action::StatusMessage(
                                format!("Error: merge failed — {err}"),
                            ));
                        }
                        Err(e) => {
                            tracing::error!("Merge error: {e}");
                            let _ = tx.send(Action::StatusMessage(
                                format!("Error: {e}"),
                            ));
                        }
                    }
                });
            } else {
                app.merge_pending = Some(key.clone());
                app.status = format!(
                    "Merge? {repo}#{pr_num} (review: {review}). Press M again to confirm."
                );
            }
        }
    }
}

pub(crate) fn handle_merge_completed(app: &mut App, session_key: &str) {
    // Confirm the merged state and remove from store so it doesn't
    // reappear on next launch.
    if let Some(session) = app.sessions.get_mut(session_key) {
        session.primary_task.state = pilot_core::TaskState::Merged;
        let task_id = session.primary_task.id.clone();
        let _ = app.store.delete_session(&task_id);
    }
    resort_sessions(app);
}

pub(crate) fn handle_open_in_browser(app: &mut App) {
    if let Some(key) = app.selected_session_key() {
        if let Some(session) = app.sessions.get(&key) {
            let url = &session.primary_task.url;
            if !url.is_empty() {
                crate::notify::open_url(url);
                app.status = "Opened in browser".into();
            } else {
                app.status = "No URL for this session".into();
            }
        }
    }
}

pub(crate) fn handle_slack_nudge(app: &mut App, action_tx: &mpsc::UnboundedSender<Action>) {
    let webhook_url = app.config.slack.webhook_url.clone();
    if webhook_url.is_none() {
        app.status = "No Slack webhook configured — set slack.webhook_url in ~/.pilot/config.yaml".into();
        return;
    }
    let webhook_url = webhook_url.unwrap();

    if let Some(key) = app.selected_session_key() {
        if let Some(session) = app.sessions.get(&key) {
            let task = &session.primary_task;
            let reviewers = &task.reviewers;
            if reviewers.is_empty() {
                app.status = "No reviewers to nudge".into();
                return;
            }

            let reviewer_list = reviewers.join(", ");
            let title = task.title.clone();
            let url = task.url.clone();
            let text = format!(
                "Friendly reminder: *{title}* is waiting for review.\n<{url}|View PR>\nReviewers: {reviewer_list}"
            );
            app.status = format!("Sending Slack nudge to {reviewer_list}…");

            let _tx = action_tx.clone();
            tokio::spawn(async move {
                let payload = serde_json::json!({ "text": text });
                let output = tokio::process::Command::new("curl")
                    .args([
                        "-s", "-X", "POST",
                        "-H", "Content-Type: application/json",
                        "-d", &serde_json::to_string(&payload).unwrap(),
                        &webhook_url,
                    ])
                    .output()
                    .await;
                match output {
                    Ok(o) if o.status.success() => {
                        tracing::info!("Slack nudge sent for {title}");
                    }
                    Ok(o) => {
                        tracing::error!("Slack nudge failed: {}", String::from_utf8_lossy(&o.stderr));
                    }
                    Err(e) => {
                        tracing::error!("Slack nudge error: {e}");
                    }
                }
            });
        }
    }
}

pub(crate) fn handle_snooze(app: &mut App) {
    if let Some(key) = app.selected_session_key() {
        if let Some(session) = app.sessions.get_mut(&key) {
            if session.is_snoozed() {
                session.snoozed_until = None;
                app.status = format!("Unsnoozed: {}", session.display_name);
            } else {
                session.snoozed_until = Some(chrono::Utc::now() + chrono::Duration::hours(4));
                app.status = format!("Snoozed for 4h: {}", session.display_name);
            }
        }
    }
}

pub(crate) fn handle_quick_reply(app: &mut App) {
    if let Some(key) = app.selected_session_key() {
        let cursor = app.detail_cursor;
        app.quick_reply_input = Some((key, String::new(), cursor));
        app.input_mode = InputMode::TextInput(TextInputKind::QuickReply);
        app.status = "Quick reply — type message, Enter to post, Esc to cancel".into();
    }
}

pub(crate) fn handle_quick_reply_cancel(app: &mut App) {
    app.quick_reply_input = None;
    if matches!(app.input_mode, InputMode::TextInput(TextInputKind::QuickReply)) {
        app.input_mode = determine_mode(app);
    }
    app.status = String::new();
}

pub(crate) fn handle_quick_reply_confirm(app: &mut App, body: String, action_tx: &mpsc::UnboundedSender<Action>) {
    if matches!(app.input_mode, InputMode::TextInput(TextInputKind::QuickReply)) {
        app.input_mode = determine_mode(app);
    }
    if let Some((session_key, _, comment_idx)) = app.quick_reply_input.take() {
        if let Some(session) = app.sessions.get(&session_key) {
            let repo = session.primary_task.repo.clone().unwrap_or_default();
            let pr_number = session.primary_task.id.key.rsplit_once('#')
                .map(|(_, n)| n.to_string())
                .unwrap_or_default();
            let reply_to = session.activity.get(comment_idx)
                .and_then(|a| a.node_id.clone());
            if !repo.is_empty() && !pr_number.is_empty() && !body.trim().is_empty() {
                app.status = "Posting reply…".into();
                let tx = action_tx.clone();
                tokio::spawn(async move {
                    let mut args = vec![
                        "pr".to_string(), "comment".to_string(),
                        pr_number, "--body".to_string(), body,
                        "--repo".to_string(), repo,
                    ];
                    if let Some(node_id) = reply_to {
                        args.push("--reply-to".to_string());
                        args.push(node_id);
                    }
                    let output = tokio::process::Command::new("gh")
                        .args(&args)
                        .output()
                        .await;
                    match output {
                        Ok(o) if o.status.success() => {
                            let _ = tx.send(Action::StatusMessage("Reply posted".into()));
                        }
                        Ok(o) => {
                            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                            let _ = tx.send(Action::StatusMessage(format!("Error: {err}")));
                        }
                        Err(e) => {
                            let _ = tx.send(Action::StatusMessage(format!("Error: {e}")));
                        }
                    }
                });
            }
        }
    }
}

/// Re-export set_mode from app for use in this module.
fn set_mode(app: &mut App, mode: InputMode) {
    if !app.input_mode.is_overlay() {
        app.input_mode = mode;
    }
}
