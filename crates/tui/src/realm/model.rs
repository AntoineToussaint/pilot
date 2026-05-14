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
use tuirealm::ratatui::layout::Rect;
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
    /// Confirm dialog asking the user to remove a workspace that fell
    /// out of scope while having running terminals. The pending
    /// workspace_key lives in `pending_removal_prompt` so the
    /// `Msg::Confirmed(true)` handler knows what to delete.
    RemoveOutOfScope,
    /// Confirm dialog asking the user to merge an issue workspace
    /// (that has live sessions) into the PR that closes it. The
    /// (issue, PR) keys live in `active_merge_prompt`; `Msg::Confirmed`
    /// dispatches `Command::ConfirmMerge` back to the daemon.
    MergeConfirm,
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
    /// Setup wizard / settings palette / editor-open state — see
    /// `SetupCtx`. Lives in one struct so the eight related fields
    /// don't clutter the top-level Model definition.
    setup: SetupCtx,
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
    /// Splits, last-viewport snapshot, and active drag — see
    /// `LayoutCtx`.
    layout: LayoutCtx,
    /// Workspace key the reply textarea (if mounted) is targeting.
    /// Set by `mount_reply`; consumed by `Msg::TextareaSubmitted` to
    /// build the `Command::PostReply` payload.
    pending_reply: Option<pilot_core::SessionKey>,
    /// Workspaces that fell out of scope (filter / scope change) but
    /// have running terminals — the daemon won't auto-remove those.
    /// Each `WorkspaceOutOfScope` event lands here; one at a time
    /// gets surfaced as a Confirm modal so the user decides whether
    /// to kill the running sessions.
    pending_removal_prompts:
        std::collections::VecDeque<(pilot_core::WorkspaceKey, String, Option<String>, usize)>,
    /// Workspace currently being prompted about. Set when the
    /// RemoveOutOfScope modal mounts; consumed by `Msg::Confirmed`.
    active_removal_prompt: Option<pilot_core::WorkspaceKey>,
    /// Pending issue→PR merge prompts. Daemon stalls a merge when
    /// the issue has live sessions and emits
    /// `WorkspaceMergePending`; we queue here and surface one at a
    /// time as a Confirm modal. Tuple: issue key, PR key, issue
    /// label, PR label, live terminal count.
    pending_merge_prompts: std::collections::VecDeque<(
        pilot_core::WorkspaceKey,
        pilot_core::WorkspaceKey,
        String,
        String,
        usize,
    )>,
    /// (issue, PR) pair currently being prompted about. Consumed by
    /// `Msg::Confirmed` when the top modal is `Id::MergeConfirm`.
    active_merge_prompt: Option<(pilot_core::WorkspaceKey, pilot_core::WorkspaceKey)>,
    /// Transient UI status (polling spinner + footer notice). See
    /// `StatusCtx`.
    status: StatusCtx,
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

use crate::realm::layout::{pane_areas, LayoutCtx, SPLIT_STEP};
use crate::realm::setup_ctx::{SettingsAction, SetupCtx};
use crate::realm::status_ctx::StatusCtx;

/// How long the first `q` stays armed waiting for the second tap.
const Q_DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(800);

/// Escape-char for the terminal-pane breakout sequence. Two
/// consecutive presses (with no intervening non-`]` key) returns
/// focus to the sidebar instead of forwarding to the PTY.
const TERMINAL_ESCAPE_CHAR: char = ']';

impl<T: TerminalAdapter> Model<T> {
    /// Backend-independent constructor — both `new` (crossterm) and
    /// `new_for_test` (TestTerminalAdapter) go through this so the
    /// common Application setup + field initializers only live in
    /// one place. Callers are responsible for prepping the terminal
    /// (raw mode, alt screen, mouse capture) before passing it in.
    fn build(terminal: T, client: Client) -> Self {
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
        Self {
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
            setup: SetupCtx::new(),
            modal_event_tx,
            q_armed_at: None,
            escape_armed_at: None,
            preselect: None,
            layout: LayoutCtx::new(),
            pending_reply: None,
            pending_removal_prompts: std::collections::VecDeque::new(),
            active_removal_prompt: None,
            pending_merge_prompts: std::collections::VecDeque::new(),
            active_merge_prompt: None,
            status: StatusCtx::new(),
        }
    }
}

