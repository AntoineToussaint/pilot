use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self as ct_event, Event as CtEvent, EventStream};
use futures::StreamExt;
use pilot_auth::{CommandProvider, CredentialChain, EnvProvider};
use pilot_config::Config;
use pilot_core::{Session, SessionState};
use pilot_events::{event_bus, EventKind, EventProducer};
use pilot_gh::{GhClient, GhPoller};
use pilot_git_ops::WorktreeManager;
use pilot_store::{SqliteStore, Store};
use pilot_tui_term::{PtySize, TermSession};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::action::{Action, ShellKind};
use crate::keys::{self, KeyMode};
use crate::monitor::{check_needs_rebase, handle_monitor_tick, run_rebase};
use crate::nav::{
    apply_search_filter, nav_items, resort_sessions, selected_nav_item,
    selected_session_from_nav, handle_sidebar_click, NavItem,
};
use crate::pane::{Direction, PaneContent, PaneManager};
use crate::picker::{build_picker_items, fetch_collaborators, PickerState};
use crate::ui;

/// Tracks an active mouse drag for pane resizing.
/// Tracks mouse drag for sidebar resize. Fields unused directly but
/// presence of Some(DragState) indicates an active drag.
#[derive(Debug, Clone)]
pub struct DragState;

/// Top-level application state.
pub struct App {
    // ── Sessions ──
    pub sessions: BTreeMap<String, Session>,
    pub terminals: BTreeMap<String, TermSession>,
    pub terminal_kinds: BTreeMap<String, ShellKind>,
    pub session_order: Vec<String>,
    pub selected: usize,

    // ── Panes ──
    pub panes: PaneManager,

    // ── Tabs ──
    pub tab_order: Vec<String>,
    pub active_tab: usize,

    // ── Input ──
    pub key_mode: KeyMode,

    // ── Search/filter ──
    pub search_active: bool,
    pub search_query: String,
    pub filtered_keys: Option<Vec<String>>,
    /// Time filter: only show sessions with activity within this many days.
    /// 0 = show all.
    pub activity_days_filter: u32,

    // ── Detail pane ──
    /// Which activity items are selected (checked) in the detail pane.
    pub selected_comments: std::collections::HashSet<usize>,
    /// Cursor position within the detail pane's comment list.
    pub detail_cursor: usize,
    /// When the current session started being viewed (for auto-mark-read).
    pub viewing_since: Option<(String, std::time::Instant)>,

    // ── UI state ──
    pub notifications: Vec<String>,
    pub detail_scroll: u16,
    pub last_term_area: (u16, u16),
    pub status: String,
    pub should_quit: bool,
    /// Whether we're waiting for quit confirmation (double-q).
    pub quit_pending: bool,
    /// Session key awaiting merge confirmation (double-M).
    pub merge_pending: Option<String>,
    /// Whether the first poll has completed.
    pub loaded: bool,
    /// Tick counter for spinner animation.
    pub tick_count: u64,
    /// Collapsed repos in the sidebar tree.
    pub collapsed_repos: std::collections::HashSet<String>,
    /// Collapsed sessions (don't show messages).
    pub collapsed_sessions: std::collections::HashSet<String>,
    /// Mouse drag state for resize.
    pub drag_resize: Option<DragState>,
    /// Pending MCP action awaiting user confirmation.
    pub pending_mcp: Option<PendingMcpAction>,
    /// Active picker overlay (reviewer/assignee editing).
    pub picker: Option<PickerState>,
    /// Cached collaborators per repo.
    pub collaborators_cache: std::collections::HashMap<String, Vec<String>>,
    /// Debounce timestamp for fix/reply Claude sends.
    pub last_claude_send: Option<std::time::Instant>,
    /// Whether the help overlay is shown.
    pub show_help: bool,
    /// Sidebar width as percentage (adjustable by mouse drag).
    pub sidebar_pct: u16,

    // ── Infrastructure ──
    pub store: Arc<dyn Store>,
    pub event_tx: EventProducer,
    pub config: Config,
    pub username: String,
    /// Session keys with active monitors — shared with MCP socket for auto-approve.
    pub monitored_sessions: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Wake handle to trigger an immediate GitHub poll.
    pub poller_wake: Option<Arc<tokio::sync::Notify>>,
    /// Sessions that have already fired a macOS notification for "asking".
    pub notified_asking: std::collections::HashSet<String>,
    /// Pending prompts to inject when Claude becomes idle.
    pub pending_prompts: std::collections::HashMap<String, String>,
    /// Detected Claude state per terminal session.
    pub agent_states: std::collections::HashMap<String, crate::agent_state::AgentState>,
    /// Cached default branch per repo (e.g. "main" or "master").
    pub default_branch_cache: std::collections::HashMap<String, String>,
    /// Text input for new session description overlay.
    pub new_session_input: Option<String>,
    /// Quick reply input: (session_key, text).
    /// (session_key, text, comment_index) for quick reply.
    pub quick_reply_input: Option<(String, String, usize)>,
    /// Whether GitHub credentials resolved successfully.
    pub credentials_ok: bool,
}

impl App {
    pub async fn new(config: Config) -> Result<Self> {
        let (event_tx, _consumer) = event_bus();
        let store: Arc<dyn Store> = Arc::new(SqliteStore::default_path()?);
        tracing::info!("State database opened at ~/.pilot/state.db");

        // Load cached sessions from SQLite for instant startup.
        let mut sessions = BTreeMap::new();
        let mut session_order = Vec::new();
        let mut loaded = false;

        if let Ok(records) = store.list_sessions() {
            for record in records {
                if let Some(json) = &record.session_json {
                    if let Ok(session) = serde_json::from_str::<Session>(json) {
                        let key = record.task_id.clone();
                        session_order.push(key.clone());
                        sessions.insert(key, session);
                    }
                }
            }
            if !sessions.is_empty() {
                loaded = true;
                tracing::info!("Restored {} cached sessions from SQLite", sessions.len());
            }
        }

        Ok(Self {
            sessions,
            terminals: BTreeMap::new(),
            terminal_kinds: BTreeMap::new(),
            session_order,
            selected: 0,
            panes: PaneManager::default_layout(),
            tab_order: Vec::new(),
            active_tab: 0,
            key_mode: KeyMode::Normal,
            search_active: false,
            search_query: String::new(),
            filtered_keys: None,
            activity_days_filter: config.display.activity_days,
            selected_comments: std::collections::HashSet::new(),
            detail_cursor: 0,
            viewing_since: None,
            notifications: Vec::new(),
            store,
            event_tx,
            detail_scroll: 0,
            last_term_area: (80, 24),
            status: "Loading…".into(),
            should_quit: false,
            quit_pending: false,
            merge_pending: None,
            loaded,
            tick_count: 0,
            collapsed_repos: std::collections::HashSet::new(),
            collapsed_sessions: std::collections::HashSet::new(),
            drag_resize: None,
            pending_mcp: None,
            picker: None,
            collaborators_cache: std::collections::HashMap::new(),
            last_claude_send: None,
            show_help: false,
            sidebar_pct: 50,
            config,
            username: String::new(),
            monitored_sessions: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            poller_wake: None,
            notified_asking: std::collections::HashSet::new(),
            pending_prompts: std::collections::HashMap::new(),
            agent_states: std::collections::HashMap::new(),
            default_branch_cache: std::collections::HashMap::new(),
            new_session_input: None,
            quick_reply_input: None,
            credentials_ok: true,
        })
    }

    pub fn selected_session_key(&self) -> Option<String> {
        selected_session_from_nav(self)
    }

    pub fn active_tab_key(&self) -> Option<&String> {
        self.tab_order.get(self.active_tab)
    }
}

