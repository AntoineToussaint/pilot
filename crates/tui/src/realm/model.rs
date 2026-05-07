//! `Model` — the realm-side replacement for pilot's `App` struct.
//!
//! ## Architecture
//!
//! Panes (Sidebar / Right / Terminals) are **not** mounted into the
//! tuirealm `Application`. They live as typed fields on `Model` and
//! we drive their `view`/`on_event`/`handle_key` directly. tuirealm's
//! `Application` only owns **modals** — that's where its mount/unmount
//! + Z-stack semantics actually pay off.
//!
//! Why: pilot's panes are persistently visible, mutate often, and the
//! orchestrator needs typed handles to drain queued commands. Mounting
//! them via `app.mount(id, Box::new(pane))` hides the concrete type
//! behind `dyn AppComponent` and forces awkward attribute-based
//! round-trips for the simplest "give me the queued commands" calls.
//! Holding them as fields is the cleaner shape.
//!
//! ## Modal stack
//!
//! Modals do go through `Application`. We track a `Vec<Id>` so multi-
//! modal stacking (rare) works, and call `app.active(&id)` whenever
//! the top changes. Modal payloads come back as `Msg`s from
//! `app.tick(...)` and `Model::update` decides what to do.

use crate::realm::components::right::Right;
use crate::realm::components::sidebar::Sidebar;
use crate::realm::components::splash::Splash;
use crate::realm::components::terminals::Terminals;
use crate::realm::keymap::realm_key_to_crossterm;
use crate::realm::UserEvent;
use crate::PaneId;
use pilot_ipc::{Client, Command as IpcCommand, Event as IpcEvent};
use std::sync::mpsc;
use std::time::Duration;
use tuirealm::application::{Application, PollStrategy};
use tuirealm::event::{Event as RealmEvent, Key, KeyEvent as RealmKey, KeyModifiers};
use tuirealm::listener::{EventListenerCfg, Poll, PortError, PortResult};
use tuirealm::ratatui::layout::{Constraint, Direction, Layout, Rect};
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, Borders};
use tuirealm::terminal::{CrosstermTerminalAdapter, TerminalAdapter};

const SIDEBAR_PID: PaneId = PaneId::new(1);
const RIGHT_PID: PaneId = PaneId::new(2);
const TERMINALS_PID: PaneId = PaneId::new(3);

/// Component IDs for modal-side mounts only. Pane access is via
/// typed fields, so panes don't appear here.
#[derive(Debug, Eq, PartialEq, Clone, Hash)]
pub enum Id {
    Splash,
    Help,
    Error,
    Polling,
    Reply,
    /// Single-line input prompt for naming a brand-new pre-PR
    /// workspace. Submit → `Command::CreateWorkspace { name }`.
    NewWorkspace,
    /// Picker for selecting an editor when 2+ are detected.
    /// Submit → `editors::launch(template, worktree)`.
    Editor,
    /// Active setup-wizard step. Each transition unmounts the
    /// previous component at this id and mounts the next; only one
    /// setup step is ever live.
    Setup,
}

/// App-level message vocabulary for modals + globals.
#[derive(Debug, PartialEq, Clone)]
pub enum Msg {
    SplashConfirmed,
    AppClose,
    Confirmed(bool),
    InputSubmitted(String),
    TextareaSubmitted(String),
    ChoicePicked(Vec<usize>),
    ChoiceRefresh,
    ChoiceBack,
    LoadingResolved(PayloadCarrier),
    PollingError((String, String, String, String)),
    PollingTimeout,
    PollingEmptyInbox(Vec<String>),
    ModalDismissed,
    /// Sidebar / Right / Terminals routes — kept in case a future
    /// pane goes through tuirealm. Today panes drain themselves
    /// directly inside the orchestrator's pane-dispatch path.
    SidebarCmds,
    RightCmds,
    TerminalCmds,
}

/// Wrapper that lets us put a non-`PartialEq` payload inside `Msg`.
#[derive(Clone)]
pub struct PayloadCarrier(
    pub std::sync::Arc<std::sync::Mutex<Option<Box<dyn std::any::Any + Send>>>>,
);

impl PartialEq for PayloadCarrier {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for PayloadCarrier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PayloadCarrier(<opaque>)")
    }
}

impl PayloadCarrier {
    pub fn take(&self) -> Option<Box<dyn std::any::Any + Send>> {
        self.0.lock().ok().and_then(|mut g| g.take())
    }
}

/// Which pane has focus when no modal is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneFocus {
    Sidebar,
    Right,
    Terminals,
}

impl PaneFocus {
    fn next(self) -> Self {
        match self {
            PaneFocus::Sidebar => PaneFocus::Right,
            PaneFocus::Right => PaneFocus::Terminals,
            PaneFocus::Terminals => PaneFocus::Sidebar,
        }
    }
}

/// Top-level application state.
pub struct Model<T: TerminalAdapter> {
    pub app: Application<Id, Msg, UserEvent>,
    pub terminal: T,
    /// Z-stack of modal ids — top is rendered last + receives input.
    pub modal_stack: Vec<Id>,
    /// Which pane has focus when no modal is active.
    focus: PaneFocus,
    /// Three pane wrappers held as typed fields so the orchestrator
    /// can call `.drain_cmds()` etc. directly. The wrappers also
    /// track their own `focused: bool` flag, which we keep in sync
    /// via `set_focus_attr()`.
    sidebar: Sidebar,
    right: Right,
    terminals: Terminals,
    /// IPC client for forwarding pane-emitted commands to the daemon.
    pub client: Client,
    pub redraw: bool,
    pub quit: bool,
    /// In-flight setup wizard. When `Some`, splash/choice/loading
    /// messages route through the runner's state machine instead of
    /// the generic post-splash path. `None` once setup completes (or
    /// the user cancels).
    setup_runner: Option<crate::setup_flow::SetupRunner>,
    /// Cached setup inputs — populated on first launch by
    /// `main::run_embedded_realm` so the wizard can be re-opened
    /// mid-session (key `,`) for adding repos / agents without
    /// re-detecting from scratch. Refresh inside the wizard via `r`.
    setup_inputs:
        Option<(crate::setup::SetupReport, std::sync::Arc<Vec<Box<dyn pilot_core::ScopeSource>>>)>,
    /// Last-known persisted setup. Cached at startup + after every
    /// successful wizard run. Used by partial flows from the
    /// Settings palette to pre-seed the SetupRunner with existing
    /// state so "Edit filters for github" doesn't lose the user's
    /// linear config.
    persisted_setup: Option<pilot_core::PersistedSetup>,
    /// Items behind the active SettingsMenu picker. Choice gives us
    /// indices; we map them back to actions here.
    settings_actions: Vec<SettingsAction>,
    /// Editors detected on PATH at startup + any custom entries
    /// from `~/.pilot/config.yaml`. Drives the `E` open-in-editor
    /// shortcut. Empty when no editor is installed (config error
    /// surfaces as a footer notice when the user hits `E`).
    editors: Vec<crate::editors::EditorTemplate>,
    /// Items behind the active editor picker (when `E` finds 2+
    /// editors). Same shape as `settings_actions`.
    editor_choices: Vec<crate::editors::EditorTemplate>,
    /// Hook invoked exactly once when setup finishes successfully.
    /// `main.rs::run_embedded_realm` installs this so the polling
    /// loop kicks off with the user's selections.
    on_setup_complete:
        Option<Box<dyn FnOnce(crate::setup_flow::SetupOutcome) + Send>>,
    /// Sender into the custom `ChannelPort`. Run loop pushes
    /// keyboard events here when a modal is up so Application's
    /// listener thread picks them up + dispatches.
    modal_event_tx: mpsc::Sender<RealmEvent<UserEvent>>,
    /// First-press time of the q-q double-tap. The first `q` outside
    /// a terminal arms the latch; a second `q` within
    /// `Q_DOUBLE_TAP_WINDOW` quits. Any other key disarms.
    q_armed_at: Option<std::time::Instant>,
    /// `]]` escape from the terminal pane: first press of the escape
    /// char is recorded; a second within the window kicks focus
    /// back to the sidebar instead of forwarding to the PTY.
    escape_armed_at: Option<std::time::Instant>,
    /// Pending `--workspace` / `--session` preselect from the CLI.
    /// Applied after the daemon's first Snapshot — by then the
    /// sidebar has the full workspace list and `focus_workspace_key`
    /// can land. Cleared once applied (one-shot).
    preselect: Option<Preselect>,
    /// Width of the sidebar column as a percentage of total width.
    /// Adjustable via `Shift-Left`/`Shift-Right` (and mouse drag);
    /// clamped to `SPLIT_RANGE`.
    sidebar_pct: u16,
    /// Height of the right-top (activity) row as a percentage of the
    /// right column. Adjustable via `Shift-Up`/`Shift-Down`; clamped
    /// to `SPLIT_RANGE`.
    right_top_pct: u16,
    /// Workspace key the reply textarea (if mounted) is targeting.
    /// Set by `mount_reply`; consumed by `Msg::TextareaSubmitted` to
    /// build the `Command::PostReply` payload.
    pending_reply: Option<pilot_core::SessionKey>,
    /// First-poll progress modal. Set by the on-setup-complete hook
    /// (and the returning-user kickoff path) so users see "Pulling
    /// from github + linear…" instead of an empty sidebar while the
    /// initial poll cycle runs. Cleared on first `WorkspaceUpserted`,
    /// timeout, or any-key dismiss.
    polling: Option<crate::realm::components::polling::Polling>,
    /// Last `tick_direct` instant — drives spinner cadence + timeout
    /// checks at ~50ms granularity from the run loop.
    polling_last_tick: std::time::Instant,
    /// Most recent footer notice — error, warning, or info. Replaces
    /// the modal-on-every-error UX. Retryable severities auto-fade
    /// after `RETRYABLE_FADE`; permanent + auth stay until cleared
    /// (key `e` or any key).
    notice: Option<crate::realm::components::footer::Notice>,
    /// Last viewport rect captured during `view()`. Mouse handling
    /// uses this to decide which pane a click landed in (without
    /// re-running the layout) and to translate splitter drag deltas
    /// into percentage changes.
    last_area: Rect,
    /// Active drag, if any. The mouse-down location identified one
    /// of the splitters; subsequent Drag events update the
    /// corresponding `_pct` field until the mouse-up.
    active_drag: Option<DragTarget>,
}

