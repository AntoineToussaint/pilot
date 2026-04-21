use std::path::PathBuf;

use crossterm::event::{KeyEvent, MouseEvent};
use pilot_core::SessionKey;
use pilot_events::Event;

/// All actions the app can perform, funnelled through a single channel.
///
/// `Event` and `CollaboratorsLoaded` are boxed because they're significantly
/// larger (~384 bytes) than the other variants (~16-80 bytes). Without the
/// box, every `Action` sitting in the mpsc queue would pay that space —
/// wasteful for the common `Tick`/`Key`/`Mouse` traffic.
#[derive(Debug, Clone)]
pub enum Action {
    // -- Terminal events --
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Resize { width: u16, height: u16 },
    Tick,

    // -- External events from providers --
    ExternalEvent(Box<Event>),

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
    FocusPanePrev,
    ResizePane(i16),
    FullscreenToggle,

    // -- Tabs --
    NextTab,
    PrevTab,
    GoToTab(usize),
    CloseTab,
    /// Kill the tmux session AND close the pilot tab. Unlike CloseTab which
    /// only detaches (tmux session survives), this wipes it completely so
    /// the next attach starts fresh.
    KillSession,

    // -- Session management --
    OpenSession(ShellKind),
    WorktreeReady {
        session_key: SessionKey,
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

    // -- PR actions --
    MergePr,
    /// Merge succeeded — set state to Merged and clean up.
    MergeCompleted { session_key: SessionKey },
    /// Open PR in browser.
    OpenInBrowser,
    /// Send a Slack reminder to reviewers.
    SlackNudge,
    /// Approve the selected PR (only when user's role is Reviewer or Assignee).
    ApprovePr,

    // -- Snooze --
    /// Snooze the selected session for N hours.
    Snooze,

    // -- Monitor --
    /// Toggle monitor mode for the selected session.
    ToggleMonitor,
    /// Internal: drive the monitor state machine for a session.
    MonitorTick { session_key: SessionKey },
    /// Result of a `Command::CheckNeedsRebase`. Reduce decides what to do.
    NeedsRebaseResult {
        session_key: SessionKey,
        needs_rebase: bool,
        wt_path: std::path::PathBuf,
        default_branch: String,
    },
    /// Result of `Command::RefreshLiveTmuxSessions`. Set of active tmux
    /// session names.
    TmuxSessionsRefreshed {
        sessions: std::collections::HashSet<String>,
    },

    // -- Picker (reviewer/assignee editing) --
    EditReviewers,
    EditAssignees,
    PickerConfirm,
    PickerCancel,
    CollaboratorsLoaded(Box<CollaboratorsLoaded>),

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

/// Payload for `Action::CollaboratorsLoaded`. Boxed in the enum to keep
/// the Action size down.
#[derive(Debug, Clone)]
pub struct CollaboratorsLoaded {
    pub repo: String,
    pub kind: PickerKind,
    pub session_key: SessionKey,
    pub pr_number: String,
    pub collaborators: Vec<String>,
    pub current: Vec<String>,
}