/// Main event loop.
pub async fn run(app: &mut App) -> Result<()> {
    let (action_tx, mut action_rx) = mpsc::unbounded_channel::<Action>();

    // 1. Crossterm event reader.
    let tx = action_tx.clone();
    tokio::spawn(async move {
        let mut stream = EventStream::new();
        while let Some(Ok(evt)) = stream.next().await {
            let action = match evt {
                CtEvent::Key(key) => Action::Key(key),
                CtEvent::Mouse(mouse) => Action::Mouse(mouse),
                CtEvent::Paste(text) => Action::Paste(text),
                CtEvent::Resize(w, h) => Action::Resize { width: w, height: h },
                _ => continue,
            };
            if tx.send(action).is_err() {
                break;
            }
        }
    });

    // 2. Tick timer.
    let tx = action_tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        loop {
            interval.tick().await;
            if tx.send(Action::Tick).is_err() {
                break;
            }
        }
    });

    // 3. GitHub provider.
    let tx = action_tx.clone();
    let github_creds = CredentialChain::new()
        .with(EnvProvider::new("GH_TOKEN"))
        .with(EnvProvider::new("GITHUB_TOKEN"))
        .with(CommandProvider::new("gh", &["auth", "token"]));

    match github_creds.resolve("github").await {
        Ok(cred) => {
            tracing::info!(source = %cred.source, "GitHub credential resolved");
            let filters: Vec<String> = app
                .config
                .providers
                .github
                .filters
                .iter()
                .filter_map(|f| f.to_search_qualifier())
                .collect();
            let poll_interval = app.config.providers.github.poll_interval;

            match GhClient::from_credential(cred).await {
                Ok(gh) => {
                    let watch_repos: Vec<String> = app
                        .config
                        .providers
                        .github
                        .filters
                        .iter()
                        .filter_map(|f| f.watch_repo().map(|r| r.to_string()))
                        .collect();
                    let gh = gh.with_filters(filters).with_watch_repos(watch_repos);
                    app.username = gh.username().to_string();
                    app.status = format!(
                        "Connected as {} ({})",
                        gh.username(),
                        gh.credential_source()
                    );
                    let mut consumer = app.event_tx.subscribe();
                    let poller = GhPoller::new(gh, app.event_tx.clone(), poll_interval);
                    app.poller_wake = Some(poller.wake_handle());
                    tokio::spawn(async move {
                        poller.run().await;
                    });
                    tokio::spawn(async move {
                        while let Some(event) = consumer.recv().await {
                            if tx.send(Action::ExternalEvent(event)).is_err() {
                                break;
                            }
                        }
                    });
                }
                Err(e) => {
                    app.status = format!("GitHub auth failed: {e}");
                    app.credentials_ok = false;
                }
            }
        }
        Err(e) => {
            app.status = format!("No GitHub credential: {e}");
            app.credentials_ok = false;
        }
    }


    // ── TUI setup ──
    // Start MCP Unix socket listener for Claude Code confirmations.
    start_mcp_socket_listener(action_tx.clone(), Arc::clone(&app.monitored_sessions));

    let mut terminal = ratatui::init();
    crossterm::execute!(
        std::io::stdout(),
        ct_event::EnableMouseCapture,
        ct_event::EnableBracketedPaste,
    )?;

    // ── Main loop ──
    loop {
        while let Ok(action) = action_rx.try_recv() {
            handle_action(app, action, &action_tx);
        }
        if app.should_quit {
            break;
        }

        terminal.draw(|frame| {
            // Keep last_term_area in sync with actual terminal size.
            app.last_term_area = (frame.area().width, frame.area().height);
            ui::render(app, frame);
        })?;

        if let Some(action) = action_rx.recv().await {
            handle_action(app, action, &action_tx);
        }
        if app.should_quit {
            break;
        }
    }

    crossterm::execute!(
        std::io::stdout(),
        ct_event::DisableMouseCapture,
        ct_event::DisableBracketedPaste,
    )?;
    ratatui::restore();
    Ok(())
}