impl Model<CrosstermTerminalAdapter> {
    pub fn new(client: Client) -> anyhow::Result<Self> {
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
        // Splash is mounted lazily by `start_setup_wizard`. Returning
        // users (with a persisted setup) boot straight to the panes.
        let mut model = Self::build(terminal, client);
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
        let terminal = tuirealm::terminal::TestTerminalAdapter::new(size)
            .map_err(|e| anyhow::anyhow!("test adapter init: {e:?}"))?;
        Ok(Self::build(terminal, client))
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
        hook: std::sync::Arc<
            dyn Fn(crate::setup_flow::SetupOutcome) + Send + Sync,
        >,
    ) -> Self {
        self.setup.on_complete = Some(hook);
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
        self.setup.inputs = Some((report.clone(), sources.clone()));
        self.setup.runner = Some(crate::setup_flow::SetupRunner::new(report, sources));
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
        self.setup.inputs = Some((report, sources));
    }

    /// Cache the user's existing PersistedSetup so partial flows
    /// from the Settings palette can pre-seed the wizard with
    /// current state instead of starting from defaults.
    pub fn cache_persisted_setup(&mut self, persisted: pilot_core::PersistedSetup) {
        self.setup.persisted = Some(persisted);
        // Mirror narrowed-repo scopes into the sidebar so headers
        // appear at startup, before the first poll completes.
        self.refresh_subscribed_repos();
    }

    /// Hand in the editors detected at startup. The `E` shortcut
    /// reads from this list; empty list = footer notice on `E`.
    pub fn cache_editors(&mut self, editors: Vec<crate::editors::EditorTemplate>) {
        self.setup.editors = editors;
    }

    /// Apply `~/.pilot/config.yaml::attention` +
    /// `ui.collapsed_repos` + `agent_shortcuts` to the sidebar at
    /// startup. Must be called before the first daemon Subscribe
    /// so the saved collapse state is in place when the Snapshot
    /// arrives.
    pub fn apply_sidebar_config(
        &mut self,
        attention: pilot_config::AttentionConfig,
        collapsed_repos: std::collections::BTreeSet<String>,
        agent_shortcuts: std::collections::HashMap<char, String>,
        default_agent: Option<String>,
        display: &pilot_config::DisplayConfig,
    ) {
        // Both panes consume the configured agent: sidebar `f` for
        // CI-fail, right pane `f` for selected comments.
        if let Some(agent) = default_agent.clone().filter(|s| !s.is_empty()) {
            self.right.set_default_agent(agent);
        }
        self.sidebar.apply_inner_config(
            attention,
            collapsed_repos,
            agent_shortcuts,
            default_agent,
            display,
        );
    }

    /// Push the GitHub-style scope ids (e.g. `github:owner/repo`) the
    /// user is subscribed to into the sidebar so a freshly-added
    /// repo gets a header even before polling finds workspaces.
    /// Called at startup with the persisted state and again on
    /// every wizard Finish.
    fn refresh_subscribed_repos(&mut self) {
        let mut scopes = std::collections::BTreeSet::new();
        if let Some(p) = &self.setup.persisted {
            for set in p.selected_scopes.values() {
                scopes.extend(set.iter().cloned());
            }
        }
        self.sidebar.apply_subscribed_scopes(&scopes);
    }

    /// Send a command to the daemon, logging failures. Wraps the raw
    /// `client.send` so a dead channel (daemon restarted, socket
    /// closed) leaves a breadcrumb in `/tmp/pilot.log` instead of
    /// silently vanishing. Most call sites genuinely don't care if
    /// the send fails (Subscribe is idempotent, terminal-Write loses
    /// keystrokes on a dead channel anyway) — but a silent log helps
    /// debug "I pressed X and nothing happened" after the fact.
    fn send_cmd(&self, cmd: IpcCommand) {
        if let Err(e) = self.client.send(cmd) {
            tracing::warn!("ipc send failed: {e}");
        }
    }

    /// Override the initial sidebar / right-top split percentages
    /// from `~/.pilot/config.yaml::ui`. Each value is clamped to
    /// `[SPLIT_MIN, SPLIT_MAX]`. `None` keeps the default.
    pub fn with_splits(mut self, sidebar_pct: Option<u16>, right_top_pct: Option<u16>) -> Self {
        self.layout.apply_persisted(sidebar_pct, right_top_pct);
        self
    }

