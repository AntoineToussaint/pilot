//! Pure application state — the "Model" in MVC / "State" in Elm architecture.
//!
//! Contains ALL data that business logic acts on: sessions, panes, input mode,
//! selection, filters, UI flags, caches. Excludes:
//! - `TerminalManager` (owns live PTYs — !Send, non-serializable)
//! - `Store` trait object (IO)
//! - tokio channels / `Notify` / shared `Arc<Mutex<...>>` handles
//!
//! The goal: `State` can be constructed in a test without any IO, and a
//! `reduce(&mut State, Action) -> Vec<Command>` function can be tested end-to-end
//! by asserting on the mutated state and the emitted commands.
//!
//! IO lives in the `App` shell, which holds a `State` plus the resources above.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use pilot_config::Config;

use crate::agent_state::AgentState;
use crate::input::InputMode;
use crate::pane::PaneManager;
use crate::session_manager::SessionManager;

/// A read-only snapshot of the live terminal map. The shell refreshes this
/// whenever terminals are spawned, closed, or reaped, so reduce can look at
/// tab order / active tab / key set without touching `TerminalManager` (whose
/// live `TermSession` values are !Send and hold PTY fds).
#[derive(Debug, Clone, Default)]
pub struct TerminalIndex {
    pub tab_order: Vec<String>,
    pub active_tab: Option<usize>,
    pub keys: std::collections::BTreeSet<String>,
}

impl TerminalIndex {
    pub fn contains_key(&self, k: &str) -> bool {
        self.keys.contains(k)
    }
    pub fn active_key(&self) -> Option<&String> {
        self.active_tab.and_then(|i| self.tab_order.get(i))
    }
}

/// The entire pure model. `reduce` mutates this and returns `Vec<Command>`.
///
/// Fields are `pub(crate)` rather than `pub` so that only `reduce`, the app
/// shell, and the renderer can mutate them directly. That's the contract:
/// business logic goes through `reduce(state, action)` or a helper method
/// defined here, never a naked field write from, say, `ui.rs`. See the
/// memory `project_mvc_refactor.md` for the full rules.
pub struct State {
    // ── Domain data ──
    pub(crate) sessions: SessionManager,
    pub(crate) selected: usize,
    pub(crate) panes: PaneManager,

    // ── Read-only projection of terminal state ──
    /// Refreshed by the shell after every TerminalManager mutation.
    pub(crate) terminal_index: TerminalIndex,

    // ── Input mode / overlays ──
    pub(crate) input_mode: InputMode,
    pub(crate) show_help: bool,

    // ── Search / filter ──
    pub(crate) search_active: bool,
    pub(crate) search_query: String,
    pub(crate) filtered_keys: Option<Vec<String>>,
    pub(crate) activity_days_filter: u32,

    // ── Detail pane ──
    pub(crate) selected_comments: HashSet<usize>,
    pub(crate) detail_cursor: usize,
    pub(crate) detail_scroll: u16,
    /// (session_key, started_viewing_at) — drives auto-mark-read.
    pub(crate) viewing_since: Option<(String, Instant)>,

    // ── Status / notifications ──
    pub(crate) notifications: Vec<String>,
    pub(crate) status: String,

    // ── Layout ──
    pub(crate) last_term_area: (u16, u16),
    pub(crate) sidebar_pct: u16,
    pub(crate) drag_resize: bool,

    // ── Quit / merge confirmation ──
    pub(crate) should_quit: bool,
    pub(crate) quit_pending: bool,
    pub(crate) merge_pending: Option<String>,

    // ── First-poll bookkeeping ──
    pub(crate) loaded: bool,
    pub(crate) purged_stale: bool,
    pub(crate) first_poll_keys: HashSet<String>,
    /// True if any ProviderError arrived before the purge window closed.
    /// When set, we skip the stale purge — an error means the fresh result
    /// set is incomplete, and purging stored sessions against it would
    /// delete live PRs that just weren't in a truncated response.
    pub(crate) first_poll_had_errors: bool,
    pub(crate) tick_count: u64,

    // ── Sidebar tree collapse state ──
    pub(crate) collapsed_repos: HashSet<String>,
    pub(crate) collapsed_sessions: HashSet<String>,

    // ── Pickers / text inputs ──
    pub(crate) picker: Option<crate::picker::PickerState>,
    pub(crate) collaborators_cache: HashMap<String, Vec<String>>,
    pub(crate) new_session_input: Option<String>,
    pub(crate) quick_reply_input: Option<(String, String, usize)>,

    // ── Claude session heuristics ──
    pub(crate) last_claude_send: Option<Instant>,
    pub(crate) notified_asking: HashSet<String>,
    pub(crate) pending_prompts: HashMap<String, String>,
    pub(crate) agent_states: HashMap<String, AgentState>,
    pub(crate) default_branch_cache: HashMap<String, String>,

    /// Tmux session names detected by the shell (periodic `tmux list-sessions`).
    /// Used to show a "session alive on disk" indicator in the sidebar for
    /// pilot sessions whose tmux process is still running but not attached.
    pub(crate) live_tmux_sessions: HashSet<String>,

    // ── Static config / identity ──
    pub(crate) config: Config,
    pub(crate) username: String,
    pub(crate) credentials_ok: bool,
    /// Canonical mirror of the shared `Arc<Mutex<HashSet<String>>>`.
    /// Reduce mutates this field; the shell writes through to the Arc.
    pub(crate) monitored_sessions: HashSet<String>,
}

impl State {
    /// Construct a default state (used for tests). Production code builds
    /// state inside `App::new` with real `Config` / `username`.
    #[cfg(test)]
    pub fn new_for_test() -> Self {
        use pilot_config::Config;
        Self::with_config(Config::default(), String::new())
    }

    pub fn with_config(config: Config, username: String) -> Self {
        Self {
            sessions: SessionManager::new(),
            selected: 0,
            panes: PaneManager::default_layout(),
            terminal_index: TerminalIndex::default(),
            input_mode: InputMode::Normal,
            show_help: false,
            search_active: false,
            search_query: String::new(),
            filtered_keys: None,
            activity_days_filter: config.display.activity_days,
            selected_comments: HashSet::new(),
            detail_cursor: 0,
            detail_scroll: 0,
            viewing_since: None,
            notifications: Vec::new(),
            status: String::new(),
            last_term_area: (80, 24),
            sidebar_pct: 30,
            drag_resize: false,
            should_quit: false,
            quit_pending: false,
            merge_pending: None,
            loaded: false,
            purged_stale: false,
            first_poll_keys: HashSet::new(),
            first_poll_had_errors: false,
            tick_count: 0,
            collapsed_repos: HashSet::new(),
            collapsed_sessions: HashSet::new(),
            picker: None,
            collaborators_cache: HashMap::new(),
            new_session_input: None,
            quick_reply_input: None,
            last_claude_send: None,
            notified_asking: HashSet::new(),
            pending_prompts: HashMap::new(),
            agent_states: HashMap::new(),
            default_branch_cache: HashMap::new(),
            live_tmux_sessions: HashSet::new(),
            config,
            username,
            credentials_ok: false,
            monitored_sessions: HashSet::new(),
        }
    }
}

/// Arc-mutex handle to the shared monitored set (helper tasks can inspect
/// it without going through the action loop). Reduce never touches this —
/// it updates `State::monitored_sessions` and the shell writes through.
///
/// Uses `parking_lot::Mutex` (infallible `.lock()`, no poisoning, faster
/// under contention than `std::sync::Mutex`).
pub type SharedMonitoredSessions = Arc<parking_lot::Mutex<HashSet<String>>>;