/// Which splitter the user is currently dragging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragTarget {
    /// The vertical line between sidebar and the right column.
    SidebarRight,
    /// The horizontal line between activity and terminal stack.
    ActivityTerminals,
}

/// Custom Port that drains events from an `mpsc::Receiver`. Pilot
/// reads crossterm directly in the run loop (so panes get keys
/// without the listener thread / main thread racing for them) and
/// pushes modal-bound events onto the sender. The listener thread
/// polls this port and delivers them to the Application's mounted
/// modal via the usual subscribe path.
struct ChannelPort {
    rx: mpsc::Receiver<RealmEvent<UserEvent>>,
}

impl Poll<UserEvent> for ChannelPort {
    fn poll(&mut self) -> PortResult<Option<RealmEvent<UserEvent>>> {
        match self.rx.try_recv() {
            Ok(ev) => Ok(Some(ev)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(
                PortError::PermanentError("event channel disconnected".into()),
            ),
        }
    }
}

/// One row in the Settings palette (`,` opens this).
#[derive(Debug, Clone)]
pub enum SettingsAction {
    /// Add / remove orgs + repos for a provider.
    EditScopes { provider_id: String, label: String },
    /// Edit role / item-type filters for a provider.
    EditFilters { provider_id: String, label: String },
    /// Re-run the providers picker (enable/disable github / linear / …).
    EditProviders,
    /// Re-run the agents picker.
    EditAgents,
    /// Bail out and run the full splash → providers → agents → … wizard.
    FullSetup,
}

impl SettingsAction {
    fn label(&self) -> String {
        match self {
            Self::EditScopes { label, .. } => format!("Add / remove repos · {label}"),
            Self::EditFilters { label, .. } => format!("Edit roles + filters · {label}"),
            Self::EditProviders => "Edit providers (github / linear / …)".into(),
            Self::EditAgents => "Edit agents (claude / codex / cursor / …)".into(),
            Self::FullSetup => "Run the full setup wizard".into(),
        }
    }
}

/// CLI-driven post-snapshot focus target. Applied once after the
/// first Snapshot so the user lands on a specific workspace +
/// (optionally) session. Used by `--workspace KEY [--session ID]`
/// and the detach flow that re-spawns pilot with these flags.
#[derive(Debug, Clone)]
pub struct Preselect {
    /// Workspace key (e.g. `"github:owner/repo#42"`) to land on.
    pub workspace_key: pilot_core::SessionKey,
    /// Optional session id to focus inside the workspace. Anything
    /// that doesn't parse as a uuid is silently ignored.
    pub session_id_raw: Option<String>,
}

/// How long retryable notices stay visible before fading. Permanent
/// + auth notices ignore this — they stay until dismissed.
const RETRYABLE_FADE: Duration = Duration::from_secs(5);
/// How long info notices ("Spawning shell…", "Setup saved", etc.)
/// stay visible if their triggering event never lands. Longer than
/// retryable so a slow worktree creation doesn't fade mid-flight.
const INFO_FADE: Duration = Duration::from_secs(15);

/// Initial split percentages. Match the legacy defaults so users
/// don't see a jumpy first frame after the migration.
const DEFAULT_SIDEBAR_PCT: u16 = 40;
const DEFAULT_RIGHT_TOP_PCT: u16 = 25;
/// Min/max for either splitter (percentage). Keeps every pane
/// usable — no zero-height activity feed, no sliver sidebar.
const SPLIT_MIN: u16 = 15;
const SPLIT_MAX: u16 = 80;
/// Step size per Shift-arrow tap. Picked so 4-5 taps cover a useful
/// range and a single tap is visibly more than a shimmer.
const SPLIT_STEP: i16 = 3;

/// How long the first `q` stays armed waiting for the second tap.
const Q_DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(800);

/// Escape-char for the terminal-pane breakout sequence. Two
/// consecutive presses (with no intervening non-`]` key) returns
/// focus to the sidebar instead of forwarding to the PTY.
const TERMINAL_ESCAPE_CHAR: char = ']';

impl Model<CrosstermTerminalAdapter> {
    pub fn new(client: Client) -> anyhow::Result<Self> {
        // Build the modal-event channel + register a custom Port for
        // it. Crossterm input is read directly in the run loop —
        // there's no `crossterm_input_listener` here, so the listener
        // thread doesn't race the main thread for keystrokes.
        let (modal_event_tx, modal_event_rx) = mpsc::channel();
        let app: Application<Id, Msg, UserEvent> = Application::init(
            EventListenerCfg::default()
                .add_port(
                    Box::new(ChannelPort { rx: modal_event_rx }),
                    Duration::from_millis(10),
                    16,
                )
                .tick_interval(Duration::from_millis(50)),
        );
        // Splash is mounted lazily by `start_setup_wizard`. Returning
        // users (with a persisted setup) boot straight to the panes.

        let mut terminal = CrosstermTerminalAdapter::new()?;
        terminal.enable_raw_mode()?;
        terminal.enter_alternate_screen()?;
        // Mouse capture: clicks/drags drive splitter resize +
        // click-to-focus. Native text selection still works in
        // modern terminals via Shift-drag (terminal-side override).
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::EnableMouseCapture,
        );

        let mut model = Self {
            app,
            terminal,
            modal_stack: Vec::new(),
            focus: PaneFocus::Sidebar,
            sidebar: Sidebar::new(SIDEBAR_PID),
            right: Right::new(RIGHT_PID),
            terminals: Terminals::new(TERMINALS_PID),
            client,
            redraw: true,
            quit: false,
            setup_runner: None,
            setup_inputs: None,
            persisted_setup: None,
            settings_actions: Vec::new(),
            editors: Vec::new(),
            editor_choices: Vec::new(),
            on_setup_complete: None,
            modal_event_tx,
            q_armed_at: None,
            escape_armed_at: None,
            preselect: None,
            sidebar_pct: DEFAULT_SIDEBAR_PCT,
            right_top_pct: DEFAULT_RIGHT_TOP_PCT,
            pending_reply: None,
            polling: None,
            polling_last_tick: std::time::Instant::now(),
            notice: None,
            last_area: Rect::default(),
            active_drag: None,
        };
        // Subscribe up-front for both first-run and returning users.
        // First-run gets an empty snapshot before the wizard finishes
        // (no polling has run yet) so nothing flickers in behind the
        // wizard. Subscribe is idempotent on the daemon side.
        let _ = model.client.send(IpcCommand::Subscribe);
        model.set_focus_attr();
        Ok(model)
    }


}

/// Headless constructor: builds the same orchestrator state without
/// touching raw mode / alternate screen / mouse capture, so tests
/// can drive `handle_pane_key` / `handle_daemon_event` against a
/// fake backend.
impl Model<tuirealm::terminal::TestTerminalAdapter> {
    pub fn new_for_test(
        client: Client,
        size: tuirealm::ratatui::layout::Size,
    ) -> anyhow::Result<Self> {
        let (modal_event_tx, modal_event_rx) = mpsc::channel();
        let app: Application<Id, Msg, UserEvent> = Application::init(
            EventListenerCfg::default()
                .add_port(
                    Box::new(ChannelPort { rx: modal_event_rx }),
                    Duration::from_millis(10),
                    16,
                )
                .tick_interval(Duration::from_millis(50)),
        );

        let terminal = tuirealm::terminal::TestTerminalAdapter::new(size)
            .map_err(|e| anyhow::anyhow!("test adapter init: {e:?}"))?;

        Ok(Self {
            app,
            terminal,
            modal_stack: Vec::new(),
            focus: PaneFocus::Sidebar,
            sidebar: Sidebar::new(SIDEBAR_PID),
            right: Right::new(RIGHT_PID),
            terminals: Terminals::new(TERMINALS_PID),
            client,
            redraw: true,
            quit: false,
            setup_runner: None,
            setup_inputs: None,
            persisted_setup: None,
            settings_actions: Vec::new(),
            editors: Vec::new(),
            editor_choices: Vec::new(),
            on_setup_complete: None,
            modal_event_tx,
            q_armed_at: None,
            escape_armed_at: None,
            preselect: None,
            sidebar_pct: DEFAULT_SIDEBAR_PCT,
            right_top_pct: DEFAULT_RIGHT_TOP_PCT,
            pending_reply: None,
            polling: None,
            polling_last_tick: std::time::Instant::now(),
            notice: None,
            last_area: Rect::default(),
            active_drag: None,
        })
    }
}