    /// Open the focused workspace's worktree in an editor. Bound to
    /// `E` from the sidebar. 1 detected editor → launch directly;
    /// 2+ → mount a Choice picker; 0 → footer notice with hint.
    /// If the workspace has no session yet (no worktree on disk),
    /// spawn a shell first — the daemon creates the worktree as a
    /// side-effect, and the editor launches once `TerminalSpawned`
    /// arrives.
    pub fn open_editor(&mut self) {
        use crate::realm::components::footer::{Notice, NoticeSeverity};

        let Some(workspace_key) = self.sidebar.selected_workspace_key().cloned() else {
            return;
        };
        if self.setup.editors.is_empty() {
            self.status.notice = Some(Notice::new(
                "no editor detected — add one under `editors:` in ~/.pilot/config.yaml",
                NoticeSeverity::Info,
            ));
            self.redraw = true;
            return;
        }
        let worktree = self
            .sidebar
            .selected_workspace()
            .and_then(|w| w.sessions.first().map(|s| s.worktree_path.clone()));
        // If there's no worktree yet, queue the editor launch and
        // ask the daemon to provision a session — `handle_daemon_event`
        // fires the editor on the matching `TerminalSpawned`.
        let Some(worktree) = worktree else {
            // Pick the editor up front (or remember the picker is
            // pending). Single editor → queue + spawn immediately.
            // Multiple → show the picker first, queue when picked.
            if self.setup.editors.len() == 1 {
                self.setup.pending_editor_launch =
                    Some((workspace_key.clone(), self.setup.editors[0].clone()));
                self.send_cmd(IpcCommand::Spawn {
                    session_key: workspace_key.clone(),
                    session_id: None,
                    kind: pilot_ipc::TerminalKind::Shell,
                    cwd: None,
                    initial_prompt: None,
                });
                self.status.notice = Some(Notice::new(
                    format!(
                        "Provisioning worktree for {workspace_key} — opening in {} when ready…",
                        self.setup.editors[0].display
                    ),
                    NoticeSeverity::Info,
                ));
                self.redraw = true;
            } else {
                // Multi-editor: defer editor pick + record that the
                // dispatch needs to spawn first.
                self.setup.pending_editor_workspace = Some(workspace_key);
                self.mount_editor_picker();
            }
            return;
        };

        match self.setup.editors.len() {
            1 => {
                let editor = self.setup.editors[0].clone();
                self.launch_editor(&editor, &worktree);
            }
            _ => self.mount_editor_picker(),
        }
    }

