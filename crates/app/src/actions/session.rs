use tokio::sync::mpsc;

use crate::action::{Action, ShellKind};
use crate::app::{App, determine_mode, spawn_terminal};
use crate::input::{InputMode, TextInputKind};
use pilot_core::SessionState;
use pilot_git_ops::WorktreeManager;

pub(crate) fn handle_new_session(app: &mut App) {
    app.new_session_input = Some(String::new());
    app.input_mode = InputMode::TextInput(TextInputKind::NewSession);
    app.status = "New session — type description, Enter to create, Esc to cancel".into();
}

pub(crate) fn handle_new_session_cancel(app: &mut App) {
    app.new_session_input = None;
    if matches!(app.input_mode, InputMode::TextInput(TextInputKind::NewSession)) {
        app.input_mode = determine_mode(app);
    }
    app.status = String::new();
}

pub(crate) fn handle_new_session_confirm(app: &mut App, description: String) {
    app.new_session_input = None;
    if matches!(app.input_mode, InputMode::TextInput(TextInputKind::NewSession)) {
        app.input_mode = determine_mode(app);
    }
    let key = format!("local:{}", chrono::Utc::now().timestamp_millis());
    let task = pilot_core::Task {
        id: pilot_core::TaskId { source: "local".into(), key: key.clone() },
        title: description.clone(),
        body: None,
        state: pilot_core::TaskState::Open,
        role: pilot_core::TaskRole::Author,
        ci: pilot_core::CiStatus::None,
        review: pilot_core::ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: String::new(),
        repo: None,
        branch: None,
        updated_at: chrono::Utc::now(),
        labels: vec![],
        reviewers: vec![],
        assignees: vec![],
        in_merge_queue: false,
        has_conflicts: false,
        needs_reply: false,
        last_commenter: None,
        recent_activity: vec![],
        additions: 0,
        deletions: 0,
    };
    let mut session = pilot_core::Session::new(task);
    session.state = pilot_core::SessionState::Active;
    app.sessions.insert(key, session);
    crate::nav::resort_sessions(app);
    app.status = format!("Created session: {description}");
}

pub(crate) fn handle_open_session(app: &mut App, shell_kind: ShellKind, action_tx: &mpsc::UnboundedSender<Action>) {
    if let Some(key) = app.selected_session_key() {
        if app.terminals.contains_key(&key) {
            // Already has terminal — just switch to that tab.
            if let Some(idx) = app.terminals.tab_order().iter().position(|k| k == &key) {
                app.terminals.set_active_tab(idx);
            }
            set_mode(app, InputMode::Terminal);
            return;
        }

        let worktree_path = app
            .sessions
            .get(&key)
            .and_then(|s| s.worktree_path.clone());
        if let Some(path) = worktree_path {
            spawn_terminal(app, &key, path, shell_kind);
            return;
        }

        if let Some(session) = app.sessions.get_mut(&key) {
            let repo = session.primary_task.repo.clone();
            let branch = session.primary_task.branch.clone();
            session.state = SessionState::CheckingOut;
            app.status = format!("Checking out worktree for {}…", session.display_name);

            if let (Some(repo_full), Some(branch)) = (repo, branch) {
                let parts: Vec<&str> = repo_full.splitn(2, '/').collect();
                if parts.len() == 2 {
                    let owner = parts[0].to_string();
                    let repo = parts[1].to_string();
                    let tx = action_tx.clone();
                    let session_key = key.clone();
                    let worktrees = WorktreeManager::default_base();

                    tokio::spawn(async move {
                        match worktrees.checkout(&owner, &repo, &branch).await {
                            Ok(wt) => {
                                let _ = tx.send(Action::WorktreeReady {
                                    session_key: session_key.clone(),
                                    path: wt.path,
                                });
                                let _ = tx.send(Action::OpenSession(shell_kind));
                            }
                            Err(e) => {
                                tracing::error!("Worktree checkout failed: {e}");
                            }
                        }
                    });
                }
            } else {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                spawn_terminal(app, &key, home.into(), shell_kind);
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