impl<T: TerminalAdapter> Model<T> {
    /// Pre-load a `--workspace` / `--session` target. Applied once
    /// the first daemon Snapshot lands.
    pub fn with_preselect(mut self, p: Preselect) -> Self {
        self.preselect = Some(p);
        self
    }

    /// Install the on-setup-complete hook before the main loop
    /// starts. `main.rs::run_embedded_realm` uses this to kick off
    /// the polling loop with the user's persisted selections.
    pub fn with_setup_complete_hook(
        mut self,
        hook: Box<dyn FnOnce(crate::setup_flow::SetupOutcome) + Send>,
    ) -> Self {
        self.on_setup_complete = Some(hook);
        self
    }

    /// Trigger the setup wizard. Called from `run_embedded_realm`
    /// when no persisted setup exists, AND from `reopen_setup` when
    /// the user wants to add a repo / agent / scope mid-session.
    /// Mounts the welcome splash; the runner consumes the next
    /// `Msg::SplashConfirmed` and unrolls into Providers / Agents /
    /// Filters / Scopes / Repos.
    pub fn start_setup_wizard(
        &mut self,
        report: crate::setup::SetupReport,
        sources: std::sync::Arc<Vec<Box<dyn pilot_core::ScopeSource>>>,
    ) {
        use tuirealm::subscription::{EventClause, Sub, SubClause};
        self.setup_inputs = Some((report.clone(), sources.clone()));
        self.setup_runner = Some(crate::setup_flow::SetupRunner::new(report, sources));
        let _ = self.app.mount(
            Id::Splash,
            Box::new(Splash::new()),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        let _ = self.app.active(&Id::Splash);
        self.modal_stack.push(Id::Splash);
        self.redraw = true;
    }

    /// Pre-populate the cached setup inputs without launching the
    /// wizard. `run_embedded_realm` calls this for returning users
    /// so the in-session `reopen_setup` path works without re-
    /// running detection.
    pub fn cache_setup_inputs(
        &mut self,
        report: crate::setup::SetupReport,
        sources: std::sync::Arc<Vec<Box<dyn pilot_core::ScopeSource>>>,
    ) {
        self.setup_inputs = Some((report, sources));
    }

    /// Cache the user's existing PersistedSetup so partial flows
    /// from the Settings palette can pre-seed the wizard with
    /// current state instead of starting from defaults.
    pub fn cache_persisted_setup(&mut self, persisted: pilot_core::PersistedSetup) {
        self.persisted_setup = Some(persisted);
    }

    /// Hand in the editors detected at startup. The `E` shortcut
    /// reads from this list; empty list = footer notice on `E`.
    pub fn cache_editors(&mut self, editors: Vec<crate::editors::EditorTemplate>) {
        self.editors = editors;
    }

    /// Open the focused workspace's worktree in an editor. Bound to
    /// `E` from the sidebar. 1 detected editor → launch directly;
    /// 2+ → mount a Choice picker; 0 → footer notice with hint.
    pub fn open_editor(&mut self) {
        use crate::realm::components::footer::{Notice, NoticeSeverity};

        let Some(workspace_key) = self.sidebar.selected_workspace_key().cloned() else {
            return;
        };
        // Resolve worktree path: take the workspace's first session.
        // Empty workspaces have no worktree on disk yet — surface
        // that as a notice.
        let worktree = self
            .sidebar
            .selected_workspace()
            .and_then(|w| w.sessions.first().map(|s| s.worktree_path.clone()));
        let Some(worktree) = worktree else {
            self.notice = Some(Notice::new(
                format!("{workspace_key}: no worktree yet — spawn a session first (s/c/x/u)"),
                NoticeSeverity::Info,
            ));
            self.redraw = true;
            return;
        };

        match self.editors.len() {
            0 => {
                self.notice = Some(Notice::new(
                    "no editor detected — add one under `editors:` in ~/.pilot/config.yaml",
                    NoticeSeverity::Info,
                ));
                self.redraw = true;
            }
            1 => {
                let editor = self.editors[0].clone();
                self.launch_editor(&editor, &worktree);
            }
            _ => {
                self.mount_editor_picker();
            }
        }
    }

    fn mount_editor_picker(&mut self) {
        use crate::realm::components::choice::Choice;
        use tuirealm::subscription::{EventClause, Sub, SubClause};
        let labels: Vec<String> =
            self.editors.iter().map(|e| e.display.clone()).collect();
        self.editor_choices = self.editors.clone();
        let modal = Choice::single("Open in which editor?", labels)
            .title("Open editor")
            .label(|s: &String| s.clone());
        let _ = self.app.mount(
            Id::Editor,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::Editor);
        let _ = self.app.active(&Id::Editor);
        self.redraw = true;
    }

    fn launch_editor(
        &mut self,
        editor: &crate::editors::EditorTemplate,
        worktree: &std::path::Path,
    ) {
        use crate::realm::components::footer::{Notice, NoticeSeverity};
        match crate::editors::launch(editor, worktree) {
            Ok(()) => {
                tracing::info!(
                    editor = %editor.id,
                    worktree = %worktree.display(),
                    "launched editor"
                );
                self.notice = Some(Notice::new(
                    format!("opened {} in {}", worktree.display(), editor.display),
                    NoticeSeverity::Info,
                ));
            }
            Err(e) => {
                tracing::warn!("editor launch failed: {e}");
                self.notice = Some(Notice::new(
                    format!("failed to launch {}: {e}", editor.display),
                    NoticeSeverity::Permanent,
                ));
            }
        }
        self.redraw = true;
    }

    /// Open the in-session Settings palette. Builds a small picker
    /// with actions like "Add a repo (github)" / "Edit agents" /
    /// etc., scoped to the user's current providers. Falls back to
    /// the full wizard when there's no cached persisted setup yet
    /// (first-run path or `--test` mode).
    pub fn open_settings(&mut self) {
        use crate::realm::components::choice::Choice;
        use tuirealm::subscription::{EventClause, Sub, SubClause};

        if self.setup_runner.is_some() || matches!(self.modal_stack.last(), Some(Id::Setup)) {
            return;
        }

        let actions = self.build_settings_actions();
        if actions.is_empty() {
            // No persisted setup → fall back to the full wizard.
            self.reopen_setup();
            return;
        }
        let labels: Vec<String> = actions.iter().map(|a| a.label()).collect();
        self.settings_actions = actions;
        let modal = Choice::single("What do you want to configure?", labels)
            .title("Settings")
            .label(|s: &String| s.clone());
        let _ = self.app.mount(
            Id::Setup,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::Setup);
        let _ = self.app.active(&Id::Setup);
        self.redraw = true;
    }

    /// Build the visible actions from the user's cached persisted
    /// setup. Per-provider actions only appear if the provider is
    /// enabled. Always includes the "full setup" escape hatch.
    fn build_settings_actions(&self) -> Vec<SettingsAction> {
        let Some(p) = &self.persisted_setup else {
            return Vec::new();
        };
        let mut actions = Vec::new();
        for provider_id in &p.enabled_providers {
            let label = match provider_id.as_str() {
                "github" => "GitHub".to_string(),
                "linear" => "Linear".to_string(),
                other => other.to_string(),
            };
            actions.push(SettingsAction::EditScopes {
                provider_id: provider_id.clone(),
                label: label.clone(),
            });
            actions.push(SettingsAction::EditFilters {
                provider_id: provider_id.clone(),
                label,
            });
        }
        actions.push(SettingsAction::EditProviders);
        actions.push(SettingsAction::EditAgents);
        actions.push(SettingsAction::FullSetup);
        actions
    }

    /// Dispatch a Settings palette pick. Builds a partial-entry
    /// SetupRunner pre-seeded with current persisted state, then
    /// mounts the first step. The on_setup_complete hook (installed
    /// by main.rs) handles persistence on Finish.
    pub fn dispatch_settings_action(&mut self, action: SettingsAction) {
        use crate::setup_flow::{PartialEntry, SetupRunner};
        let Some((report, sources)) = self.setup_inputs.clone() else {
            tracing::warn!("dispatch_settings_action: no cached inputs");
            return;
        };
        let entry = match action {
            SettingsAction::EditProviders => PartialEntry::EditProviders,
            SettingsAction::EditAgents => PartialEntry::EditAgents,
            SettingsAction::EditFilters { provider_id, .. } => {
                PartialEntry::EditFilter(provider_id)
            }
            SettingsAction::EditScopes { provider_id, .. } => {
                PartialEntry::EditScopes(provider_id)
            }
            SettingsAction::FullSetup => {
                self.start_setup_wizard(report, sources);
                return;
            }
        };
        // Pre-seed the accumulator from persisted state so partial
        // flows don't drop the user's other-provider config.
        let outcome = match self.persisted_setup.clone() {
            Some(p) => crate::setup_flow::persisted_to_outcome(p, report),
            None => crate::setup_flow::SetupOutcome::default_enabled(report),
        };
        let (runner, step) = SetupRunner::at_partial(outcome, sources, entry);
        self.setup_runner = Some(runner);
        let owned_runner = self.setup_runner.take().expect("just set");
        self.handle_runner_step(owned_runner, step);
    }

    /// Re-open the full setup wizard mid-session. Uses the cached
    /// `(report, sources)` populated at startup. No-op when the
    /// cache is empty (`--test`, `--connect`).
    pub fn reopen_setup(&mut self) {
        if self.setup_runner.is_some() {
            return;
        }
        let Some((report, sources)) = self.setup_inputs.clone() else {
            tracing::warn!("reopen_setup: no cached setup inputs");
            return;
        };
        self.start_setup_wizard(report, sources);
    }

    /// Mount the first-poll progress modal. Called from the
    /// on-setup-complete hook (and from the returning-user kickoff
    /// path) once polling has been kicked off on the daemon side.
    pub fn show_polling(&mut self, sources: Vec<String>) {
        self.polling = Some(crate::realm::components::polling::Polling::new(sources));
        self.polling_last_tick = std::time::Instant::now();
        self.redraw = true;
    }

    /// Restore terminal state (idempotent).
    pub fn shutdown(&mut self) {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::DisableMouseCapture,
        );
        let _ = self.terminal.leave_alternate_screen();
        let _ = self.terminal.disable_raw_mode();
    }
    /// Render the current frame.
    pub fn view(&mut self) {
        // Pull state out before the closure so the borrow checker is
        // happy — `terminal.draw` takes `&mut self.terminal` while we
        // also need `&mut self.app` etc. inside.
        let sidebar_pct = self.sidebar_pct;
        let right_top_pct = self.right_top_pct;
        let polling_status: Option<(&'static str, String)> = self
            .polling
            .as_ref()
            .map(|p| (p.spinner_glyph(), p.status_label()));
        // Resolve the focused pane's keymap for the footer's left
        // hint zone before entering the closure (avoids borrow
        // conflicts with `terminal.draw`).
        let keymap: &'static [crate::pane::Binding] = match self.focus {
            PaneFocus::Sidebar => self.sidebar.keymap(),
            PaneFocus::Right => self.right.keymap(),
            PaneFocus::Terminals => self.terminals.keymap(),
        };
        let notice = self.notice.clone();
        let mut captured_area = Rect::default();
        let _ = self.terminal.draw(|f| {
            let area = f.area();
            captured_area = area;
            let (pane_area, footer_area) = split_for_footer(area);
            let (left, right_top, right_bottom) =
                pane_areas(pane_area, sidebar_pct, right_top_pct);
            self.sidebar.view_in(left, f);
            self.right.view_in(right_top, f);
            self.terminals.view_in(right_bottom, f);

            // Footer: keymap + polling status + notice.
            crate::realm::components::footer::render(
                f,
                footer_area,
                keymap,
                polling_status.as_ref().map(|(s, l)| (*s, l.as_str())),
                notice.as_ref(),
            );

            // Modal stack last (highest z-order).
            if let Some(top) = self.modal_stack.last() {
                self.app.view(top, f, area);
            }
        });
        self.last_area = captured_area;
        // Resize commands are queued by the terminal stack's render
        // path each time a slot's rect changes. Drain + ship them so
        // libghostty's PTY learns the new size — without this,
        // typing into a freshly-shown terminal produces output that
        // falls off the bottom of the live grid.
        for cmd in self.terminals.drain_cmds() {
            let _ = self.client.send(cmd);
        }
    }