    fn mount_editor_picker(&mut self) {
        use crate::realm::components::choice::Choice;
        use tuirealm::subscription::{EventClause, Sub, SubClause};
        let labels: Vec<String> =
            self.setup.editors.iter().map(|e| e.display.clone()).collect();
        self.setup.editor_choices = self.setup.editors.clone();
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
                self.status.notice = Some(Notice::new(
                    format!("opened {} in {}", worktree.display(), editor.display),
                    NoticeSeverity::Info,
                ));
            }
            Err(e) => {
                tracing::warn!("editor launch failed: {e}");
                self.status.notice = Some(Notice::new(
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

        if self.setup.runner.is_some() || matches!(self.modal_stack.last(), Some(Id::Setup)) {
            return;
        }

        let actions = self.build_settings_actions();
        if actions.is_empty() {
            // No persisted setup → fall back to the full wizard.
            self.reopen_setup();
            return;
        }
        let labels: Vec<String> = actions.iter().map(|a| a.label()).collect();
        self.setup.settings_actions = actions;
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
        let Some(p) = &self.setup.persisted else {
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
        let Some((report, sources)) = self.setup.inputs.clone() else {
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
        let outcome = match self.setup.persisted.clone() {
            Some(p) => crate::setup_flow::persisted_to_outcome(p, report),
            None => crate::setup_flow::SetupOutcome::default_enabled(report),
        };
        let (runner, step) = SetupRunner::at_partial(outcome, sources, entry);
        self.setup.runner = Some(runner);
        let owned_runner = self.setup.runner.take().expect("just set");
        self.handle_runner_step(owned_runner, step);
    }

    /// Re-open the full setup wizard mid-session. Uses the cached
    /// `(report, sources)` populated at startup. No-op when the
    /// cache is empty (`--test`, `--connect`).
    pub fn reopen_setup(&mut self) {
        if self.setup.runner.is_some() {
            return;
        }
        let Some((report, sources)) = self.setup.inputs.clone() else {
            tracing::warn!("reopen_setup: no cached setup inputs");
            return;
        };
        self.start_setup_wizard(report, sources);
    }

    /// Mount the first-poll progress modal. Called from the
    /// on-setup-complete hook (and from the returning-user kickoff
    /// path) once polling has been kicked off on the daemon side.
    pub fn show_polling(&mut self, sources: Vec<String>) {
        self.status.show_polling(sources);
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
        let sidebar_pct = self.layout.sidebar_pct;
        let right_top_pct = self.layout.right_top_pct;
        let polling_status: Option<(&'static str, String)> = self
            .status
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
        let notice = self.status.notice.clone();
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
        self.layout.last_area = captured_area;
        // Resize commands are queued by the terminal stack's render
        // path each time a slot's rect changes. Drain + ship them so
        // libghostty's PTY learns the new size — without this,
        // typing into a freshly-shown terminal produces output that
        // falls off the bottom of the live grid.
        for cmd in self.terminals.drain_cmds() {
            self.send_cmd(cmd);
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
                if let Some(mut runner) = self.setup.runner.take() {
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
                    self.send_cmd(cmd);
                }
            }
            Msg::RightCmds => {
                for cmd in self.right.drain_cmds() {
                    self.send_cmd(cmd);
                }
            }
            Msg::TerminalCmds => {
                for cmd in self.terminals.drain_cmds() {
                    self.send_cmd(cmd);
                }
            }
            Msg::ChoicePicked(picks) => {
                // Editor picker (Id::Editor) — pick → launch (or
                // defer behind a session-spawn when the workspace
                // has no worktree yet).
                if matches!(self.modal_stack.last(), Some(Id::Editor)) {
                    let editor = picks
                        .first()
                        .and_then(|i| self.setup.editor_choices.get(*i).cloned());
                    self.setup.editor_choices.clear();
                    self.pop_modal();
                    let Some(editor) = editor else { return };
                    // Was this open-editor deferred behind a worktree
                    // creation? If so, queue + spawn shell.
                    if let Some(workspace_key) =
                        self.setup.pending_editor_workspace.take()
                    {
                        self.setup.pending_editor_launch =
                            Some((workspace_key.clone(), editor.clone()));
                        self.send_cmd(IpcCommand::Spawn {
                            session_key: workspace_key.clone(),
                            session_id: None,
                            kind: pilot_ipc::TerminalKind::Shell,
                            cwd: None,
                            initial_prompt: None,
                        });
                        use crate::realm::components::footer::{
                            Notice, NoticeSeverity,
                        };
                        self.status.notice = Some(Notice::new(
                            format!(
                                "Provisioning worktree for {workspace_key} — opening in {} when ready…",
                                editor.display
                            ),
                            NoticeSeverity::Info,
                        ));
                        self.redraw = true;
                        return;
                    }
                    // Worktree already on disk — launch directly.
                    let worktree = self
                        .sidebar
                        .selected_workspace()
                        .and_then(|w| w.sessions.first().map(|s| s.worktree_path.clone()));
                    if let Some(worktree) = worktree {
                        self.launch_editor(&editor, &worktree);
                    }
                }
                // Settings palette is a non-runner Choice modal — if
                // the user just picked an action, route into a
                // partial wizard flow before falling through.
                else if !self.setup.settings_actions.is_empty()
                    && matches!(self.modal_stack.last(), Some(Id::Setup))
                    && self.setup.runner.is_none()
                {
                    let action = picks
                        .first()
                        .and_then(|i| self.setup.settings_actions.get(*i).cloned());
                    self.setup.settings_actions.clear();
                    self.pop_modal();
                    if let Some(action) = action {
                        self.dispatch_settings_action(action);
                    }
                } else if let Some(mut runner) = self.setup.runner.take() {
                    let step = runner.step_choice_picked(picks);
                    self.handle_runner_step(runner, step);
                } else {
                    self.pop_modal();
                }
            }
            Msg::ChoiceRefresh => {
                if let Some(mut runner) = self.setup.runner.take() {
                    let step = runner.step_choice_refresh();
                    self.handle_runner_step(runner, step);
                }
            }
            Msg::ChoiceBack => {
                if let Some(mut runner) = self.setup.runner.take() {
                    let step = runner.step_choice_back();
                    self.handle_runner_step(runner, step);
                } else {
                    self.pop_modal();
                }
            }
            Msg::LoadingResolved(carrier) => {
                if let Some(mut runner) = self.setup.runner.take() {
                    let payload = carrier.take().unwrap_or_else(|| Box::new(()));
                    let step = runner.step_loading_resolved(payload);
                    self.handle_runner_step(runner, step);
                } else {
                    self.pop_modal();
                }
            }
            Msg::ModalDismissed => {
                if let Some(mut runner) = self.setup.runner.take() {
                    let step = runner.step_dismissed();
                    self.handle_runner_step(runner, step);
                } else {
                    // Dispatch by which modal was on top BEFORE the
                    // pop so we route the "no" decision correctly.
                    let top = self.modal_stack.last().cloned();
                    self.pop_modal();
                    match top {
                        Some(Id::RemoveOutOfScope) => {
                            self.active_removal_prompt = None;
                        }
                        Some(Id::MergeConfirm) => {
                            // Esc on the merge modal = "no, keep
                            // them separate." Tell the daemon to drop
                            // the stall so future polls don't
                            // re-prompt.
                            if let Some((issue_key, pr_key)) =
                                self.active_merge_prompt.take()
                            {
                                self.send_cmd(IpcCommand::ConfirmMerge {
                                    issue_workspace_key: issue_key,
                                    pr_workspace_key: pr_key,
                                    accept: false,
                                });
                            }
                        }
                        _ => {}
                    }
                    // Always try to surface a queued prompt after a
                    // modal dismisses — not just when the dismissed
                    // modal itself was a prompt. Otherwise a user who
                    // has Help / Settings open when the daemon emits
                    // a prompt would have it stuck in the queue.
                    self.maybe_mount_next_removal_prompt();
                    self.maybe_mount_next_merge_prompt();
                }
            }
            Msg::Confirmed(yes) => {
                let top = self.modal_stack.last().cloned();
                self.pop_modal();
                match top {
                    Some(Id::RemoveOutOfScope) => {
                        let target = self.active_removal_prompt.take();
                        if yes
                            && let Some(workspace_key) = target
                        {
                            // Kill terminals + delete workspace.
                            let session_key: pilot_core::SessionKey =
                                (&workspace_key).into();
                            self.send_cmd(IpcCommand::Kill { session_key });
                        }
                    }
                    Some(Id::MergeConfirm) => {
                        if let Some((issue_key, pr_key)) =
                            self.active_merge_prompt.take()
                        {
                            self.send_cmd(IpcCommand::ConfirmMerge {
                                issue_workspace_key: issue_key,
                                pr_workspace_key: pr_key,
                                accept: yes,
                            });
                        }
                    }
                    _ => {}
                }
                self.maybe_mount_next_removal_prompt();
                self.maybe_mount_next_merge_prompt();
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
                    self.send_cmd(IpcCommand::PostReply {
                        session_key,
                        body,
                    });
                    // Footer hint so the user knows it submitted —
                    // poll-tick brings the new comment back into
                    // the activity feed within a few seconds (we
                    // also kick a Refresh below so it doesn't wait
                    // for the 60s loop).
                    use crate::realm::components::footer::{Notice, NoticeSeverity};
                    self.status.notice = Some(Notice::new(
                        "Reply submitted — fetching…",
                        NoticeSeverity::Info,
                    ));
                    self.send_cmd(IpcCommand::Refresh);
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
                self.status.notice = Some(Notice::new(
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
                self.setup.runner = Some(runner);
                self.mount_setup_modal(component);
            }
            RunnerStep::Finish(outcome) => {
                let sources: Vec<String> =
                    outcome.enabled_providers.iter().cloned().collect();
                // Cache the new persisted state so subsequent partial
                // flows (Settings → Add a repo) see the latest scopes.
                self.setup.persisted =
                    Some(crate::setup_flow::outcome_to_persisted(&outcome));
                // Push the new repo subscriptions into the sidebar so
                // the user sees a header for the freshly-added repo
                // even before polling finds workspaces under it.
                self.refresh_subscribed_repos();
                if let Some(hook) = self.setup.on_complete.as_ref() {
                    hook(outcome);
                }
                self.unmount_setup_modal();
                self.send_cmd(IpcCommand::Subscribe);
                // Kick off an immediate poll so a freshly added repo
                // surfaces its open PRs/issues within seconds instead
                // of waiting for the long-lived 60s loop tick.
                self.send_cmd(IpcCommand::Refresh);
                self.set_focus_attr();
                if !sources.is_empty() {
                    self.show_polling(sources);
                }
            }
            RunnerStep::Cancel => {
                self.unmount_setup_modal();
                self.send_cmd(IpcCommand::Subscribe);
                self.set_focus_attr();
            }
            RunnerStep::Stay => {
                self.setup.runner = Some(runner);
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
                if self.layout.nudge_splits(dx, dy) {
                    self.redraw = true;
                }
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
            // `r` opens the reply textarea targeted at the selected
            // workspace. Available from Sidebar AND Right (Activity)
            // — replying is more naturally "an action on the thing
            // I'm reading" than "an action on the row". Disabled in
            // Terminals where `r` belongs to the PTY.
            Key::Char('r')
                if key.modifiers.is_empty() && self.focus != PaneFocus::Terminals =>
            {
                self.q_armed_at = None;
                if let Some(workspace_key) = self.sidebar.selected_workspace_key().cloned() {
                    self.mount_reply(workspace_key);
                }
                return;
            }
            // `e` from the sidebar: open the focused workspace's
            // worktree in an editor (Zed / VS Code / Cursor / …).
            // Detection happens at startup; users add custom editors
            // in `~/.pilot/config.yaml::editors`.
            Key::Char('e')
                if key.modifiers.is_empty() && self.focus == PaneFocus::Sidebar =>
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
                self.send_cmd(cmd);
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
                self.status.notice = Some(Notice::new(
                    format!("Spawning {label}…"),
                    NoticeSeverity::Info,
                ));
            }
        }
        for cmd in cmds {
            self.send_cmd(cmd);
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
        (self.layout.sidebar_pct, self.layout.right_top_pct)
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

    /// Test entry point: drive a key through the *modal* pipeline —
    /// send into `modal_event_tx`, poll `app.tick` until the modal
    /// produces a Msg (or a short deadline elapses), then `update`
    /// each Msg. Mirrors the runloop's modal branch (lines ~2049-2106
    /// in `run_loop`). Exists because `dispatch_key` calls
    /// `handle_pane_key`, which is gated on an empty modal stack and
    /// therefore can't exercise key handling for a mounted Confirm,
    /// Input, etc.
    pub fn dispatch_modal_key(&mut self, key: RealmKey) {
        let _ = self.modal_event_tx.send(RealmEvent::Keyboard(key));
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            match self.app.tick(PollStrategy::Once(Duration::ZERO)) {
                Ok(messages) if !messages.is_empty() => {
                    for msg in messages {
                        self.update(msg);
                    }
                    return;
                }
                Ok(_) => {}
                Err(_) => return,
            }
            if std::time::Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Test entry point: drive a mouse event through `handle_mouse`
    /// after manually setting `last_area` (since `view()` would
    /// otherwise be needed to populate it).
    pub fn dispatch_mouse_in(
        &mut self,
        m: crossterm::event::MouseEvent,
        area: Rect,
    ) {
        self.layout.last_area = area;
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

        if self.layout.last_area.width == 0 || self.layout.last_area.height == 0 {
            return;
        }
        let (sidebar_rect, right_top_rect, right_bottom_rect) = pane_areas(
            self.layout.last_area,
            self.layout.sidebar_pct,
            self.layout.right_top_pct,
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
                    && self.layout.hit_test_splitter(
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
                        self.send_cmd(IpcCommand::Write {
                            terminal_id,
                            bytes,
                        });
                        self.redraw = true;
                        return;
                    }
                }
                if let Some(target) = self.layout.hit_test_splitter(
                    m.column,
                    m.row,
                    sidebar_rect,
                    right_top_rect,
                ) {
                    self.layout.active_drag = Some(target);
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
                if let Some(target) = self.layout.active_drag {
                    if self.layout.update_drag(target, m.column, m.row) {
                        self.redraw = true;
                    }
                }
            }
            MouseEventKind::Up(button) => {
                let was_drag = self.layout.active_drag.take().is_some();
                if was_drag {
                    // Persist the final split percentages — drag
                    // events fire dozens per second, so we deferred
                    // the write until release.
                    self.layout.persist();
                }
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
                        self.send_cmd(IpcCommand::Write {
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
                        self.send_cmd(IpcCommand::Write {
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

    /// If there's a queued "out-of-scope workspace has active
    /// sessions" prompt and no modal is currently up, mount it. The
    /// user's answer (Y → kill, N/Esc → keep) is handled in the
    /// `Msg::Confirmed` / `Msg::ModalDismissed` arms.
    fn maybe_mount_next_removal_prompt(&mut self) {
        use crate::realm::components::confirm::Confirm;
        use tuirealm::subscription::{EventClause, Sub, SubClause};

        if !self.modal_stack.is_empty() {
            return;
        }
        let Some((workspace_key, label, title, count)) =
            self.pending_removal_prompts.pop_front()
        else {
            return;
        };
        let terminals_phrase = if count == 1 {
            "1 running terminal".to_string()
        } else {
            format!("{count} running terminals")
        };
        // Trim the title so a verbose PR description doesn't make the
        // modal three lines tall. 80 chars + an ellipsis fits within
        // the dynamic-height Confirm modal cleanly.
        let runner_label = match title.as_deref().filter(|s| !s.is_empty()) {
            Some(t) => {
                let title_short = if t.chars().count() > 80 {
                    let truncated: String = t.chars().take(79).collect();
                    format!("{truncated}…")
                } else {
                    t.to_string()
                };
                format!(
                    "{label} \"{title_short}\" is no longer in your filter scope but has {terminals_phrase} — kill and remove?"
                )
            }
            None => format!(
                "{label} is no longer in your filter scope but has {terminals_phrase} — kill and remove?"
            ),
        };
        let modal = Confirm::new(runner_label).default_no();
        self.active_removal_prompt = Some(workspace_key);
        let _ = self.app.mount(
            Id::RemoveOutOfScope,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::RemoveOutOfScope);
        let _ = self.app.active(&Id::RemoveOutOfScope);
        self.redraw = true;
    }

    /// Surface the next queued issue→PR merge prompt when no modal
    /// is currently up. The user's answer drives `Msg::Confirmed` /
    /// `Msg::ModalDismissed`, which dispatch a `Command::ConfirmMerge`
    /// back to the daemon. Default-no: silently absorbing a session
    /// the user is in the middle of using would be the surprising
    /// outcome, so Enter biases toward "leave them separate".
    fn maybe_mount_next_merge_prompt(&mut self) {
        use crate::realm::components::confirm::Confirm;
        use tuirealm::subscription::{EventClause, Sub, SubClause};

        if !self.modal_stack.is_empty() {
            return;
        }
        let Some((issue_key, pr_key, issue_label, pr_label, count)) =
            self.pending_merge_prompts.pop_front()
        else {
            return;
        };
        let terminals_phrase = if count == 1 {
            "1 running terminal".to_string()
        } else {
            format!("{count} running terminals")
        };
        let question = format!(
            "{pr_label} closes {issue_label}, which has {terminals_phrase}. \
             Merge the issue's sessions into the PR workspace?",
        );
        let modal = Confirm::new(question).default_no();
        self.active_merge_prompt = Some((issue_key, pr_key));
        let _ = self.app.mount(
            Id::MergeConfirm,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::MergeConfirm);
        let _ = self.app.active(&Id::MergeConfirm);
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

        // Out-of-scope workspaces with running terminals — queue a
        // Confirm prompt before killing anything. Don't forward the
        // event to panes; they'd just ignore it anyway and a queued
        // prompt is the only reasonable response.
        if let IpcEvent::WorkspaceOutOfScope {
            workspace_key,
            label,
            title,
            active_terminal_count,
        } = &event
        {
            // Dedupe: ignore re-emits for the workspace currently
            // being prompted about OR already queued. The daemon
            // dedupes per-process, but a daemon restart would reset
            // its state and could spam the same prompt. Belt and
            // braces.
            let already_active = self
                .active_removal_prompt
                .as_ref()
                .map(|k| k == workspace_key)
                .unwrap_or(false);
            let already_queued = self
                .pending_removal_prompts
                .iter()
                .any(|(k, _, _, _)| k == workspace_key);
            if !already_active && !already_queued {
                self.pending_removal_prompts.push_back((
                    workspace_key.clone(),
                    label.clone(),
                    title.clone(),
                    *active_terminal_count,
                ));
                self.maybe_mount_next_removal_prompt();
                self.redraw = true;
            }
            return;
        }
        // Same pattern for issue→PR merge prompts: queue + surface
        // one at a time so the modal stack doesn't pile up.
        if let IpcEvent::WorkspaceMergePending {
            issue_workspace_key,
            pr_workspace_key,
            issue_label,
            pr_label,
            active_terminal_count,
        } = &event
        {
            let already_active = self
                .active_merge_prompt
                .as_ref()
                .map(|(i, _)| i == issue_workspace_key)
                .unwrap_or(false);
            let already_queued = self
                .pending_merge_prompts
                .iter()
                .any(|(i, _, _, _, _)| i == issue_workspace_key);
            if !already_active && !already_queued {
                self.pending_merge_prompts.push_back((
                    issue_workspace_key.clone(),
                    pr_workspace_key.clone(),
                    issue_label.clone(),
                    pr_label.clone(),
                    *active_terminal_count,
                ));
                self.maybe_mount_next_merge_prompt();
                self.redraw = true;
            }
            return;
        }
        // Silent-merge notice: the daemon collapsed an issue row into
        // its PR without prompting (no live sessions to worry about).
        // Flash a footer line so the row disappearance has context.
        if let IpcEvent::WorkspaceMerged {
            issue_label,
            pr_label,
            ..
        } = &event
        {
            use crate::realm::components::footer::{Notice, NoticeSeverity};
            self.status.notice = Some(Notice::new(
                format!("merged {issue_label} into {pr_label}"),
                NoticeSeverity::Info,
            ));
            self.redraw = true;
            return;
        }
        self.sidebar.on_daemon_event(&event);
        self.right.on_daemon_event(&event);
        self.terminals.on_daemon_event(&event);
        if let Some(p) = self.status.polling.as_mut() {
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
            self.status.clear_spawning_notice();
            self.sync_panes();
            // Editor-deferred-by-spawn: the user pressed `e` on a
            // workspace with no worktree; we asked the daemon to
            // spawn a shell so a worktree got provisioned. Look
            // up the queued target's worktree from the sidebar's
            // workspace map (NOT `selected_workspace()`) so the
            // launch fires even if the user has since navigated
            // to a different workspace.
            if let Some((target_key, editor)) = self.setup.pending_editor_launch.clone()
                && let Some(worktree) = self
                    .sidebar
                    .workspace_by_key(&target_key)
                    .and_then(|w| w.sessions.first().map(|s| s.worktree_path.clone()))
            {
                self.setup.pending_editor_launch = None;
                self.launch_editor(&editor, &worktree);
            }
        } else {
            self.sync_panes();
        }
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
        if self.status.tick_notice() {
            self.redraw = true;
        }
    }

    /// Drive the polling spinner + termination check from the run
    /// loop. Cheap; called every iteration. Returns Some(msg) when
    /// the polling modal wants to be torn down.
    pub fn polling_tick(&mut self) -> Option<Msg> {
        let msg = self.status.polling_tick();
        if msg.is_some() {
            self.redraw = true;
        }
        msg
    }

    /// Tear down the polling modal. Called when its tick / feed
    /// returns Some(msg) (saw workspace, timed out, etc.).
    fn dismiss_polling(&mut self) {
        if self.status.dismiss_polling() {
            self.redraw = true;
        }
    }

    /// Project sidebar selection onto the right pane + terminal stack.
    /// Cheap to call; the inner setters bail when nothing changed.
    /// Called after every key dispatch and every daemon event.
    fn sync_panes(&mut self) {
        let workspace = self.sidebar.selected_workspace().cloned();
        let session_key = self.sidebar.selected_workspace_key().cloned();
        // Also forward the workspace's persisted SessionLayout to
        // the terminal stack so the user's tile arrangement
        // follows them across workspace switches. Each workspace's
        // default session carries its own Tabs/Splits state; the
        // stack used to keep whatever layout the LAST workspace
        // had, so jumping from a split workspace to a tabs one
        // would render the new one with the old split's tree.
        let layout = workspace
            .as_ref()
            .and_then(|w| w.default_session())
            .map(|s| s.layout.clone())
            .unwrap_or_default();
        self.right.set_workspace(workspace);
        self.terminals.set_active_session(session_key);
        self.terminals.set_layout(layout);
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
                        // ChannelPort is polled by the listener thread
                        // every 10ms, so a tight 15ms window often
                        // expires before the listener delivers the
                        // event we just pushed — the keypress sits in
                        // the channel and isn't acted on until the
                        // user presses another key. The Confirm modal
                        // showed this loudly: "Y not responsive; Esc
                        // worked after a few tries".
                        //
                        // Poll in a short loop with a 150ms deadline
                        // so we keep checking until messages arrive or
                        // the user perceives latency. 150ms is well
                        // under the human-noticeable threshold for
                        // key feedback but long enough to absorb the
                        // 10ms listener cadence + system jitter.
                        let deadline = std::time::Instant::now()
                            + Duration::from_millis(150);
                        let mut handled = false;
                        loop {
                            match model
                                .app
                                .tick(PollStrategy::Once(Duration::ZERO))
                            {
                                Ok(messages) if !messages.is_empty() => {
                                    for msg in messages {
                                        model.update(msg);
                                    }
                                    handled = true;
                                    break;
                                }
                                Ok(_) => {}
                                Err(_) => break,
                            }
                            if std::time::Instant::now() >= deadline {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        // After the first tick lands, drain anything
                        // else the modal pushed in the same window —
                        // a single tuirealm `Cmd` can fan out into
                        // multiple `Msg`s and we don't want them to
                        // straggle into the next keypress.
                        if handled
                            && let Ok(messages) =
                                model.app.tick(PollStrategy::Once(Duration::ZERO))
                        {
                            for msg in messages {
                                model.update(msg);
                            }
                        }
                        // Modals can mutate internal state without
                        // producing a `Msg`, so force a redraw too.
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