fn handle_action(app: &mut App, action: Action, action_tx: &mpsc::UnboundedSender<Action>) {
    match action {
        Action::Quit => {
            if app.terminals.is_empty() || app.quit_pending {
                // Graceful shutdown: send Ctrl-C to all running terminals.
                for (key, term) in app.terminals.iter_mut() {
                    tracing::info!("Sending Ctrl-C to terminal {key}");
                    let _ = term.write(&[0x03]); // Ctrl-C = ETX
                }
                save_all_sessions(app);
                app.should_quit = true;
            } else {
                app.quit_pending = true;
                app.status = format!(
                    "Quit? {} terminal{} running. Press q again to confirm.",
                    app.terminals.len(),
                    if app.terminals.len() == 1 { "" } else { "s" }
                );
            }
        }

        Action::Tick => {
            app.tick_count += 1;
            // Process pending PTY output for all terminals (needed for ghostty backend).
            for term in app.terminals.values_mut() {
                term.process_pending();
            }
            // Update Claude state detection for each Claude terminal.
            {
                use crate::agent_state::{AgentState, detect_state};
                let asking_patterns = &app.config.agent.config.asking_patterns;
                let claude_keys: Vec<String> = app.terminals.keys()
                    .filter(|k| app.terminal_kinds.get(*k)
                        .map(|kind| matches!(kind, ShellKind::Claude))
                        .unwrap_or(false))
                    .cloned()
                    .collect();

                for key in &claude_keys {
                    if let Some(term) = app.terminals.get(key) {
                        let prev = app.agent_states.get(key).copied()
                            .unwrap_or(AgentState::Active);
                        let new_state = detect_state(
                            term.last_output_at(),
                            term.recent_output(),
                            prev,
                            asking_patterns,
                        );
                        // Handle transitions.
                        if new_state != prev {
                            if new_state == AgentState::Active {
                                app.notified_asking.remove(key);
                            }
                            if new_state == AgentState::Asking && !app.notified_asking.contains(key) {
                                app.notified_asking.insert(key.clone());
                                let title = app.sessions.get(key)
                                    .map(|s| s.display_name.clone())
                                    .unwrap_or_else(|| key.clone());
                                app.status = format!("Claude needs input: {title}");
                                let title_clone = title.clone();
                                tokio::spawn(async move {
                                    crate::notify::send_macos_notification(
                                        &format!("pilot: {title_clone}"),
                                        "Claude needs your input",
                                    ).await;
                                });
                            }
                        }
                        app.agent_states.insert(key.clone(), new_state);
                    }
                }

                // Clean up states for removed terminals.
                app.agent_states.retain(|k, _| app.terminals.contains_key(k));
            }
            // Inject pending prompts when Claude becomes idle (or after 5s timeout).
            {
                use crate::agent_state::AgentState;
                let ready_keys: Vec<String> = app.pending_prompts.keys()
                    .filter(|key| {
                        let is_idle = app.agent_states.get(*key)
                            .map(|s| *s == AgentState::Idle)
                            .unwrap_or(false);
                        // Also inject if terminal exists and we've waited 5+ seconds.
                        let has_terminal = app.terminals.contains_key(*key);
                        let waited_long = app.last_claude_send
                            .map(|t| t.elapsed().as_secs() >= 5)
                            .unwrap_or(false);
                        is_idle || (has_terminal && waited_long)
                    })
                    .cloned()
                    .collect();
                for key in ready_keys {
                    if let Some(prompt) = app.pending_prompts.remove(&key) {
                        if let Some(term) = app.terminals.get_mut(&key) {
                            let _ = term.write(prompt.as_bytes());
                            let _ = term.write(b"\r");
                            tracing::info!("Injected pending prompt into {key}");
                            app.status = "Prompt sent to Claude".into();
                        }
                    }
                }
            }

            // Note: stale sessions are handled by TaskRemoved events from the poller.
            // We don't purge from SQLite on startup — the nav filters hide merged/closed PRs.

            // Auto-mark-read: if viewing a session for 2+ seconds, mark it read.
            if let Some(key) = app.selected_session_key() {
                match &app.viewing_since {
                    Some((viewed_key, since)) if viewed_key == &key => {
                        if since.elapsed().as_secs() >= 2 {
                            if let Some(session) = app.sessions.get_mut(&key) {
                                if session.unread_count() > 0 {
                                    session.mark_read();
                                }
                            }
                        }
                    }
                    _ => {
                        app.viewing_since = Some((key, std::time::Instant::now()));
                    }
                }
            } else {
                app.viewing_since = None;
            }

            // Save all sessions every ~3s (30 ticks at 100ms).
            if app.tick_count % 30 == 0 && !app.sessions.is_empty() {
                save_all_sessions(app);
            }
            // MCP confirmations now come via Unix socket (no polling needed).
            let exited: Vec<String> = app
                .terminals
                .iter()
                .filter(|(_, t)| t.is_finished())
                .map(|(k, _)| k.clone())
                .collect();
            for key in exited {
                app.terminals.remove(&key);
                app.terminal_kinds.remove(&key);
                app.tab_order.retain(|k| k != &key);
                if let Some(_prompt) = app.pending_prompts.remove(&key) {
                    tracing::warn!("Pending prompt lost for {key} (terminal exited)");
                    app.status = format!("Warning: queued prompt lost — terminal exited");
                }
                if let Some(session) = app.sessions.get_mut(&key) {
                    session.state = SessionState::Active;
                    // If monitored and fixing CI, Claude exited — wait for CI.
                    if let Some(pilot_core::MonitorState::CiFixing { attempt }) = &session.monitor {
                        let attempt = *attempt;
                        session.monitor = Some(pilot_core::MonitorState::WaitingCi { after_attempt: attempt });
                        tracing::info!("Monitor: Claude exited for {key}, waiting for CI (attempt {attempt})");
                    }
                }
            }
            if app.active_tab >= app.tab_order.len() && !app.tab_order.is_empty() {
                app.active_tab = app.tab_order.len() - 1;
            }

            // Periodic merge conflict check for monitored sessions (~30s).
            if app.tick_count % 300 == 0 {
                let rebase_candidates: Vec<_> = app.sessions.iter()
                    .filter(|(_, s)| matches!(s.monitor, Some(pilot_core::MonitorState::Idle)))
                    .filter(|(_, s)| s.worktree_path.is_some())
                    .map(|(k, s)| {
                        let repo = s.primary_task.repo.clone().unwrap_or_default();
                        let pr_num = s.primary_task.id.key.rsplit_once('#')
                            .map(|(_, n)| n.to_string())
                            .unwrap_or_default();
                        let wt_path = s.worktree_path.clone().unwrap();
                        (k.clone(), repo, pr_num, wt_path)
                    })
                    .filter(|(_, repo, pr, _)| !repo.is_empty() && !pr.is_empty())
                    .collect();

                for (key, repo, pr_num, wt_path) in rebase_candidates {
                    // Look up default branch from cache; if missing, spawn a fetch and skip this cycle.
                    let default_branch = match app.default_branch_cache.get(&repo) {
                        Some(branch) => branch.clone(),
                        None => {
                            let repo_clone = repo.clone();
                            let cache_tx = action_tx.clone();
                            tokio::spawn(async move {
                                let output = tokio::process::Command::new("gh")
                                    .args(["api", &format!("repos/{repo_clone}"), "--jq", ".default_branch"])
                                    .output()
                                    .await;
                                if let Ok(o) = output {
                                    if o.status.success() {
                                        let branch = String::from_utf8_lossy(&o.stdout).trim().to_string();
                                        let _ = cache_tx.send(Action::CacheDefaultBranch {
                                            repo: repo_clone,
                                            branch,
                                        });
                                    }
                                }
                            });
                            continue; // Skip this cycle; will rebase on the next 30s tick.
                        }
                    };
                    // Transition to Rebasing BEFORE spawning async task.
                    if let Some(session) = app.sessions.get_mut(&key) {
                        session.monitor = Some(pilot_core::MonitorState::Rebasing);
                    }
                    let tx = action_tx.clone();
                    tokio::spawn(async move {
                        if check_needs_rebase(&repo, &pr_num).await {
                            tracing::info!("Monitor: {key} needs rebase");
                            run_rebase(&wt_path, tx, key, &default_branch).await;
                        }
                    });
                }
            }
        }

        Action::Key(key) => {
            // Help overlay — any key dismisses it.
            if app.show_help {
                app.show_help = false;
                return;
            }
            // MCP confirmation mode — y/n to approve/reject.
            if app.pending_mcp.is_some() {
                use crossterm::event::KeyCode;
                match key.code {
                    KeyCode::Char('y') | KeyCode::Enter => {
                        handle_action(app, Action::ApproveMcpAction, action_tx);
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {
                        handle_action(app, Action::RejectMcpAction, action_tx);
                    }
                    _ => {} // ignore other keys during confirmation
                }
                return;
            }
            // Clear quit confirmation on any key that isn't q.
            if app.quit_pending && key.code != crossterm::event::KeyCode::Char('q') {
                app.quit_pending = false;
                app.status = String::new();
            }
            // Clear merge confirmation on any key that isn't M.
            if app.merge_pending.is_some() && key.code != crossterm::event::KeyCode::Char('M') {
                app.merge_pending = None;
                app.status = String::new();
            }
            // Search mode intercepts keys.
            if app.search_active {
                use crossterm::event::KeyCode;
                match key.code {
                    KeyCode::Esc => {
                        handle_action(app, Action::SearchClear, action_tx);
                    }
                    KeyCode::Enter => {
                        app.search_active = false; // keep filter, exit typing
                    }
                    KeyCode::Backspace => {
                        handle_action(app, Action::SearchBackspace, action_tx);
                    }
                    KeyCode::Char(c) => {
                        handle_action(app, Action::SearchInput(c), action_tx);
                    }
                    _ => {}
                }
                return;
            }
            // Picker overlay intercepts keys.
            if app.picker.is_some() {
                use crossterm::event::KeyCode;
                match key.code {
                    KeyCode::Esc => {
                        handle_action(app, Action::PickerCancel, action_tx);
                    }
                    KeyCode::Enter => {
                        // If nothing was changed yet, toggle the current item first.
                        if let Some(ref mut picker) = app.picker {
                            let any_changed = picker.items.iter().any(|i| i.selected != i.was_selected);
                            if !any_changed {
                                let filtered = picker.filtered_indices();
                                if let Some(&real_idx) = filtered.get(picker.cursor) {
                                    picker.items[real_idx].selected = !picker.items[real_idx].selected;
                                }
                            }
                        }
                        handle_action(app, Action::PickerConfirm, action_tx);
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        if let Some(ref mut picker) = app.picker {
                            let count = picker.filtered_indices().len();
                            if count > 0 {
                                picker.cursor = (picker.cursor + 1).min(count - 1);
                            }
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        if let Some(ref mut picker) = app.picker {
                            picker.cursor = picker.cursor.saturating_sub(1);
                        }
                    }
                    KeyCode::Char(' ') => {
                        if let Some(ref mut picker) = app.picker {
                            let filtered = picker.filtered_indices();
                            if let Some(&real_idx) = filtered.get(picker.cursor) {
                                picker.items[real_idx].selected = !picker.items[real_idx].selected;
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        if let Some(ref mut picker) = app.picker {
                            picker.filter.pop();
                            picker.cursor = 0;
                        }
                    }
                    KeyCode::Char(c) => {
                        if let Some(ref mut picker) = app.picker {
                            picker.filter.push(c);
                            picker.cursor = 0;
                        }
                    }
                    _ => {}
                }
                return;
            }

            // New session input intercepts keys.
            if app.new_session_input.is_some() {
                use crossterm::event::KeyCode;
                match key.code {
                    KeyCode::Esc => {
                        handle_action(app, Action::NewSessionCancel, action_tx);
                    }
                    KeyCode::Enter => {
                        let desc = app.new_session_input.clone().unwrap_or_default();
                        if !desc.trim().is_empty() {
                            handle_action(app, Action::NewSessionConfirm { description: desc }, action_tx);
                        }
                    }
                    KeyCode::Backspace => {
                        if let Some(ref mut input) = app.new_session_input {
                            input.pop();
                        }
                    }
                    KeyCode::Char(c) => {
                        if let Some(ref mut input) = app.new_session_input {
                            input.push(c);
                        }
                    }
                    _ => {}
                }
                return;
            }

            // Quick reply input intercepts keys.
            if app.quick_reply_input.is_some() {
                use crossterm::event::KeyCode;
                match key.code {
                    KeyCode::Esc => {
                        handle_action(app, Action::QuickReplyCancel, action_tx);
                    }
                    KeyCode::Enter => {
                        let body = app.quick_reply_input.as_ref().map(|(_, t, _)| t.clone()).unwrap_or_default();
                        if !body.trim().is_empty() {
                            handle_action(app, Action::QuickReplyConfirm { body }, action_tx);
                        }
                    }
                    KeyCode::Backspace => {
                        if let Some((_, ref mut text, _)) = app.quick_reply_input {
                            text.pop();
                        }
                    }
                    KeyCode::Char(c) => {
                        if let Some((_, ref mut text, _)) = app.quick_reply_input {
                            text.push(c);
                        }
                    }
                    _ => {}
                }
                return;
            }

            let mapped = keys::map_key(key, app.key_mode);
            match mapped {
                Action::WaitingPrefix => {
                    app.key_mode = KeyMode::PanePrefix;
                }
                Action::None if app.key_mode == KeyMode::Terminal => {
                    // Forward to PTY.
                    if let Some(tab_key) = app.active_tab_key().cloned() {
                        if let Some(term) = app.terminals.get_mut(&tab_key) {
                            term.scroll_reset();
                            if let Some(bytes) = keys::key_to_bytes(&key) {
                                if let Err(e) = term.write(&bytes) {
                                    tracing::error!("PTY write failed: {e}");
                                    app.status = format!("Error: terminal write failed: {e}");
                                }
                            }
                        } else {
                            tracing::warn!("Terminal mode but no terminal for tab: {tab_key}");
                            app.key_mode = KeyMode::Normal;
                            app.status = "Terminal disconnected — returned to sidebar".into();
                        }
                    } else {
                        tracing::warn!("Terminal mode but no active tab");
                        app.key_mode = KeyMode::Normal;
                    }
                }
                other => {
                    // After pane prefix, return to previous mode.
                    if app.key_mode == KeyMode::PanePrefix {
                        app.key_mode = determine_mode(app);
                    }
                    handle_action(app, other, action_tx);
                }
            }
        }

        // ── Navigation ──
        Action::SelectNext => {
            let nav_count = nav_items(app).len();
            if nav_count > 0 {
                app.selected = (app.selected + 1).min(nav_count - 1);
            }
            reset_detail_state(app);
        }
        Action::SelectPrev => {
            app.selected = app.selected.saturating_sub(1);
            reset_detail_state(app);
        }

        // ── Pane operations ──
        Action::SplitVertical => {
            let content = current_detail_content(app);
            app.panes.split_vertical(content);
            app.key_mode = determine_mode(app);
        }
        Action::SplitHorizontal => {
            let content = current_detail_content(app);
            app.panes.split_horizontal(content);
            app.key_mode = determine_mode(app);
        }
        Action::ClosePane => {
            app.panes.close_focused();
            app.key_mode = determine_mode(app);
        }
        Action::FocusPaneNext => {
            // Clear terminal mode so determine_mode re-evaluates from the new pane.
            if app.key_mode == KeyMode::Terminal {
                app.key_mode = KeyMode::Normal;
            }
            app.panes.focus_next();
            app.key_mode = determine_mode(app);
        }
        Action::FocusPaneUp | Action::FocusPaneDown
        | Action::FocusPaneLeft | Action::FocusPaneRight => {
            // Clear terminal mode so determine_mode re-evaluates from the new pane.
            if app.key_mode == KeyMode::Terminal {
                app.key_mode = KeyMode::Normal;
            }
            let dir = match action {
                Action::FocusPaneUp => Direction::Up,
                Action::FocusPaneDown => Direction::Down,
                Action::FocusPaneLeft => Direction::Left,
                _ => Direction::Right,
            };
            app.panes.focus_direction(dir, ratatui::prelude::Rect::default());
            app.key_mode = determine_mode(app);
        }
        Action::ResizePane(delta) => {
            app.panes.resize_focused(delta);
        }
        Action::FullscreenToggle => {
            app.panes.fullscreen_toggle();
        }

        // ── Tabs ──
        Action::NextTab => {
            if !app.tab_order.is_empty() {
                app.active_tab = (app.active_tab + 1) % app.tab_order.len();
                sync_selected_to_tab(app);
                app.key_mode = KeyMode::Terminal;
            }
        }
        Action::PrevTab => {
            if !app.tab_order.is_empty() {
                app.active_tab = (app.active_tab + app.tab_order.len() - 1) % app.tab_order.len();
                sync_selected_to_tab(app);
                app.key_mode = KeyMode::Terminal;
            }
        }
        Action::GoToTab(n) => {
            let idx = n - 1;
            if idx < app.tab_order.len() {
                app.active_tab = idx;
                sync_selected_to_tab(app);
                app.key_mode = KeyMode::Terminal;
            }
        }
        Action::CloseTab => {
            if let Some(key) = app.active_tab_key().cloned() {
                // Kill terminal but keep session.
                app.terminals.remove(&key);
                app.terminal_kinds.remove(&key);
                app.tab_order.retain(|k| k != &key);
                if let Some(session) = app.sessions.get_mut(&key) {
                    session.state = SessionState::Active;
                }
                if app.active_tab >= app.tab_order.len() && !app.tab_order.is_empty() {
                    app.active_tab = app.tab_order.len() - 1;
                }
                app.key_mode = determine_mode(app);
            }
        }

        // ── Session management ──
        Action::MarkRead => {
            if let Some(key) = app.selected_session_key() {
                if let Some(session) = app.sessions.get_mut(&key) {
                    session.mark_read();
                    session.primary_task.needs_reply = false;
                    if let Err(e) = app.store.mark_read(&session.task_id, session.seen_count as i64)
                    {
                        tracing::warn!("Failed to persist mark_read: {e}");
                    }
                }
                // Re-sort but keep cursor on the same session.
                let prev_mode = app.key_mode;
                resort_sessions(app);
                // Stay in current mode (don't jump away from detail).
                app.key_mode = prev_mode;
                update_detail_pane(app);
                app.status = "Marked as read".into();
            }
        }


        Action::OpenSession(shell_kind) => {
            if let Some(key) = app.selected_session_key() {
                if app.terminals.contains_key(&key) {
                    // Already has terminal — just switch to that tab.
                    if let Some(idx) = app.tab_order.iter().position(|k| k == &key) {
                        app.active_tab = idx;
                    }
                    app.key_mode = KeyMode::Terminal;
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

        Action::WorktreeReady { session_key, path } => {
            if let Some(session) = app.sessions.get_mut(&session_key) {
                session.worktree_path = Some(path);
                session.state = SessionState::Active;
                app.status = format!("Worktree ready: {}", session.display_name);
                // If this session is monitored, kick the state machine now that worktree is ready.
                if session.monitor.is_some() {
                    let _ = action_tx.send(Action::MonitorTick { session_key: session_key.clone() });
                }
            }
        }

        Action::ExternalEvent(event) => {
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
                        let mut session = Session::new(task.clone());
                        for activity in &task.recent_activity {
                            session.push_activity(activity.clone());
                        }
                        if let Ok(Some(record)) = app.store.get_session(&task.id) {
                            session.seen_count = record.seen_count as usize;
                            session.last_viewed_at = record.last_viewed_at;
                        }
                        if !app.session_order.contains(&key) {
                            app.session_order.push(key.clone());
                        }
                        app.sessions.insert(key, session);
                    }
                    // Persist full session to SQLite.
                    if let Some(session) = app.sessions.get(&persist_key) {
                        let json = serde_json::to_string(session).ok();
                        let _ = app.store.save_session(&pilot_store::SessionRecord {
                            task_id: persist_key.clone(),
                            seen_count: session.seen_count as i64,
                            last_viewed_at: session.last_viewed_at,
                            created_at: session.created_at,
                            session_json: json,
                            metadata: None,
                        });
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
                                    crate::notify::send_macos_notification(
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
                                    crate::notify::send_macos_notification(
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
                                    crate::notify::send_macos_notification(
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
                    app.terminals.remove(&key);
                    app.terminal_kinds.remove(&key);
                    app.session_order.retain(|k| k != &key);
                    app.tab_order.retain(|k| k != &key);
                    if app.selected >= app.session_order.len() && !app.session_order.is_empty() {
                        app.selected = app.session_order.len() - 1;
                    }
                    // Also remove from persistent store so it doesn't come back on restart.
                    let _ = app.store.delete_session(id);
                }
                EventKind::ProviderError { ref message } => {
                    tracing::warn!("Provider error: {message}");
                    // Only show error in status bar if we haven't loaded yet.
                    // Transient errors after initial load are just logged.
                    if !app.loaded {
                        app.status = format!("Error: {message}");
                    }
                }
            }
        }

        Action::ToggleRepo(repo) => {
            // Determine which repo to toggle.
            let repo = if repo.is_empty() {
                // Use the current nav item to find the repo.
                match selected_nav_item(app) {
                    Some(NavItem::Repo(r)) => r,
                    Some(NavItem::Session(k)) => {
                        app.sessions.get(&k).map(|s| s.repo.clone()).unwrap_or_default()
                    }
                    None => String::new(),
                }
            } else {
                repo
            };
            if !repo.is_empty() {
                let repo_for_lookup = repo.clone();
                if app.collapsed_repos.contains(&repo) {
                    app.collapsed_repos.remove(&repo);
                } else {
                    app.collapsed_repos.insert(repo);
                }
                // Keep cursor on the repo header after collapse.
                let items = nav_items(app);
                if let Some(idx) = items.iter().position(|i| matches!(i, NavItem::Repo(r) if r == &repo_for_lookup)) {
                    app.selected = idx;
                }
                // Clamp.
                let nav_count = nav_items(app).len();
                if app.selected >= nav_count && nav_count > 0 {
                    app.selected = nav_count - 1;
                }
            }
        }
        Action::ToggleSession(key) => {
            // If key is empty, use the currently selected session.
            let key = if key.is_empty() {
                app.selected_session_key().unwrap_or_default()
            } else {
                key
            };
            if !key.is_empty() {
                if app.collapsed_sessions.contains(&key) {
                    app.collapsed_sessions.remove(&key);
                } else {
                    app.collapsed_sessions.insert(key);
                }
            }
        }

        Action::Mouse(mouse) => {
            use crossterm::event::{MouseEventKind, MouseButton};
            let (term_w, _term_h) = app.last_term_area;
            // The sidebar border is at sidebar_pct% of the terminal width.
            let border_col = (term_w as u32 * app.sidebar_pct as u32 / 100) as u16;

            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    // Check if clicking on the sidebar/detail divider (±1 col).
                    if mouse.column.abs_diff(border_col) <= 1 {
                        app.drag_resize = Some(DragState);
                        return;
                    }

                    // Click on sidebar area → select item or toggle repo.
                    if mouse.column < border_col {
                        app.key_mode = KeyMode::Normal;
                        // Map click row to a sidebar item.
                        // Row 0 = title bar, 1 = search, 2+ = items.
                        let click_row = mouse.row.saturating_sub(2) as usize;
                        handle_sidebar_click(app, click_row, action_tx);
                    } else {
                        // Click on detail/terminal area.
                        // Check if there's a running terminal for the selected session.
                        let has_term = app.selected_session_key()
                            .and_then(|k| app.terminals.get(&k).map(|_| ()))
                            .is_some();
                        // Click in upper part → detail, lower part → terminal (if exists).
                        let right_area_height = _term_h;
                        let detail_cutoff = right_area_height * 30 / 100; // 30% for detail
                        if has_term && mouse.row > detail_cutoff {
                            app.key_mode = KeyMode::Terminal;
                        } else {
                            app.key_mode = KeyMode::Detail;
                        }
                        update_detail_pane(app);
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if app.drag_resize.is_some() {
                        // Resize sidebar by mouse position.
                        if term_w > 0 {
                            let new_pct = (mouse.column as u32 * 100 / term_w as u32) as u16;
                            app.sidebar_pct = new_pct.clamp(20, 80);
                        }
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    app.drag_resize = None;
                }
                MouseEventKind::ScrollUp => {
                    if app.key_mode == KeyMode::Terminal {
                        // Send mouse scroll events to the PTY.
                        // SGR encoding: \x1b[<65;col;rowM for scroll up.
                        if let Some(tab_key) = app.active_tab_key().cloned() {
                            if let Some(term) = app.terminals.get_mut(&tab_key) {
                                let col = mouse.column + 1;
                                let row = mouse.row + 1;
                                for _ in 0..3 {
                                    let seq = format!("\x1b[<64;{col};{row}M");
                                    let _ = term.write(seq.as_bytes());
                                }
                            }
                        }
                    } else {
                        app.detail_scroll = app.detail_scroll.saturating_sub(3);
                    }
                }
                MouseEventKind::ScrollDown => {
                    if app.key_mode == KeyMode::Terminal {
                        if let Some(tab_key) = app.active_tab_key().cloned() {
                            if let Some(term) = app.terminals.get_mut(&tab_key) {
                                let col = mouse.column + 1;
                                let row = mouse.row + 1;
                                for _ in 0..3 {
                                    let seq = format!("\x1b[<65;{col};{row}M");
                                    let _ = term.write(seq.as_bytes());
                                }
                            }
                        }
                    } else {
                        app.detail_scroll = app.detail_scroll.saturating_add(3);
                    }
                }
                _ => {}
            }
        }

        Action::Paste(text) => {
            // Forward paste to the active terminal using bracketed paste mode.
            if app.key_mode == KeyMode::Terminal {
                if let Some(tab_key) = app.active_tab_key().cloned() {
                    if let Some(term) = app.terminals.get_mut(&tab_key) {
                        // Bracketed paste: \x1b[200~ ... \x1b[201~
                        let _ = term.write(b"\x1b[200~");
                        let _ = term.write(text.as_bytes());
                        let _ = term.write(b"\x1b[201~");
                    }
                }
            }
        }

        // ── Detail pane ──
        Action::DetailCursorUp => {
            app.detail_cursor = app.detail_cursor.saturating_sub(1);
            // Auto-mark current comment as read when navigating to it.
            if let Some(key) = app.selected_session_key() {
                if let Some(session) = app.sessions.get_mut(&key) {
                    if session.is_activity_unread(app.detail_cursor) {
                        session.mark_activity_read(app.detail_cursor);
                    }
                }
            }
        }
        Action::DetailCursorDown => {
            if let Some(key) = app.selected_session_key() {
                if let Some(session) = app.sessions.get(&key) {
                    let max = session.activity.len().saturating_sub(1);
                    app.detail_cursor = (app.detail_cursor + 1).min(max);
                }
            }
            // Auto-mark current comment as read when navigating to it.
            if let Some(key) = app.selected_session_key() {
                if let Some(session) = app.sessions.get_mut(&key) {
                    if session.is_activity_unread(app.detail_cursor) {
                        session.mark_activity_read(app.detail_cursor);
                    }
                }
            }
        }
        Action::ToggleCommentSelect => {
            let idx = app.detail_cursor;
            if app.selected_comments.contains(&idx) {
                app.selected_comments.remove(&idx);
            } else {
                app.selected_comments.insert(idx);
            }
            // Mark as read.
            if let Some(key) = app.selected_session_key() {
                if let Some(session) = app.sessions.get_mut(&key) {
                    if session.is_activity_unread(idx) {
                        session.mark_activity_read(idx);
                    }
                }
            }
        }
        Action::SelectAllComments => {
            if let Some(key) = app.selected_session_key() {
                if let Some(session) = app.sessions.get(&key) {
                    if app.selected_comments.len() == session.activity.len() {
                        app.selected_comments.clear();
                    } else {
                        app.selected_comments = (0..session.activity.len()).collect();
                    }
                }
            }
        }
        Action::FixWithClaude => {
            fix_or_reply_with_claude(app, action_tx, "fix");
        }
        Action::ReplyWithClaude => {
            fix_or_reply_with_claude(app, action_tx, "reply");
        }

        // ── Search ──
        Action::SearchActivate => {
            app.search_active = true;
        }
        Action::SearchInput(c) => {
            app.search_query.push(c);
            apply_search_filter(app);
        }
        Action::SearchBackspace => {
            app.search_query.pop();
            if app.search_query.is_empty() {
                app.filtered_keys = None;
            } else {
                apply_search_filter(app);
            }
        }
        Action::SearchClear => {
            app.search_query.clear();
            app.filtered_keys = None;
            app.search_active = false;
        }

        // ── Picker (reviewer/assignee) ──
        Action::EditReviewers | Action::EditAssignees => {
            let kind = if matches!(action, Action::EditReviewers) {
                crate::action::PickerKind::Reviewer
            } else {
                crate::action::PickerKind::Assignee
            };
            if let Some(key) = app.selected_session_key() {
                if let Some(session) = app.sessions.get(&key) {
                    let task = &session.primary_task;
                    let repo = task.repo.as_deref().unwrap_or("").to_string();
                    let pr_number = task.id.key.rsplit_once('#')
                        .map(|(_, n)| n.to_string())
                        .unwrap_or_default();

                    if repo.is_empty() || pr_number.is_empty() {
                        app.status = "No PR info available".into();
                        return;
                    }

                    let current: Vec<String> = match kind {
                        crate::action::PickerKind::Reviewer => task.reviewers.clone(),
                        crate::action::PickerKind::Assignee => task.assignees.clone(),
                    };

                    if let Some(collabs) = app.collaborators_cache.get(&repo) {
                        let items = build_picker_items(collabs, &current);
                        app.picker = Some(PickerState {
                            kind,
                            items,
                            cursor: 0,
                            filter: String::new(),
                            session_key: key,
                            repo,
                            pr_number,
                        });
                    } else {
                        app.status = format!("Loading collaborators for {repo}…");
                        let repo_clone = repo.clone();
                        let tx = action_tx.clone();
                        tokio::spawn(async move {
                            let collabs = fetch_collaborators(&repo_clone).await;
                            let _ = tx.send(Action::CollaboratorsLoaded {
                                repo: repo_clone,
                                kind,
                                session_key: key,
                                pr_number,
                                collaborators: collabs,
                                current,
                            });
                        });
                    }
                }
            }
        }

        Action::CollaboratorsLoaded { repo, kind, session_key, pr_number, collaborators, current } => {
            app.collaborators_cache.insert(repo.clone(), collaborators.clone());
            let items = build_picker_items(&collaborators, &current);
            app.picker = Some(PickerState {
                kind,
                items,
                cursor: 0,
                filter: String::new(),
                session_key,
                repo,
                pr_number,
            });
            app.status = String::new();
        }

        Action::PickerCancel => {
            app.picker = None;
        }

        Action::PickerConfirm => {
            if let Some(picker) = app.picker.take() {
                let added: Vec<String> = picker.items.iter()
                    .filter(|i| i.selected && !i.was_selected)
                    .map(|i| i.login.clone())
                    .collect();
                let removed: Vec<String> = picker.items.iter()
                    .filter(|i| !i.selected && i.was_selected)
                    .map(|i| i.login.clone())
                    .collect();

                if added.is_empty() && removed.is_empty() {
                    return;
                }

                let label = match picker.kind {
                    crate::action::PickerKind::Reviewer => "reviewer",
                    crate::action::PickerKind::Assignee => "assignee",
                };
                app.status = format!("Updating {label}s…");

                // Optimistic update.
                if let Some(session) = app.sessions.get_mut(&picker.session_key) {
                    let people = match picker.kind {
                        crate::action::PickerKind::Reviewer => &mut session.primary_task.reviewers,
                        crate::action::PickerKind::Assignee => &mut session.primary_task.assignees,
                    };
                    people.retain(|p| !removed.contains(p));
                    for user in &added {
                        if !people.contains(user) {
                            people.push(user.clone());
                        }
                    }
                }

                let repo = picker.repo;
                let pr = picker.pr_number;
                let kind = picker.kind;
                let tx = action_tx.clone();
                tokio::spawn(async move {
                    let (add_flag, remove_flag) = match kind {
                        crate::action::PickerKind::Reviewer => ("--add-reviewer", "--remove-reviewer"),
                        crate::action::PickerKind::Assignee => ("--add-assignee", "--remove-assignee"),
                    };
                    let mut args = vec![
                        "pr".to_string(), "edit".to_string(), pr,
                        "--repo".to_string(), repo,
                    ];
                    for user in &added {
                        args.push(add_flag.to_string());
                        args.push(user.clone());
                    }
                    for user in &removed {
                        args.push(remove_flag.to_string());
                        args.push(user.clone());
                    }
                    tracing::info!("Running: gh {}", args.join(" "));
                    let output = tokio::process::Command::new("gh")
                        .args(&args)
                        .output()
                        .await;
                    match output {
                        Ok(o) if o.status.success() => {
                            let label = match kind {
                                crate::action::PickerKind::Reviewer => "reviewers",
                                crate::action::PickerKind::Assignee => "assignees",
                            };
                            tracing::info!("Updated {label}: +{added:?} -{removed:?}");
                            let _ = tx.send(Action::StatusMessage(format!("Updated {label}")));
                        }
                        Ok(o) => {
                            let stderr = String::from_utf8_lossy(&o.stderr);
                            let stdout = String::from_utf8_lossy(&o.stdout);
                            tracing::error!("gh pr edit failed (exit {}): stderr={stderr} stdout={stdout}", o.status);
                            let _ = tx.send(Action::StatusMessage(format!("Error: {}", stderr.trim())));
                        }
                        Err(e) => {
                            tracing::error!("gh pr edit error: {e}");
                            let _ = tx.send(Action::StatusMessage(format!("Error: {e}")));
                        }
                    }
                });
            }
        }

        Action::Resize { width, height } => {
            app.last_term_area = (width, height);
        }
        Action::CollapseSelected => {
            match selected_nav_item(app) {
                Some(NavItem::Repo(repo)) => {
                    // Collapse this repo.
                    app.collapsed_repos.insert(repo);
                }
                Some(NavItem::Session(key)) => {
                    // Collapse the repo this session belongs to.
                    if let Some(session) = app.sessions.get(&key) {
                        let repo = session.repo.clone();
                        app.collapsed_repos.insert(repo.clone());
                        // Move cursor to the repo header.
                        let items = nav_items(app);
                        if let Some(idx) = items.iter().position(|i| matches!(i, NavItem::Repo(r) if r == &repo)) {
                            app.selected = idx;
                        }
                    }
                }
                None => {}
            }
        }
        Action::ExpandSelected => {
            match selected_nav_item(app) {
                Some(NavItem::Repo(repo)) => {
                    // Expand this repo.
                    app.collapsed_repos.remove(&repo);
                }
                Some(NavItem::Session(_key)) => {
                    // Session is already visible — right arrow does nothing.
                    // Only Enter goes to the detail pane.
                }
                None => {}
            }
        }

        Action::OpenInBrowser => {
            if let Some(key) = app.selected_session_key() {
                if let Some(session) = app.sessions.get(&key) {
                    let url = &session.primary_task.url;
                    if !url.is_empty() {
                        let url = url.clone();
                        tokio::spawn(async move {
                            let _ = tokio::process::Command::new("open")
                                .arg(&url)
                                .output()
                                .await;
                        });
                        app.status = format!("Opened in browser");
                    } else {
                        app.status = "No URL for this session".into();
                    }
                }
            }
        }

        Action::MergePr => {
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
                        app.key_mode = KeyMode::Normal;
                        // Optimistic update — show as queued/merging immediately.
                        if let Some(session) = app.sessions.get_mut(&key) {
                            session.primary_task.in_merge_queue = true;
                        }
                        let repo = repo.to_string();
                        let pr = pr_num.clone();
                        let _session_key = key.clone();
                        let tx = action_tx.clone();
                        tokio::spawn(async move {
                            let output = tokio::process::Command::new("gh")
                                .args(["pr", "merge", &pr, "--squash", "--repo", &repo])
                                .output()
                                .await;
                            match output {
                                Ok(o) if o.status.success() => {
                                    tracing::info!("Merged {repo}#{pr}");
                                    let _ = tx.send(Action::StatusMessage(
                                        format!("Merged {repo}#{pr}"),
                                    ));
                                }
                                Ok(o) => {
                                    let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                                    tracing::error!("Merge failed: {err}");
                                    // Revert optimistic update — send task state back to Open.
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

        Action::SlackNudge => {
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

        Action::ToggleMonitor => {
            if let Some(key) = app.selected_session_key() {
                if let Some(session) = app.sessions.get_mut(&key) {
                    if session.monitor.is_some() {
                        // Turn off monitor.
                        session.monitor = None;
                        app.monitored_sessions.lock().expect("monitored_sessions lock").remove(&key);
                        app.status = format!("Monitor stopped: {}", session.display_name);
                    } else {
                        // Turn on monitor.
                        session.monitor = Some(pilot_core::MonitorState::Idle);
                        app.monitored_sessions.lock().expect("monitored_sessions lock").insert(key.clone());
                        app.status = format!("Monitor started: {}", session.display_name);

                        // Ensure worktree exists.
                        if session.worktree_path.is_none() {
                            let repo = session.primary_task.repo.clone();
                            let branch = session.primary_task.branch.clone();
                            session.state = SessionState::CheckingOut;

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
                                            }
                                            Err(e) => {
                                                tracing::error!("Monitor worktree checkout failed: {e}");
                                            }
                                        }
                                    });
                                }
                            }
                        }

                        // If CI is already failing and worktree is ready, trigger immediate fix.
                        // If worktree isn't ready yet, WorktreeReady will trigger the tick.
                        if session.primary_task.ci == pilot_core::CiStatus::Failure
                            && session.worktree_path.is_some()
                        {
                            let _ = action_tx.send(Action::MonitorTick { session_key: key });
                        }
                    }
                }
            }
        }

        Action::MonitorTick { session_key } => {
            handle_monitor_tick(app, &session_key, action_tx);
        }

        Action::ToggleHelp => {
            app.show_help = !app.show_help;
        }

        Action::CycleTimeFilter => {
            app.activity_days_filter = match app.activity_days_filter {
                1 => 3,
                3 => 7,
                7 => 30,
                30 => 0, // 0 = all
                _ => 1,
            };
            let label = match app.activity_days_filter {
                0 => "all time".to_string(),
                d => format!("last {d}d"),
            };
            app.status = format!("Filter: {label}");
            app.selected = 0;
        }

        Action::McpConfirmRequest { tool, display, response_tx } => {
            app.pending_mcp = Some(PendingMcpAction {
                tool: tool.clone(),
                display,
                response_tx: Some(response_tx),
            });
            app.status = format!("Claude wants to: {tool} — y/n");
        }
        Action::ApproveMcpAction => {
            if let Some(action) = app.pending_mcp.take() {
                if let Some(tx) = action.response_tx {
                    let _ = tx.send(true);
                }
                app.status = format!("Approved: {}", action.tool);
            }
        }
        Action::RejectMcpAction => {
            if let Some(action) = app.pending_mcp.take() {
                if let Some(tx) = action.response_tx {
                    let _ = tx.send(false);
                }
                app.status = format!("Rejected: {}", action.tool);
            }
        }

        Action::StatusMessage(msg) => {
            app.status = msg;
        }

        Action::Snooze => {
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

        Action::QuickReply => {
            if let Some(key) = app.selected_session_key() {
                let cursor = app.detail_cursor;
                app.quick_reply_input = Some((key, String::new(), cursor));
                app.status = "Quick reply — type message, Enter to post, Esc to cancel".into();
            }
        }

        Action::QuickReplyCancel => {
            app.quick_reply_input = None;
            app.status = String::new();
        }

        Action::QuickReplyConfirm { body } => {
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

        Action::NewSession => {
            app.new_session_input = Some(String::new());
            app.status = "New session — type description, Enter to create, Esc to cancel".into();
        }

        Action::NewSessionCancel => {
            app.new_session_input = None;
            app.status = String::new();
        }

        Action::NewSessionConfirm { description } => {
            app.new_session_input = None;
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
            app.session_order.push(key.clone());
            app.sessions.insert(key, session);
            crate::nav::resort_sessions(app);
            app.status = format!("Created session: {description}");
        }

        Action::CacheDefaultBranch { repo, branch } => {
            tracing::info!("Cached default branch for {repo}: {branch}");
            app.default_branch_cache.insert(repo, branch);
        }

        Action::Refresh => {
            if let Some(ref wake) = app.poller_wake {
                wake.notify_one();
                app.status = "Refreshing…".into();
            }
        }

        Action::None | Action::WaitingPrefix => {}
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────────

pub(crate) fn spawn_terminal(app: &mut App, session_key: &str, cwd: std::path::PathBuf, kind: ShellKind) {
    let (cols, rows) = app.last_term_area;
    let size = PtySize {
        rows: rows.max(10),
        cols: cols.max(20),
        pixel_width: 0,
        pixel_height: 0,
    };

    // Build the inner command (claude or shell).
    let inner_cmd: Vec<String> = match kind {
        ShellKind::Claude => app.config.agent.config.spawn_command(false),
        ShellKind::Shell => vec![app.config.shell.command.clone()],
    };

    // Wrap in tmux so the process survives pilot quit.
    // -A: attach if exists, create if not.
    let tmux_name = session_key.replace(':', "_").replace('/', "_");
    let inner_joined = inner_cmd.join(" ");
    let cmd_strs: Vec<String> = vec![
        "tmux".into(),
        "new-session".into(),
        "-A".into(),
        "-s".into(),
        tmux_name,
        inner_joined,
    ];
    let cmd: Vec<&str> = cmd_strs.iter().map(|s| s.as_str()).collect();

    // Hide tmux chrome — no status bar.
    let _ = std::process::Command::new("tmux")
        .args(["set-option", "-g", "status", "off"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // Mark that Claude has been used in this session.
    if matches!(kind, ShellKind::Claude) {
        if let Some(session) = app.sessions.get_mut(session_key) {
            session.had_claude = true;
        }
    }

    // Build env vars for the MCP server to use.
    let session = app.sessions.get(session_key);
    let task_id = session.map(|s| s.task_id.to_string()).unwrap_or_default();
    let pr_number = session
        .and_then(|s| s.primary_task.id.key.rsplit_once('#'))
        .map(|(_, n)| n.to_string())
        .unwrap_or_default();
    let repo = session
        .and_then(|s| s.primary_task.repo.as_ref())
        .cloned()
        .unwrap_or_default();

    let env = vec![
        ("PILOT_SESSION".to_string(), task_id),
        ("PILOT_PR_NUMBER".to_string(), pr_number),
        ("PILOT_REPO".to_string(), repo),
    ];

    // For Claude sessions, write .mcp.json in the worktree so Claude discovers our MCP server.
    if matches!(kind, ShellKind::Claude) {
        if let Ok(mcp_server_path) = which_pilot_mcp() {
            let mcp_json = serde_json::json!({
                "mcpServers": {
                    "pilot": {
                        "command": mcp_server_path,
                        "args": [],
                        "env": {
                            "PILOT_SESSION": env[0].1.clone(),
                            "PILOT_PR_NUMBER": env[1].1.clone(),
                            "PILOT_REPO": env[2].1.clone(),
                        }
                    }
                }
            });
            let mcp_path = cwd.join(".mcp.json");
            if let Err(e) = std::fs::write(&mcp_path, serde_json::to_string_pretty(&mcp_json).unwrap()) {
                tracing::warn!("Failed to write .mcp.json: {e}");
            } else {
                tracing::info!("Wrote .mcp.json to {}", mcp_path.display());
                // Add .mcp.json to .gitignore if not already there.
                let gitignore = cwd.join(".gitignore");
                if let Ok(content) = std::fs::read_to_string(&gitignore) {
                    if !content.contains(".mcp.json") {
                        let _ = std::fs::write(&gitignore, format!("{content}\n.mcp.json\n"));
                    }
                }
            }
        }
    }

    let term_result = TermSession::spawn(&cmd, size, Some(&cwd), env);

    match term_result {
        Ok(term) => {
            app.terminals.insert(session_key.to_string(), term);
            app.terminal_kinds.insert(session_key.to_string(), kind);
            if !app.tab_order.contains(&session_key.to_string()) {
                app.tab_order.push(session_key.to_string());
            }
            app.active_tab = app.tab_order.len() - 1;
            if let Some(session) = app.sessions.get_mut(session_key) {
                session.state = SessionState::Working;
            }
            // Auto-split: if no terminal pane exists, split the detail pane.
            let has_term_pane = app
                .panes
                .find_pane(|c| matches!(c, PaneContent::Terminal(_)))
                .is_some();
            if !has_term_pane {
                // Find the detail pane and split it.
                if let Some(detail_id) = app
                    .panes
                    .find_pane(|c| matches!(c, PaneContent::Detail(_)))
                {
                    app.panes.focused = detail_id;
                    app.panes
                        .split_vertical(PaneContent::Terminal(session_key.to_string()));
                }
            } else {
                // Update existing terminal pane to show new session.
                if let Some(term_id) = app
                    .panes
                    .find_pane(|c| matches!(c, PaneContent::Terminal(_)))
                {
                    app.panes
                        .set_content(term_id, PaneContent::Terminal(session_key.to_string()));
                }
            }
            app.key_mode = KeyMode::Terminal;
            app.status = match kind {
                ShellKind::Claude => format!("Claude Code started in {}", cwd.display()),
                ShellKind::Shell => format!("Shell started in {}", cwd.display()),
            };
        }
        Err(e) => {
            if let Some(session) = app.sessions.get_mut(session_key) {
                session.state = SessionState::Active;
            }
            app.status = format!("Terminal spawn failed: {e}");
            tracing::error!("Terminal spawn failed: {e}");
        }
    }
}

/// Construct a context prompt from selected comments and paste it into the
/// active Claude terminal session.
fn fix_or_reply_with_claude(app: &mut App, action_tx: &mpsc::UnboundedSender<Action>, mode: &str) {
    // Debounce: ignore if we sent something in the last 1.5s.
    let now = std::time::Instant::now();
    if let Some(last) = app.last_claude_send {
        if now.duration_since(last).as_millis() < 1500 {
            app.status = "Wait — Claude was just fed. Press again in a sec.".into();
            return;
        }
    }

    let Some(session_key) = app.selected_session_key() else {
        app.status = "No session selected".into();
        return;
    };

    // If no terminal running, open one first and queue the prompt.
    let just_spawned = !app.terminals.contains_key(&session_key);
    if just_spawned {
        handle_action(app, Action::OpenSession(ShellKind::Claude), action_tx);
    }

    let Some(session) = app.sessions.get(&session_key) else {
        return;
    };

    let task = &session.primary_task;

    // Detect what needs fixing: conflicts, CI failure, review comments, or combination.
    let ci_failing = task.ci == pilot_core::CiStatus::Failure;
    let has_failed_checks = task.checks.iter().any(|c| c.status == pilot_core::CiStatus::Failure);
    let has_conflicts = task.has_conflicts;

    // Gather selected comments (or all unread if none selected).
    let indices: Vec<usize> = if app.selected_comments.is_empty() {
        (0..session.unread_count()).collect()
    } else {
        let mut v: Vec<usize> = app.selected_comments.iter().copied().collect();
        v.sort();
        v
    };

    let has_comments = !indices.is_empty();

    // Must have SOMETHING to fix.
    if !ci_failing && !has_comments && !has_conflicts {
        app.status = "Nothing to fix — CI green, no conflicts, no unread comments".into();
        return;
    }

    // Build context-aware prompt.
    let mut prompt = String::new();

    // Determine the task description based on what's broken.
    let mut issues: Vec<&str> = vec![];
    if has_conflicts { issues.push("resolve merge conflicts"); }
    if ci_failing { issues.push("fix CI failures"); }
    if has_comments && mode == "fix" { issues.push("address review comments"); }
    if has_comments && mode == "reply" { issues.push("draft replies to review comments"); }

    let action_word = if issues.is_empty() {
        "investigate this PR"
    } else {
        // Leak is fine — this is a one-off prompt string.
        &*issues.join(" AND ").leak()
    };

    prompt.push_str(&format!("# Task: {action_word}\n\n"));
    prompt.push_str("## PR\n\n");
    prompt.push_str(&format!("- **Title:** {}\n", task.title));
    prompt.push_str(&format!("- **URL:** {}\n", task.url));
    if let Some(ref branch) = task.branch {
        prompt.push_str(&format!("- **Branch:** `{branch}`\n"));
    }
    prompt.push_str(&format!("- **CI:** {:?}\n", task.ci));
    prompt.push_str(&format!("- **Review:** {:?}\n", task.review));
    if has_conflicts {
        prompt.push_str("- **Merge conflicts:** YES\n");
    }

    // Merge conflict instructions.
    if has_conflicts {
        prompt.push_str("\n## Merge Conflicts\n\n");
        prompt.push_str("This PR has merge conflicts with the base branch.\n");
        prompt.push_str("1. Run `git fetch origin main && git rebase origin/main`\n");
        prompt.push_str("2. Resolve any conflicts\n");
        prompt.push_str("3. Use `pilot_push` to force-push the rebased branch\n\n");
    }

    // CI failure details.
    if ci_failing || has_failed_checks {
        prompt.push_str("\n## CI Failures\n\n");
        let failed: Vec<_> = task.checks.iter()
            .filter(|c| c.status == pilot_core::CiStatus::Failure)
            .collect();
        if failed.is_empty() {
            prompt.push_str("CI is failing but no individual check details available.\n");
            prompt.push_str("Use `pilot_get_pr_state` to fetch the latest CI status and logs.\n");
        } else {
            for check in &failed {
                prompt.push_str(&format!("- **FAILED: {}**", check.name));
                if let Some(ref url) = check.url {
                    prompt.push_str(&format!(" — [view logs]({url})"));
                }
                prompt.push_str("\n");
            }
        }
        prompt.push_str("\nInvestigate the failing checks, read the logs, and fix the code.\n");
    }

    // Review comments (if any).
    if has_comments {
        prompt.push_str("\n## Review Comments\n\n");
        for &idx in &indices {
            if let Some(activity) = session.activity.get(idx) {
                let kind_label = match activity.kind {
                    pilot_core::ActivityKind::Comment => "Comment",
                    pilot_core::ActivityKind::Review => "Review",
                    pilot_core::ActivityKind::StatusChange => "Status",
                    pilot_core::ActivityKind::CiUpdate => "CI",
                };
                let quoted_body = activity.body.lines()
                    .map(|line| format!("> {line}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                prompt.push_str(&format!(
                    "### {kind_label} from {} ({})\n\n{quoted_body}\n\n",
                    activity.author,
                    pilot_core::time::time_ago(&activity.created_at),
                ));
            }
        }
    }

    prompt.push_str("\n## Instructions\n\n");
    prompt.push_str("**IMPORTANT:** You have access to MCP tools provided by pilot. ");
    prompt.push_str("Use these instead of raw `git` or `gh` commands so the user can ");
    prompt.push_str("review your actions before they execute:\n\n");
    prompt.push_str("- `pilot_get_pr_state` — fetch live PR state (CI, reviews) before/after changes\n");
    prompt.push_str("- `pilot_push` — push your commits (user will confirm)\n");
    prompt.push_str("- `pilot_reply` — post a comment reply (user will confirm)\n");
    prompt.push_str("- `pilot_resolve_thread` — mark a review thread resolved\n");
    prompt.push_str("- `pilot_request_changes` — request changes\n");
    prompt.push_str("- `pilot_approve` — approve PR\n");
    prompt.push_str("- `pilot_merge` — merge PR\n\n");

    if mode == "fix" {
        prompt.push_str("After making code changes:\n");
        prompt.push_str("1. Make the changes locally (you're already in the worktree)\n");
        prompt.push_str("2. Use `pilot_push` to push (NOT `git push`)\n");
        prompt.push_str("3. Optionally use `pilot_get_pr_state` to confirm CI passed\n");
        prompt.push_str("4. Use `pilot_reply` to respond to the comments\n");
    } else {
        prompt.push_str("Draft concise, professional replies. ");
        prompt.push_str("Use `pilot_reply` to post each reply (the user will review before posting).\n");
    }

    // Write context to file with timestamp to avoid race conditions.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let context_dir = std::path::PathBuf::from(&home).join(".pilot").join("context");
    let _ = std::fs::create_dir_all(&context_dir);
    let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
    let safe_key = session_key.replace(':', "_").replace('/', "_");
    let context_file = context_dir.join(format!("{safe_key}_{timestamp}.md"));
    if let Err(e) = std::fs::write(&context_file, &prompt) {
        app.status = format!("Failed to write context file: {e}");
        return;
    }
    // Also write a stable "latest" symlink for pilot_get_context.
    let latest = context_dir.join(format!("{safe_key}.md"));
    let _ = std::fs::remove_file(&latest);
    let _ = std::fs::copy(&context_file, &latest);

    // Queue the prompt — it will be injected when Claude is idle.
    tracing::info!("Queued prompt for {session_key} ({} bytes)", prompt.len());
    app.pending_prompts.insert(session_key.clone(), prompt);
    app.selected_comments.clear();
    app.last_claude_send = Some(now);

    if let Some(idx) = app.tab_order.iter().position(|k| k == &session_key) {
        app.active_tab = idx;
    }
    app.key_mode = KeyMode::Terminal;

    {
        let n = indices.len();
        app.status = format!(
            "Queued {n} comment{} for Claude to {mode}",
            if n == 1 { "" } else { "s" }
        );
    }
}


/// Sync sidebar selection to the active tab.
fn sync_selected_to_tab(app: &mut App) {
    if let Some(tab_key) = app.active_tab_key().cloned() {
        if let Some(idx) = app.session_order.iter().position(|k| k == &tab_key) {
            app.selected = idx;
        }
        update_detail_pane(app);
    }
}

/// Update the detail pane content to match the selected session.
pub(crate) fn update_detail_pane(app: &mut App) {
    if let Some(key) = app.selected_session_key() {
        if let Some(detail_id) = app
            .panes
            .find_pane(|c| matches!(c, PaneContent::Detail(_)))
        {
            app.panes
                .set_content(detail_id, PaneContent::Detail(key.clone()));
        }

        // Auto-reattach: if a tmux session exists but no terminal in pilot, reattach.
        if !app.terminals.contains_key(&key) {
            let tmux_name = key.replace(':', "_").replace('/', "_");
            let has_tmux = std::process::Command::new("tmux")
                .args(["has-session", "-t", &tmux_name])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if has_tmux {
                if let Some(session) = app.sessions.get(&key) {
                    let cwd = session.worktree_path.clone()
                        .unwrap_or_else(|| {
                            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                            std::path::PathBuf::from(home)
                        });
                    tracing::info!("Auto-reattaching tmux session: {tmux_name}");
                    spawn_terminal(app, &key, cwd, ShellKind::Claude);
                }
            }
        }

        // Update terminal pane if the session has a running terminal.
        if app.terminals.contains_key(&key) {
            if let Some(term_id) = app
                .panes
                .find_pane(|c| matches!(c, PaneContent::Terminal(_)))
            {
                app.panes
                    .set_content(term_id, PaneContent::Terminal(key));
            }
        }
    }
}



/// Find the pilot-mcp-server binary. Check next to the pilot binary first, then PATH.
fn which_pilot_mcp() -> Result<String, ()> {
    // Check next to current executable.
    if let Ok(exe) = std::env::current_exe() {
        let Some(parent) = exe.parent() else { return Err(()) };
        let sibling = parent.join("pilot-mcp-server");
        if sibling.exists() {
            return Ok(sibling.to_string_lossy().to_string());
        }
    }
    // Check PATH.
    let output = std::process::Command::new("which")
        .arg("pilot-mcp-server")
        .output();
    if let Ok(o) = output {
        if o.status.success() {
            let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(path);
            }
        }
    }
    tracing::warn!("pilot-mcp-server binary not found");
    Err(())
}

/// Start the Unix socket listener for MCP confirmations.
/// Runs as a tokio task, sends McpConfirmRequest actions through the channel.
pub fn start_mcp_socket_listener(
    action_tx: mpsc::UnboundedSender<Action>,
    monitored_sessions: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let socket_path = std::path::PathBuf::from(&home).join(".pilot").join("pilot.sock");

    // Remove stale socket.
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::create_dir_all(socket_path.parent().unwrap());

    tokio::spawn(async move {
        let listener = match tokio::net::UnixListener::bind(&socket_path) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("Failed to bind MCP socket at {}: {e}", socket_path.display());
                return;
            }
        };
        tracing::info!("MCP socket listening at {}", socket_path.display());

        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("MCP socket accept error: {e}");
                    continue;
                }
            };

            let tx = action_tx.clone();
            let monitored = Arc::clone(&monitored_sessions);
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};

                // Read length-prefixed JSON request.
                let mut len_buf = [0u8; 4];
                if stream.read_exact(&mut len_buf).await.is_err() {
                    return;
                }
                let len = u32::from_be_bytes(len_buf) as usize;
                let mut buf = vec![0u8; len];
                if stream.read_exact(&mut buf).await.is_err() {
                    return;
                }

                let req: serde_json::Value = match serde_json::from_slice(&buf) {
                    Ok(v) => v,
                    Err(_) => return,
                };

                let tool = req.get("tool").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                let display = req.get("display").and_then(|v| v.as_str()).unwrap_or(&tool).to_string();
                let session_id = req.get("session_id").and_then(|v| v.as_str()).unwrap_or("").to_string();

                // Auto-approve pilot_push for monitored sessions.
                let auto_approve = tool == "pilot_push"
                    && !session_id.is_empty()
                    && monitored.lock().unwrap().contains(&session_id);

                let approved = if auto_approve {
                    tracing::info!("Monitor: auto-approving {tool} for {session_id}");
                    // Notify the monitor state machine that a push happened.
                    let _ = tx.send(Action::MonitorTick { session_key: session_id });
                    true
                } else {
                    // Create a sync channel for the response (MCP handler blocks on this).
                    let (resp_tx, resp_rx) = std::sync::mpsc::channel::<bool>();

                    // Send to the TUI.
                    let _ = tx.send(Action::McpConfirmRequest {
                        tool: tool.clone(),
                        display,
                        response_tx: resp_tx,
                    });

                    // Wait for user approval (blocking on sync channel, in a tokio task).
                    tokio::task::spawn_blocking(move || {
                        resp_rx.recv_timeout(std::time::Duration::from_secs(120)).unwrap_or(false)
                    })
                    .await
                    .unwrap_or(false)
                };

                // Send response back to MCP server.
                let resp = serde_json::json!({ "approved": approved, "message": "" });
                let resp_bytes = serde_json::to_vec(&resp).unwrap();
                let len = (resp_bytes.len() as u32).to_be_bytes();
                let _ = stream.write_all(&len).await;
                let _ = stream.write_all(&resp_bytes).await;
                let _ = stream.flush().await;
            });
        }
    });
}

/// A pending action from the MCP server awaiting user confirmation.
#[derive(Debug, Clone)]
pub struct PendingMcpAction {
    pub tool: String,
    pub display: String,
    /// Channel to send approval/rejection back to the MCP server.
    pub response_tx: Option<std::sync::mpsc::Sender<bool>>,
}

/// Persist all sessions to SQLite.
fn save_all_sessions(app: &App) {
    let mut saved = 0;
    let mut errors = 0;
    for (key, session) in &app.sessions {
        let json = serde_json::to_string(session).ok();
        match app.store.save_session(&pilot_store::SessionRecord {
            task_id: key.clone(),
            seen_count: session.seen_count as i64,
            last_viewed_at: session.last_viewed_at,
            created_at: session.created_at,
            session_json: json,
            metadata: None,
        }) {
            Ok(()) => saved += 1,
            Err(e) => {
                tracing::error!("Failed to save session {key}: {e}");
                errors += 1;
            }
        }
    }
    tracing::info!("Saved {saved} sessions ({errors} errors)");
}

/// Reset detail pane state when switching sessions and auto-update detail.
fn reset_detail_state(app: &mut App) {
    app.detail_scroll = 0;
    app.detail_cursor = 0;
    app.selected_comments.clear();
    update_detail_pane(app);
}

/// Determine the key mode based on what the focused pane contains.
fn determine_mode(app: &App) -> KeyMode {
    // If we're in terminal mode, stay there unless explicitly exited.
    // This prevents pane operations from accidentally resetting the mode.
    if app.key_mode == KeyMode::Terminal {
        // Check if the terminal is still alive.
        if let Some(key) = app.active_tab_key() {
            if app.terminals.contains_key(key) {
                return KeyMode::Terminal;
            }
        }
    }
    // Default: use pane content to determine mode.
    match app.panes.focused_content() {
        Some(PaneContent::Terminal(_)) => KeyMode::Terminal,
        Some(PaneContent::Detail(_)) => KeyMode::Detail,
        _ => KeyMode::Normal,
    }
}

/// Get a PaneContent::Detail for the currently selected session.
fn current_detail_content(app: &App) -> PaneContent {
    app.selected_session_key()
        .map(|k| PaneContent::Detail(k.clone()))
        .unwrap_or(PaneContent::Empty)
}