    /// Apply one `Msg`.
    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::SplashConfirmed => {
                // Splash is only mounted during the setup wizard now,
                // so this always advances into Providers. The
                // returning-user "subscribe + focus" path runs from
                // `Model::new` directly.
                if let Some(mut runner) = self.setup_runner.take() {
                    let step = runner.step_splash_confirmed();
                    self.handle_runner_step(runner, step);
                } else {
                    // Defensive: if Splash somehow ended up mounted
                    // without a runner, just pop it.
                    self.pop_modal();
                }
            }
            Msg::AppClose => {
                self.quit = true;
            }
            Msg::SidebarCmds => {
                for cmd in self.sidebar.drain_cmds() {
                    let _ = self.client.send(cmd);
                }
            }
            Msg::RightCmds => {
                for cmd in self.right.drain_cmds() {
                    let _ = self.client.send(cmd);
                }
            }
            Msg::TerminalCmds => {
                for cmd in self.terminals.drain_cmds() {
                    let _ = self.client.send(cmd);
                }
            }
            Msg::ChoicePicked(picks) => {
                // Editor picker (Id::Editor) — pick → launch.
                if matches!(self.modal_stack.last(), Some(Id::Editor)) {
                    let editor = picks
                        .first()
                        .and_then(|i| self.editor_choices.get(*i).cloned());
                    let worktree = self
                        .sidebar
                        .selected_workspace()
                        .and_then(|w| w.sessions.first().map(|s| s.worktree_path.clone()));
                    self.editor_choices.clear();
                    self.pop_modal();
                    if let (Some(editor), Some(worktree)) = (editor, worktree) {
                        self.launch_editor(&editor, &worktree);
                    }
                }
                // Settings palette is a non-runner Choice modal — if
                // the user just picked an action, route into a
                // partial wizard flow before falling through.
                else if !self.settings_actions.is_empty()
                    && matches!(self.modal_stack.last(), Some(Id::Setup))
                    && self.setup_runner.is_none()
                {
                    let action = picks
                        .first()
                        .and_then(|i| self.settings_actions.get(*i).cloned());
                    self.settings_actions.clear();
                    self.pop_modal();
                    if let Some(action) = action {
                        self.dispatch_settings_action(action);
                    }
                } else if let Some(mut runner) = self.setup_runner.take() {
                    let step = runner.step_choice_picked(picks);
                    self.handle_runner_step(runner, step);
                } else {
                    self.pop_modal();
                }
            }
            Msg::ChoiceRefresh => {
                if let Some(mut runner) = self.setup_runner.take() {
                    let step = runner.step_choice_refresh();
                    self.handle_runner_step(runner, step);
                }
            }
            Msg::ChoiceBack => {
                if let Some(mut runner) = self.setup_runner.take() {
                    let step = runner.step_choice_back();
                    self.handle_runner_step(runner, step);
                } else {
                    self.pop_modal();
                }
            }
            Msg::LoadingResolved(carrier) => {
                if let Some(mut runner) = self.setup_runner.take() {
                    let payload = carrier.take().unwrap_or_else(|| Box::new(()));
                    let step = runner.step_loading_resolved(payload);
                    self.handle_runner_step(runner, step);
                } else {
                    self.pop_modal();
                }
            }
            Msg::ModalDismissed => {
                if let Some(mut runner) = self.setup_runner.take() {
                    let step = runner.step_dismissed();
                    self.handle_runner_step(runner, step);
                } else {
                    self.pop_modal();
                }
            }
            Msg::TextareaSubmitted(body) => {
                // Reply submit: build a PostReply for the workspace
                // we mounted the textarea against and send it to the
                // daemon. Empty bodies dismiss without posting.
                self.pop_modal();
                let target = self.pending_reply.take();
                if let Some(session_key) = target
                    && !body.trim().is_empty()
                {
                    let _ = self.client.send(IpcCommand::PostReply {
                        session_key,
                        body,
                    });
                }
            }
            Msg::InputSubmitted(text) => {
                // Dispatch by which Input modal is currently on top.
                // Today: NewWorkspace → CreateWorkspace. Future input
                // prompts (rename, snooze duration, …) get their own
                // arm here.
                let top = self.modal_stack.last().cloned();
                self.pop_modal();
                match top {
                    Some(Id::NewWorkspace) => {
                        let name = text.trim().to_string();
                        if !name.is_empty() {
                            tracing::info!(
                                workspace_name = %name,
                                "creating new pre-PR workspace"
                            );
                            let _ = self
                                .client
                                .send(IpcCommand::CreateWorkspace { name });
                        }
                    }
                    _ => {
                        // Unknown input source — silently drop. The
                        // pop above already cleared the modal.
                    }
                }
            }
            // Polling outcomes — surface as footer notices, never
            // as full-screen modals. Permanent + auth errors are
            // sticky; retryable ones (which shouldn't reach here)
            // auto-fade in render.
            Msg::PollingError((source, kind, detail, message)) => {
                tracing::warn!(
                    "polling error from {source} ({kind}): {message} — {detail}"
                );
                use crate::realm::components::footer::{Notice, NoticeSeverity};
                let severity = match kind.as_str() {
                    "auth" => NoticeSeverity::Auth,
                    "retryable" => NoticeSeverity::Retryable,
                    _ => NoticeSeverity::Permanent,
                };
                self.notice = Some(Notice::new(
                    format!("{source}: {message}"),
                    severity,
                ));
                self.redraw = true;
            }
            Msg::PollingTimeout => {
                tracing::info!("polling first-cycle timeout — modal dismissed");
            }
            Msg::PollingEmptyInbox(queries) => {
                tracing::info!(
                    "polling completed with empty inbox; queries seen: {queries:?}"
                );
            }
            _ => {
                self.pop_modal();
            }
        }
    }

    /// Apply a [`crate::setup_flow::RunnerStep`] returned by the
    /// runner — mount the next modal, fire the on-complete hook, or
    /// drop the wizard. The `runner` argument lets us conditionally
    /// hold on to the runner across step transitions: `Next` puts it
    /// back; `Finish` / `Cancel` drop it.
    fn handle_runner_step(
        &mut self,
        runner: crate::setup_flow::SetupRunner,
        step: crate::setup_flow::RunnerStep,
    ) {
        use crate::setup_flow::RunnerStep;
        match step {
            RunnerStep::Next(component) => {
                self.setup_runner = Some(runner);
                self.mount_setup_modal(component);
            }
            RunnerStep::Finish(outcome) => {
                let sources: Vec<String> =
                    outcome.enabled_providers.iter().cloned().collect();
                if let Some(hook) = self.on_setup_complete.take() {
                    hook(outcome);
                }
                self.unmount_setup_modal();
                let _ = self.client.send(IpcCommand::Subscribe);
                self.set_focus_attr();
                if !sources.is_empty() {
                    self.show_polling(sources);
                }
            }
            RunnerStep::Cancel => {
                self.unmount_setup_modal();
                let _ = self.client.send(IpcCommand::Subscribe);
                self.set_focus_attr();
            }
            RunnerStep::Stay => {
                self.setup_runner = Some(runner);
            }
        }
    }

    /// Unmount whatever's at `Id::Setup` (or `Id::Splash` if the
    /// wizard hasn't moved off splash yet) and mount `component`
    /// there. The setup id is reused — only one wizard step is ever
    /// live at a time.
    fn mount_setup_modal(
        &mut self,
        component: Box<dyn tuirealm::component::AppComponent<Msg, UserEvent>>,
    ) {
        use tuirealm::subscription::{EventClause, Sub, SubClause};
        // Unmount whatever's on top.
        if let Some(top) = self.modal_stack.last().cloned() {
            let _ = self.app.umount(&top);
            self.modal_stack.pop();
        }
        let _ = self.app.mount(
            Id::Setup,
            component,
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::Setup);
        let _ = self.app.active(&Id::Setup);
        self.redraw = true;
    }

    /// Drop whatever setup-related modal is on top of the stack.
    /// Called on Finish / Cancel.
    fn unmount_setup_modal(&mut self) {
        if let Some(top) = self.modal_stack.last().cloned() {
            let _ = self.app.umount(&top);
            self.modal_stack.pop();
        }
        if let Some(top) = self.modal_stack.last() {
            let _ = self.app.active(top);
        }
        self.redraw = true;
    }

    /// Top-level key handler when no modal is active. Routes Tab,
    /// global escapes, and forwards everything else to the focused
    /// pane wrapper.
    fn handle_pane_key(&mut self, key: RealmKey) {
        match key.code {
            // Tab cycles panes — but ONLY when the active pane has
            // no PTY swallowing keys. Inside a terminal with a live
            // PTY, Tab belongs to the shell / agent; the `]]`
            // escape sequence is the only way out (tmux-style
            // prefix model). With no terminals running, Tab cycles
            // normally — there's no inner program to forward it to.
            Key::Tab
                if !key.modifiers.contains(KeyModifiers::SHIFT)
                    && (self.focus != PaneFocus::Terminals
                        || self.terminals.is_empty()) =>
            {
                self.q_armed_at = None;
                self.focus = self.focus.next();
                self.set_focus_attr();
                self.redraw = true;
                return;
            }
            Key::Char('q')
                if key.modifiers.is_empty() && self.focus != PaneFocus::Terminals =>
            {
                // q-q double-tap: first q outside a terminal arms the
                // latch; second q within Q_DOUBLE_TAP_WINDOW quits.
                let now = std::time::Instant::now();
                if let Some(armed_at) = self.q_armed_at
                    && now.duration_since(armed_at) <= Q_DOUBLE_TAP_WINDOW
                {
                    self.quit = true;
                    return;
                }
                self.q_armed_at = Some(now);
                self.redraw = true;
                return;
            }
            Key::Char('?')
                if key.modifiers.is_empty() && self.focus != PaneFocus::Terminals =>
            {
                self.q_armed_at = None;
                self.mount_help();
                return;
            }
            // Shift-arrows: resize splitters. Disabled inside a
            // terminal so the shell can still bind them.
            Key::Left | Key::Right | Key::Up | Key::Down
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    && self.focus != PaneFocus::Terminals =>
            {
                self.q_armed_at = None;
                let (dx, dy) = match key.code {
                    Key::Left => (-SPLIT_STEP, 0),
                    Key::Right => (SPLIT_STEP, 0),
                    Key::Up => (0, -SPLIT_STEP),
                    Key::Down => (0, SPLIT_STEP),
                    _ => (0, 0),
                };
                self.nudge_splits(dx, dy);
                return;
            }
            // Ctrl-Shift-D: detach the focused pane into a new pilot
            // process. Many terminals report Ctrl-Shift-letter as the
            // capital letter with CONTROL set; some include SHIFT too.
            // Match either form.
            Key::Char('D')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.focus != PaneFocus::Terminals =>
            {
                self.q_armed_at = None;
                if let Some(spec) = self.focused_detach_spec() {
                    spawn_detached_pilot(&spec);
                }
                return;
            }
            // `r` on a sidebar row: open the reply textarea targeted
            // at the selected workspace. Submit posts via the
            // provider's reply hook (currently github only).
            Key::Char('r')
                if key.modifiers.is_empty() && self.focus == PaneFocus::Sidebar =>
            {
                self.q_armed_at = None;
                if let Some(workspace_key) = self.sidebar.selected_workspace_key().cloned() {
                    self.mount_reply(workspace_key);
                }
                return;
            }
            // `E` from the sidebar: open the focused workspace's
            // worktree in an editor (Zed / VS Code / Cursor / …).
            // Detection happens at startup; users add custom editors
            // in `~/.pilot/config.yaml::editors`.
            Key::Char('E')
                if (key.modifiers.is_empty()
                    || key.modifiers == KeyModifiers::SHIFT)
                    && self.focus == PaneFocus::Sidebar =>
            {
                self.q_armed_at = None;
                self.open_editor();
                return;
            }
            // `n` from the sidebar: prompt for a workspace name and
            // create a brand-new pre-PR workspace. Lets the user
            // start work in a fresh worktree before opening a PR
            // (e.g. exploration / spike / experiments).
            Key::Char('n')
                if key.modifiers.is_empty() && self.focus == PaneFocus::Sidebar =>
            {
                self.q_armed_at = None;
                self.mount_new_workspace_input();
                return;
            }
            // `,` opens the Settings palette — small picker with
            // "Add a repo (github)" / "Edit agents" / etc. Familiar
            // mnemonic from VS Code / Sublime ("Cmd-," for
            // settings). Disabled inside a terminal so the shell
            // can still bind it.
            Key::Char(',')
                if key.modifiers.is_empty() && self.focus != PaneFocus::Terminals =>
            {
                self.q_armed_at = None;
                self.open_settings();
                return;
            }
            // `e`: clear the most recent footer notice. Lets users
            // dismiss sticky auth/permanent errors after they've
            // read them. Disabled inside terminals (the shell may
            // bind `e`).
            Key::Char('e')
                if key.modifiers.is_empty() && self.focus != PaneFocus::Terminals =>
            {
                self.q_armed_at = None;
                if self.notice.take().is_some() {
                    self.redraw = true;
                    return;
                }
                // No notice to clear → fall through so panes can use
                // `e` for their own bindings.
            }
            _ => {
                // Any other key disarms.
                self.q_armed_at = None;
            }
        }

        // Terminal-pane escape sequence (`]]` by default). Two
        // consecutive presses of the escape char inside a terminal
        // return focus to the sidebar instead of forwarding to the
        // PTY. The first `]` is held back; if a non-`]` key arrives
        // before the second `]`, the held char is flushed to the PTY
        // first so the user's `]` isn't silently swallowed.
        if self.focus == PaneFocus::Terminals
            && key.modifiers.is_empty()
            && matches!(key.code, Key::Char(c) if c == TERMINAL_ESCAPE_CHAR)
        {
            if self.escape_armed_at.is_some() {
                // Second `]` — break out to sidebar.
                self.escape_armed_at = None;
                self.focus = PaneFocus::Sidebar;
                self.set_focus_attr();
                self.redraw = true;
                return;
            }
            self.escape_armed_at = Some(std::time::Instant::now());
            return;
        }
        if self.focus == PaneFocus::Terminals && self.escape_armed_at.take().is_some() {
            // Non-`]` key arrived after a held `]` — flush the held
            // char to the PTY before the new key, so typing patterns
            // like `]a` aren't lost.
            let mut held_cmds: Vec<IpcCommand> = Vec::new();
            let held = crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(TERMINAL_ESCAPE_CHAR),
                crossterm::event::KeyModifiers::NONE,
            );
            self.terminals.handle_key_direct(held, &mut held_cmds);
            for cmd in held_cmds {
                let _ = self.client.send(cmd);
            }
        }

        // We have a typed key already; skip the synthetic Event
        // round-trip and call the pane wrappers' direct entry points.
        let ct = realm_key_to_crossterm(&key);
        let mut cmds: Vec<IpcCommand> = Vec::new();
        match self.focus {
            PaneFocus::Sidebar => self.sidebar.handle_key_direct(ct, &mut cmds),
            PaneFocus::Right => self.right.handle_key_direct(ct, &mut cmds),
            // Terminals pane with NO active terminal can't route to a
            // PTY. The empty-state hint says "press s for shell, c
            // for claude" — those bindings live on Sidebar, so we
            // forward there instead. PTY-routing resumes once the
            // first TerminalSpawned arrives.
            PaneFocus::Terminals if self.terminals.is_empty() => {
                self.sidebar.handle_key_direct(ct, &mut cmds);
            }
            PaneFocus::Terminals => self.terminals.handle_key_direct(ct, &mut cmds),
        }
        // Surface spawn intent in the footer so the user sees that
        // worktree creation / process startup is happening (can take
        // 1-3s on first session). The notice clears when the matching
        // `TerminalSpawned` arrives in `handle_daemon_event`.
        for cmd in &cmds {
            if let IpcCommand::Spawn { kind, .. } = cmd {
                use crate::realm::components::footer::{Notice, NoticeSeverity};
                let label = match kind {
                    pilot_ipc::TerminalKind::Shell => "shell".to_string(),
                    pilot_ipc::TerminalKind::Agent(a) => a.to_string(),
                    other => format!("{other:?}").to_lowercase(),
                };
                self.notice = Some(Notice::new(
                    format!("Spawning {label}…"),
                    NoticeSeverity::Info,
                ));
            }
        }
        for cmd in cmds {
            let _ = self.client.send(cmd);
        }
        // Sidebar j/k changes selection — propagate to right + terminals.
        self.sync_panes();
        self.redraw = true;
    }

    /// Returns true when the q-q latch is armed (used by the bottom
    /// hint bar to show "press q again" briefly).
    pub fn q_arm_pending(&self) -> bool {
        self.q_armed_at
            .is_some_and(|t| t.elapsed() <= Q_DOUBLE_TAP_WINDOW)
    }

    /// Read-only accessor — which pane currently has focus. Used by
    /// tests + (in future) the bottom hint bar.
    pub fn focus(&self) -> PaneFocus {
        self.focus
    }

    /// Sidebar / right / activity split percentages — exposed so tests
    /// can verify Shift-arrow + drag updates apply correctly.
    pub fn split_pcts(&self) -> (u16, u16) {
        (self.sidebar_pct, self.right_top_pct)
    }

    /// Top of the modal stack (or None if no modal is mounted). Used
    /// by tests to verify that `?` mounts the help modal, etc.
    pub fn top_modal(&self) -> Option<&Id> {
        self.modal_stack.last()
    }

    /// Test entry point: drive a key through `handle_pane_key`. Lets
    /// integration tests bypass the run-loop's crossterm polling.
    pub fn dispatch_key(&mut self, key: RealmKey) {
        self.handle_pane_key(key);
    }

    /// Test entry point: drive a mouse event through `handle_mouse`
    /// after manually setting `last_area` (since `view()` would
    /// otherwise be needed to populate it).
    pub fn dispatch_mouse_in(
        &mut self,
        m: crossterm::event::MouseEvent,
        area: Rect,
    ) {
        self.last_area = area;
        self.handle_mouse(m);
    }

    /// Test accessor — read-only handle to the sidebar wrapper.
    pub fn sidebar(&self) -> &crate::realm::components::sidebar::Sidebar {
        &self.sidebar
    }

    /// DetachSpec for the focused pane, or None if it can't detach
    /// (e.g. cursor on a repo header in the sidebar).
    fn focused_detach_spec(&self) -> Option<crate::pane::DetachSpec> {
        match self.focus {
            PaneFocus::Sidebar => self.sidebar.detachable(),
            PaneFocus::Right => self.right.detachable(),
            PaneFocus::Terminals => self.terminals.detachable(),
        }
    }

    /// Mouse routing:
    /// - Down on a splitter line → start drag (resize panes on
    ///   subsequent Drag events until Up).
    /// - Down anywhere else → focus the pane the click landed in.
    /// - Up → end the active drag.
    /// - ScrollUp/Down over the terminal pane → forward to the
    ///   terminal's scrollback (libghostty handles the actual move).
    pub fn handle_mouse(&mut self, m: crossterm::event::MouseEvent) {
        use crossterm::event::MouseEventKind;

        if self.last_area.width == 0 || self.last_area.height == 0 {
            return;
        }
        let (sidebar_rect, right_top_rect, right_bottom_rect) = pane_areas(
            self.last_area,
            self.sidebar_pct,
            self.right_top_pct,
        );

        match m.kind {
            MouseEventKind::Down(button) => {
                self.q_armed_at = None;
                // Click inside the terminal pane while the inner
                // program tracks mouse → forward the click as an
                // escape sequence so Claude Code et al. respond to
                // their own UI. Splitter drag still wins on the
                // splitter line.
                if rect_contains(right_bottom_rect, m.column, m.row)
                    && self.focus == PaneFocus::Terminals
                    && self.terminals.focused_terminal_tracks_mouse()
                    && self.hit_test_splitter(
                        m.column,
                        m.row,
                        sidebar_rect,
                        right_top_rect,
                    )
                    .is_none()
                {
                    let cell_col = m.column.saturating_sub(right_bottom_rect.x) as u32;
                    let cell_row = m.row.saturating_sub(right_bottom_rect.y) as u32;
                    let vt_button = match button {
                        crossterm::event::MouseButton::Left => {
                            libghostty_vt::mouse::Button::Left
                        }
                        crossterm::event::MouseButton::Middle => {
                            libghostty_vt::mouse::Button::Middle
                        }
                        crossterm::event::MouseButton::Right => {
                            libghostty_vt::mouse::Button::Right
                        }
                    };
                    if let Some((terminal_id, bytes)) = self.terminals.encode_mouse(
                        libghostty_vt::mouse::Action::Press,
                        Some(vt_button),
                        cell_col,
                        cell_row,
                    ) {
                        let _ = self.client.send(IpcCommand::Write {
                            terminal_id,
                            bytes,
                        });
                        self.redraw = true;
                        return;
                    }
                }
                if let Some(target) = self.hit_test_splitter(
                    m.column,
                    m.row,
                    sidebar_rect,
                    right_top_rect,
                ) {
                    self.active_drag = Some(target);
                    return;
                }
                let target = if rect_contains(sidebar_rect, m.column, m.row) {
                    Some(PaneFocus::Sidebar)
                } else if rect_contains(right_top_rect, m.column, m.row) {
                    Some(PaneFocus::Right)
                } else if rect_contains(right_bottom_rect, m.column, m.row) {
                    Some(PaneFocus::Terminals)
                } else {
                    None
                };
                if let Some(focus) = target {
                    if self.focus != focus {
                        self.focus = focus;
                        self.set_focus_attr();
                        self.redraw = true;
                    }
                    // Clicking inside the sidebar should also move the
                    // cursor to whatever row was clicked (workspace
                    // selection). Right + Terminals don't have row-
                    // level selection today.
                    if focus == PaneFocus::Sidebar
                        && self.sidebar.click_to_select(sidebar_rect, m.row)
                    {
                        self.sync_panes();
                        self.redraw = true;
                    }
                }
            }
            MouseEventKind::Drag(_) => {
                if let Some(target) = self.active_drag {
                    self.update_drag(target, m.column, m.row);
                }
            }
            MouseEventKind::Up(button) => {
                let was_drag = self.active_drag.take().is_some();
                if !was_drag
                    && rect_contains(right_bottom_rect, m.column, m.row)
                    && self.focus == PaneFocus::Terminals
                    && self.terminals.focused_terminal_tracks_mouse()
                {
                    let cell_col = m.column.saturating_sub(right_bottom_rect.x) as u32;
                    let cell_row = m.row.saturating_sub(right_bottom_rect.y) as u32;
                    let vt_button = match button {
                        crossterm::event::MouseButton::Left => {
                            libghostty_vt::mouse::Button::Left
                        }
                        crossterm::event::MouseButton::Middle => {
                            libghostty_vt::mouse::Button::Middle
                        }
                        crossterm::event::MouseButton::Right => {
                            libghostty_vt::mouse::Button::Right
                        }
                    };
                    if let Some((terminal_id, bytes)) = self.terminals.encode_mouse(
                        libghostty_vt::mouse::Action::Release,
                        Some(vt_button),
                        cell_col,
                        cell_row,
                    ) {
                        let _ = self.client.send(IpcCommand::Write {
                            terminal_id,
                            bytes,
                        });
                        self.redraw = true;
                    }
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                // Trackpad / wheel scroll over the terminal pane.
                // Two paths:
                //   1. Inner program enabled mouse tracking (Claude
                //      Code, vim, less, etc.) → encode the wheel
                //      event as the protocol's escape sequence and
                //      ship to the PTY. The inner app handles its
                //      own scrolling.
                //   2. Plain shell at a prompt → scroll libghostty's
                //      scrollback in-pane.
                if !rect_contains(right_bottom_rect, m.column, m.row) {
                    return;
                }
                if self.terminals.focused_terminal_tracks_mouse() {
                    // Wheel buttons in xterm/SGR protocol: 4 = up, 5 = down.
                    let button = if matches!(m.kind, MouseEventKind::ScrollUp) {
                        libghostty_vt::mouse::Button::Four
                    } else {
                        libghostty_vt::mouse::Button::Five
                    };
                    let cell_col = m.column.saturating_sub(right_bottom_rect.x) as u32;
                    let cell_row = m.row.saturating_sub(right_bottom_rect.y) as u32;
                    if let Some((terminal_id, bytes)) = self.terminals.encode_mouse(
                        libghostty_vt::mouse::Action::Press,
                        Some(button),
                        cell_col,
                        cell_row,
                    ) {
                        let _ = self.client.send(IpcCommand::Write {
                            terminal_id,
                            bytes,
                        });
                    }
                } else {
                    const STEP: isize = 3;
                    let delta = if matches!(m.kind, MouseEventKind::ScrollUp) {
                        -STEP
                    } else {
                        STEP
                    };
                    self.terminals.scroll_active(delta);
                }
                self.redraw = true;
            }
            _ => {}
        }
    }

    /// Test whether `(col, row)` lands within tolerance of one of the
    /// two splitter lines. Tolerance: ±1 cell so users don't have to
    /// land pixel-perfect on the divider.
    fn hit_test_splitter(
        &self,
        col: u16,
        row: u16,
        sidebar_rect: Rect,
        right_top_rect: Rect,
    ) -> Option<DragTarget> {
        // Vertical splitter sits between sidebar and the right column.
        let v_x = sidebar_rect.x + sidebar_rect.width;
        if col + 1 >= v_x
            && col <= v_x + 1
            && row >= self.last_area.y
            && row < self.last_area.y + self.last_area.height
        {
            return Some(DragTarget::SidebarRight);
        }
        // Horizontal splitter sits between right-top and right-bottom.
        let h_y = right_top_rect.y + right_top_rect.height;
        if row + 1 >= h_y
            && row <= h_y + 1
            && col >= right_top_rect.x
            && col < right_top_rect.x + right_top_rect.width
        {
            return Some(DragTarget::ActivityTerminals);
        }
        None
    }

    /// Translate a drag's `(col, row)` into a new percentage for the
    /// active splitter and apply it.
    fn update_drag(&mut self, target: DragTarget, col: u16, row: u16) {
        match target {
            DragTarget::SidebarRight => {
                if self.last_area.width == 0 {
                    return;
                }
                let rel = col.saturating_sub(self.last_area.x) as i32;
                let pct = (rel * 100 / self.last_area.width as i32).clamp(
                    SPLIT_MIN as i32,
                    SPLIT_MAX as i32,
                ) as u16;
                if pct != self.sidebar_pct {
                    self.sidebar_pct = pct;
                    self.redraw = true;
                }
            }
            DragTarget::ActivityTerminals => {
                let (_, right_top_rect, right_bottom_rect) = pane_areas(
                    self.last_area,
                    self.sidebar_pct,
                    self.right_top_pct,
                );
                let right_height =
                    right_top_rect.height + right_bottom_rect.height;
                if right_height == 0 {
                    return;
                }
                let rel = row.saturating_sub(right_top_rect.y) as i32;
                let pct = (rel * 100 / right_height as i32).clamp(
                    SPLIT_MIN as i32,
                    SPLIT_MAX as i32,
                ) as u16;
                if pct != self.right_top_pct {
                    self.right_top_pct = pct;
                    self.redraw = true;
                }
            }
        }
    }

    /// Adjust the split percentages. `dx > 0` widens the sidebar;
    /// `dy > 0` grows the activity row at the terminal stack's
    /// expense. Clamps both axes to `[SPLIT_MIN, SPLIT_MAX]`.
    fn nudge_splits(&mut self, dx: i16, dy: i16) {
        let new_sidebar = clamp_pct(self.sidebar_pct as i16 + dx);
        let new_top = clamp_pct(self.right_top_pct as i16 + dy);
        if new_sidebar != self.sidebar_pct || new_top != self.right_top_pct {
            self.sidebar_pct = new_sidebar;
            self.right_top_pct = new_top;
            self.redraw = true;
        }
    }

    /// Mount the reply textarea targeted at `workspace_key`. Submit
    /// → `Msg::TextareaSubmitted(body)` → orchestrator builds a
    /// `Command::PostReply { session_key, body }`.
    fn mount_reply(&mut self, workspace_key: pilot_core::SessionKey) {
        use crate::realm::components::textarea::Textarea;
        use tuirealm::subscription::{EventClause, Sub, SubClause};

        if matches!(self.modal_stack.last(), Some(Id::Reply)) {
            return;
        }

        let label = workspace_key.to_string();
        let modal = Textarea::new("Reply").with_header(format!("on {label}"));
        let _ = self.app.mount(
            Id::Reply,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::Reply);
        let _ = self.app.active(&Id::Reply);
        self.pending_reply = Some(workspace_key);
        self.redraw = true;
    }

    /// Mount the "New workspace" name prompt. Submit →
    /// `Msg::InputSubmitted(name)` while `Id::NewWorkspace` is on
    /// top → `Command::CreateWorkspace { name }`. The daemon
    /// allocates a slug-based key and persists the empty workspace.
    fn mount_new_workspace_input(&mut self) {
        use crate::realm::components::input::Input;
        use tuirealm::subscription::{EventClause, Sub, SubClause};

        if matches!(self.modal_stack.last(), Some(Id::NewWorkspace)) {
            return;
        }

        let modal = Input::new("Name this workspace")
            .title("New workspace")
            .placeholder("e.g. spike-rate-limit, refactor-auth, …")
            .with_validator(|s: &str| !s.trim().is_empty());
        let _ = self.app.mount(
            Id::NewWorkspace,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::NewWorkspace);
        let _ = self.app.active(&Id::NewWorkspace);
        self.redraw = true;
    }

    /// Build + mount a Help modal listing the focused pane's keymap
    /// plus the global section. Idempotent: re-pressing `?` while
    /// help is up is a no-op (the existing modal stays).
    fn mount_help(&mut self) {
        use crate::pane::Binding;
        use crate::realm::components::help::{Help, HelpSection};
        use tuirealm::subscription::{EventClause, Sub, SubClause};

        if self.modal_stack.last() == Some(&Id::Help) {
            return;
        }

        const GLOBAL: &[Binding] = &[
            Binding { keys: "Tab", label: "cycle panes" },
            Binding { keys: "Shift-Arrows", label: "resize splitters" },
            Binding { keys: "Ctrl-Shift-D", label: "detach pane" },
            Binding { keys: "?", label: "this help" },
            Binding { keys: "q q", label: "quit" },
        ];

        let sections = vec![
            HelpSection { title: "Global", bindings: GLOBAL },
            HelpSection { title: "Sidebar", bindings: self.sidebar.keymap() },
            HelpSection { title: "Activity", bindings: self.right.keymap() },
            HelpSection { title: "Terminals", bindings: self.terminals.keymap() },
        ];

        let _ = self.app.mount(
            Id::Help,
            Box::new(Help::new(sections)),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::Help);
        let _ = self.app.active(&Id::Help);
        self.redraw = true;
    }

    /// Push a modal.
    pub fn push_modal(&mut self, id: Id) {
        self.modal_stack.push(id.clone());
        let _ = self.app.active(&id);
        self.redraw = true;
    }

    fn pop_modal(&mut self) {
        if let Some(top) = self.modal_stack.pop() {
            // Always unmount — every modal id is now transient
            // (mounted on demand by start_setup_wizard / mount_help /
            // mount_reply / etc.).
            let _ = self.app.umount(&top);
        }
        if let Some(top) = self.modal_stack.last() {
            let _ = self.app.active(top);
        }
        self.redraw = true;
    }

    fn set_focus_attr(&mut self) {
        self.sidebar.set_focused(self.focus == PaneFocus::Sidebar);
        self.right.set_focused(self.focus == PaneFocus::Right);
        self.terminals
            .set_focused(self.focus == PaneFocus::Terminals);
    }

    /// Forward an inbound daemon event into all three panes. Each
    /// pane decides whether the event is relevant. After the very
    /// first Snapshot, apply any pending CLI preselect. Also feeds
    /// the polling modal so it can detect "first task arrived".
    pub fn handle_daemon_event(&mut self, event: IpcEvent) {
        let is_snapshot = matches!(&event, IpcEvent::Snapshot { .. });
        let is_spawn =
            matches!(&event, IpcEvent::TerminalSpawned { .. } | IpcEvent::TerminalFocusRequested { .. });
        self.sidebar.on_daemon_event(&event);
        self.right.on_daemon_event(&event);
        self.terminals.on_daemon_event(&event);
        if let Some(p) = self.polling.as_mut() {
            p.feed_daemon_event(&event);
        }
        if is_snapshot && self.preselect.is_some() {
            self.apply_preselect();
        }
        if is_spawn {
            // A terminal just appeared — auto-focus the Terminals
            // pane so the user can start typing immediately, and
            // clear any "Spawning…" footer notice that was set when
            // the matching Spawn command was sent.
            self.focus = PaneFocus::Terminals;
            self.set_focus_attr();
            if let Some(n) = &self.notice
                && n.message.starts_with("Spawning")
            {
                self.notice = None;
            }
        }
        self.sync_panes();
        self.redraw = true;
    }

    /// Auto-fade transient notices. Called once per iteration in
    /// the run loop. Severity decides the timeout:
    /// - Retryable: 5s. Hiccups self-heal, no need to linger.
    /// - Info: 15s. Spawn-progress and similar — long enough that a
    ///   slow worktree creation doesn't fade mid-flight; short
    ///   enough that a stuck notice (e.g. spawn never landed)
    ///   doesn't follow the user around forever.
    /// - Permanent / Auth: stay until dismissed (`e`).
    pub fn tick_notice(&mut self) {
        use crate::realm::components::footer::NoticeSeverity;
        let Some(n) = &self.notice else { return };
        let timeout = match n.severity {
            NoticeSeverity::Retryable => Some(RETRYABLE_FADE),
            NoticeSeverity::Info => Some(INFO_FADE),
            NoticeSeverity::Auth | NoticeSeverity::Permanent => None,
        };
        if let Some(t) = timeout
            && n.set_at.elapsed() >= t
        {
            self.notice = None;
            self.redraw = true;
        }
    }

    /// Drive the polling spinner + termination check from the run
    /// loop. Cheap; called every iteration. Returns Some(msg) when
    /// the polling modal wants to be torn down.
    pub fn polling_tick(&mut self) -> Option<Msg> {
        const TICK_INTERVAL: Duration = Duration::from_millis(80);
        if self.polling_last_tick.elapsed() < TICK_INTERVAL {
            return None;
        }
        self.polling_last_tick = std::time::Instant::now();
        let polling = self.polling.as_mut()?;
        let msg = polling.tick_direct();
        if msg.is_some() {
            self.redraw = true;
        }
        msg
    }

    /// Tear down the polling modal. Called when its tick / feed
    /// returns Some(msg) (saw workspace, timed out, etc.).
    fn dismiss_polling(&mut self) {
        if self.polling.take().is_some() {
            self.redraw = true;
        }
    }

    /// Project sidebar selection onto the right pane + terminal stack.
    /// Cheap to call; the inner setters bail when nothing changed.
    /// Called after every key dispatch and every daemon event.
    fn sync_panes(&mut self) {
        let workspace = self.sidebar.selected_workspace().cloned();
        let session_key = self.sidebar.selected_workspace_key().cloned();
        self.right.set_workspace(workspace);
        self.terminals.set_active_session(session_key);
    }

    /// Apply the pending `--workspace [--session]` selection. One-shot
    /// — clears `self.preselect` so subsequent snapshots don't
    /// override the user's manual cursor moves.
    fn apply_preselect(&mut self) {
        let Some(p) = self.preselect.take() else {
            return;
        };
        let landed = self.sidebar.focus_workspace_key(&p.workspace_key);
        if !landed {
            tracing::info!(
                "preselect: workspace key {:?} not found in first snapshot",
                p.workspace_key
            );
            return;
        }
        if let Some(raw) = p.session_id_raw
            && let Ok(uuid) = uuid::Uuid::parse_str(&raw)
        {
            let _ = self.sidebar.focus_session_id(pilot_core::SessionId(uuid));
            // Move focus to terminals so the user can type immediately.
            self.focus = PaneFocus::Terminals;
            self.set_focus_attr();
        }
    }
}

