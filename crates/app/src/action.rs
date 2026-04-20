use std::path::PathBuf;

use crossterm::event::{KeyEvent, MouseEvent};
use pilot_events::Event;

/// All actions the app can perform, funnelled through a single channel.
#[derive(Debug, Clone)]
pub enum Action {
    // -- Terminal events --
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Resize { width: u16, height: u16 },
    Tick,

    // -- External events from providers --
    ExternalEvent(Event),

    // -- Session navigation --
    SelectNext,
    SelectPrev,

    // -- Pane management (Ctrl-w prefix) --
    SplitVertical,
    SplitHorizontal,
    ClosePane,
    FocusPaneUp,
    FocusPaneDown,
    FocusPaneLeft,
    FocusPaneRight,
    FocusPaneNext,
    ResizePane(i16),
    FullscreenToggle,

    // -- Tabs --
    NextTab,
    PrevTab,
    GoToTab(usize),
    CloseTab,

    // -- Session management --
    OpenSession(ShellKind),
    WorktreeReady {
        session_key: String,
        path: PathBuf,
    },
    MarkRead,
    ToggleRepo(String),
    ToggleSession(String),
    /// Collapse whatever the cursor is on (repo or session).
    CollapseSelected,
    /// Expand whatever the cursor is on (repo or session).
    ExpandSelected,

    // -- Detail pane actions --
    DetailCursorUp,
    DetailCursorDown,
    ToggleCommentSelect,
    SelectAllComments,
    /// Send selected comments to Claude for fixing.
    FixWithClaude,
    /// Draft a reply to selected comments.
    ReplyWithClaude,

    // -- MCP confirmation --
    McpConfirmRequest {
        tool: String,
        display: String,
        /// Channel to send the approval/rejection response back.
        response_tx: std::sync::mpsc::Sender<bool>,
    },
    ApproveMcpAction,
    RejectMcpAction,

    // -- PR actions --
    MergePr,
    /// Merge succeeded — set state to Merged and clean up.
    MergeCompleted { session_key: String },
    /// Open PR in browser.
    OpenInBrowser,
    /// Send a Slack reminder to reviewers.
    SlackNudge,

    // -- Snooze --
    /// Snooze the selected session for N hours.
    Snooze,

    // -- Monitor --
    /// Toggle monitor mode for the selected session.
    ToggleMonitor,
    /// Internal: drive the monitor state machine for a session.
    MonitorTick { session_key: String },

    // -- Picker (reviewer/assignee editing) --
    EditReviewers,
    EditAssignees,
    PickerConfirm,
    PickerCancel,
    CollaboratorsLoaded {
        repo: String,
        kind: PickerKind,
        session_key: String,
        pr_number: String,
        collaborators: Vec<String>,
        current: Vec<String>,
    },

    // -- Help --
    ToggleHelp,

    // -- Time filter --
    CycleTimeFilter,

    // -- Search --
    SearchActivate,
    SearchInput(char),
    SearchBackspace,
    SearchClear,

    // -- New session --
    NewSession,
    NewSessionConfirm { description: String },
    NewSessionCancel,

    // -- Quick reply --
    /// Quick reply -- open text input to post a comment directly.
    QuickReply,
    QuickReplyConfirm { body: String },
    QuickReplyCancel,

    // -- Status --
    /// Status message from an async task (shown in status bar).
    StatusMessage(String),

    /// Cache the default branch for a repo (fetched asynchronously).
    CacheDefaultBranch { repo: String, branch: String },

    // -- Commands --
    Refresh,
    Quit,

    /// Waiting for a second key (e.g. after Ctrl-w).
    WaitingPrefix,
    None,
}

/// What to spawn in the embedded terminal.
#[derive(Debug, Clone, Copy)]
pub enum ShellKind {
    Claude,
    Shell,
}

/// What the picker overlay is editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Reviewer,
    Assignee,
}
