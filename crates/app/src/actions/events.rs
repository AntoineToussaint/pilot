use tokio::sync::mpsc;

use crate::action::Action;
use crate::app::{App, update_detail_pane};
use crate::nav::resort_sessions;
use pilot_events::{Event, EventKind};

pub(crate) fn handle_external_event(app: &mut App, event: Event, action_tx: &mpsc::UnboundedSender<Action>) {
    let summary = event.summary();
    app.notifications.insert(0, summary);
    if app.notifications.len() > 100 {
        app.notifications.truncate(100);
    }

    match event.kind {
        EventKind::TaskUpdated(task) => {
            // Track which sessions the provider knows about.

            if !app.loaded {
                app.loaded = true;
                app.selected = 0;
                update_detail_pane(app);
                app.status = format!(
                    "Loaded — {} as {}",
                    app.config.providers.github.filters.iter()
                        .filter_map(|f| f.org.as_ref())
                        .next()
                        .unwrap_or(&"all repos".to_string()),
                    app.username
                );
            }
            let key = task.id.to_string();
            let persist_key = key.clone();
            if let Some(session) = app.sessions.get_mut(&key) {
                let existing_count = session.activity.len();
                let fresh_count = task.recent_activity.len();
                if fresh_count > existing_count {
                    for a in task.recent_activity.iter().take(fresh_count - existing_count) {
                        session.push_activity(a.clone());
                    }
                }
                session.primary_task = task;
            } else {
                let mut session = pilot_core::Session::new(task.clone());
                for activity in &task.recent_activity {
                    session.push_activity(activity.clone());
                }
                if let Ok(Some(record)) = app.store.get_session(&task.id) {
                    session.seen_count = record.seen_count as usize;
                    session.last_viewed_at = record.last_viewed_at;
                }
                app.sessions.insert(key, session);
            }
            // Persist full session to SQLite.
            if let Some(session) = app.sessions.get(&persist_key) {
                let json = serde_json::to_string(session).ok();
                if let Err(e) = app.store.save_session(&pilot_store::SessionRecord {
                    task_id: persist_key.clone(),
                    seen_count: session.seen_count as i64,
                    last_viewed_at: session.last_viewed_at,
                    created_at: session.created_at,
                    session_json: json,
                    metadata: None,
                }) {
                    tracing::error!("Failed to save session {persist_key}: {e}");
                }
            }
            resort_sessions(app);
        }
        EventKind::NewActivity {
            ref task_id,
            ref activity,
        } => {
            let key = task_id.to_string();
            if let Some(session) = app.sessions.get_mut(&key) {
                session.push_activity(activity.clone());
            }
            // Notify on new comment needing reply (only after initial load).
            if app.loaded {
                if let Some(session) = app.sessions.get(&key) {
                    if session.primary_task.needs_reply
                        && session.primary_task.role == pilot_core::TaskRole::Author
                    {
                        let title = session.display_name.clone();
                        let author = activity.author.clone();
                        tokio::spawn(async move {
                            crate::notify::send_notification(
                                &format!("{author} commented on {title}"),
                                "You may need to reply",
                            )
                            .await;
                        });
                    }
                }
            }
            resort_sessions(app);
        }
        EventKind::TaskStateChanged {
            ref task_id, new, ..
        } => {
            let key = task_id.to_string();
            if let Some(session) = app.sessions.get_mut(&key) {
                session.primary_task.state = new;
            }
            resort_sessions(app);
        }
        EventKind::CiStatusChanged {
            ref task_id, new, ..
        } => {
            let key = task_id.to_string();
            if let Some(session) = app.sessions.get_mut(&key) {
                session.primary_task.ci = new;
                // Drive the monitor state machine on CI changes.
                if session.monitor.is_some() {
                    let _ = action_tx.send(Action::MonitorTick { session_key: key.clone() });
                }
            }
            // Notify on CI failure for authored PRs (only after initial load).
            if app.loaded && new == pilot_core::CiStatus::Failure {
                if let Some(session) = app.sessions.get(&key) {
                    if session.primary_task.role == pilot_core::TaskRole::Author {
                        let title = session.display_name.clone();
                        tokio::spawn(async move {
                            crate::notify::send_notification(
                                &format!("CI failed: {title}"),
                                "A CI check failed on your PR",
                            )
                            .await;
                        });
                    }
                }
            }
            resort_sessions(app);
        }
        EventKind::ReviewStatusChanged {
            ref task_id, new, ..
        } => {
            let key = task_id.to_string();
            if let Some(session) = app.sessions.get_mut(&key) {
                session.primary_task.review = new;
            }
            // Notify on PR approval for authored PRs (only after initial load).
            if app.loaded && new == pilot_core::ReviewStatus::Approved {
                if let Some(session) = app.sessions.get(&key) {
                    if session.primary_task.role == pilot_core::TaskRole::Author {
                        let title = session.display_name.clone();
                        tokio::spawn(async move {
                            crate::notify::send_notification(
                                &format!("Approved: {title}"),
                                "Your PR was approved! Ready to merge.",
                            )
                            .await;
                        });
                    }
                }
            }
            resort_sessions(app);
        }
        EventKind::TaskRemoved(ref id) => {
            let key = id.to_string();
            app.sessions.remove(&key);
            app.close_terminal(&key);
            let order_len = app.sessions.order().len();
            if app.selected >= order_len && order_len > 0 {
                app.selected = order_len - 1;
            }
            // Also remove from persistent store so it doesn't come back on restart.
            if let Err(e) = app.store.delete_session(id) {
                tracing::error!("Failed to delete session {id}: {e}");
            }
        }
        EventKind::ProviderError { ref message } => {
            tracing::warn!("Provider error: {message}");
            // Don't show transient errors in status bar — they clear on next poll.
            // Persistent errors will keep firing and user can check /tmp/pilot.log.
        }
    }
}