/// Clamp a candidate percentage into the legal split range.
fn clamp_pct(raw: i16) -> u16 {
    raw.clamp(SPLIT_MIN as i16, SPLIT_MAX as i16) as u16
}

/// True if `(col, row)` lies within `rect`'s half-open bounds.
fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x + rect.width
        && row >= rect.y
        && row < rect.y + rect.height
}

/// Spawn a new `pilot` process pinned to the focused pane's
/// detachable scope. Detached: the new process gets its own session
/// so closing the parent doesn't kill it. Errors are logged, not
/// surfaced — detach is best-effort UX.
fn spawn_detached_pilot(spec: &crate::pane::DetachSpec) {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("detach: current_exe unavailable: {e}");
            return;
        }
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(&spec.args);
    // Decouple from the parent so closing this pilot doesn't take
    // the detached one with it. Implementation lives in
    // `crate::platform` — setsid() on unix, DETACHED_PROCESS on
    // Windows (TODO).
    crate::platform::detach_child_process(&mut cmd);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    if let Err(e) = cmd.spawn() {
        tracing::warn!("detach: spawn failed: {e}");
    }
}

/// Carve the bottom row off for the footer. Returns
/// (pane_area, footer_area) — `pane_area` is what the three panes
/// fill; `footer_area` is the 1-row hint/status line at the bottom.
fn split_for_footer(area: Rect) -> (Rect, Rect) {
    if area.height < 2 {
        return (area, Rect::default());
    }
    let pane = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height - 1,
    };
    let footer = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    (pane, footer)
}

/// Compute the three pane rects (sidebar / right-top / right-bottom).
/// `sidebar_pct` is the sidebar's share of the total width;
/// `right_top_pct` is the activity row's share of the right column's
/// height. Both should already be clamped to `[SPLIT_MIN, SPLIT_MAX]`.
fn pane_areas(area: Rect, sidebar_pct: u16, right_top_pct: u16) -> (Rect, Rect, Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(sidebar_pct), Constraint::Min(0)])
        .split(area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(right_top_pct), Constraint::Min(0)])
        .split(cols[1]);
    (cols[0], rows[0], rows[1])
}

#[allow(dead_code)]
fn placeholder(f: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(" pilot · realm migration scaffold ")
        .borders(Borders::ALL);
    f.render_widget(block, area);
}

/// Run the realm-based pilot loop with a pre-built IPC client.
/// `main.rs::run_embedded_realm` constructs the client + daemon pair
/// before calling this so the daemon is already serving when the UI
/// boots.
pub fn run_with_client(client: Client) -> anyhow::Result<()> {
    let mut model = Model::new(client)?;
    let result = run_loop(&mut model);
    model.shutdown();
    result
}

/// Test-only: run with an unconnected client. Useful for manual
/// smoke tests without spinning up the full daemon stack.
pub fn run() -> anyhow::Result<()> {
    let (client, _server) = pilot_ipc::channel::pair();
    run_with_client(client)
}

/// Run the loop on a pre-configured model. Used by
/// `main::run_embedded_realm` so it can install the on-setup-complete
/// hook + start the wizard before entering the loop.
pub fn run_loop_with_model<T: TerminalAdapter>(
    mut model: Model<T>,
) -> anyhow::Result<()> {
    let result = run_loop(&mut model);
    model.shutdown();
    result
}

fn run_loop<T: TerminalAdapter>(model: &mut Model<T>) -> anyhow::Result<()> {
    while !model.quit {
        // 1. Drain inbound daemon events (cheap try_recv).
        while let Ok(evt) = model.client.rx.try_recv() {
            model.handle_daemon_event(evt);
        }

        // 2. Polling-modal spinner heartbeat + retryable notice fade.
        if let Some(msg) = model.polling_tick() {
            model.dismiss_polling();
            model.update(msg);
        }
        model.tick_notice();

        // 3. Process tuirealm-side messages (timer ticks for Loading,
        // injected modal keys). Non-blocking — listener thread already
        // queued any work it had.
        if let Ok(messages) = model.app.tick(PollStrategy::Once(Duration::ZERO)) {
            if !messages.is_empty() {
                model.redraw = true;
                for msg in messages {
                    model.update(msg);
                }
            }
        }

        // 4. Render if dirty — before the blocking input read so the
        // user sees their last action immediately.
        if model.redraw {
            model.view();
            model.redraw = false;
        }

        // 5. Block briefly for input. ONE crossterm reader path: when
        // a modal is up, route to the Application's active component
        // via the ChannelPort. Otherwise drive panes directly.
        if let Ok(true) = crossterm::event::poll(Duration::from_millis(40)) {
            match crossterm::event::read() {
                Ok(crossterm::event::Event::Key(key)) => {
                    let realm_key = crossterm_to_realm(key);
                    if model.modal_stack.is_empty() {
                        model.handle_pane_key(realm_key);
                    } else {
                        let _ = model
                            .modal_event_tx
                            .send(RealmEvent::Keyboard(realm_key));
                        // Modals can mutate internal state (Choice's
                        // cursor, Input's buffer) without producing a
                        // `Msg`, so app.tick() returns an empty Vec
                        // even though something changed. Force a
                        // redraw so the next frame reflects the
                        // mutation — j/k inside Choice was the
                        // visible bug.
                        model.redraw = true;
                    }
                }
                Ok(crossterm::event::Event::Mouse(m)) => {
                    if model.modal_stack.is_empty() {
                        model.handle_mouse(m);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn crossterm_to_realm(key: crossterm::event::KeyEvent) -> RealmKey {
    use crossterm::event::{KeyCode as CKC, KeyModifiers as CKM};
    let code = match key.code {
        CKC::Char(c) => Key::Char(c),
        CKC::Enter => Key::Enter,
        CKC::Esc => Key::Esc,
        CKC::Backspace => Key::Backspace,
        CKC::Left => Key::Left,
        CKC::Right => Key::Right,
        CKC::Up => Key::Up,
        CKC::Down => Key::Down,
        CKC::Home => Key::Home,
        CKC::End => Key::End,
        CKC::PageUp => Key::PageUp,
        CKC::PageDown => Key::PageDown,
        CKC::Tab => Key::Tab,
        CKC::BackTab => Key::BackTab,
        CKC::Delete => Key::Delete,
        CKC::Insert => Key::Insert,
        CKC::F(n) => Key::Function(n),
        _ => Key::Null,
    };
    let mut mods = KeyModifiers::empty();
    if key.modifiers.contains(CKM::SHIFT) {
        mods |= KeyModifiers::SHIFT;
    }
    if key.modifiers.contains(CKM::CONTROL) {
        mods |= KeyModifiers::CONTROL;
    }
    if key.modifiers.contains(CKM::ALT) {
        mods |= KeyModifiers::ALT;
    }
    RealmKey::new(code, mods)
}
