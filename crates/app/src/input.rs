//! Input mode state machine for key handling.
//!
//! Replaces the nested if/else chain in the `Action::Key` handler with a
//! single `match app.input_mode` dispatch. Only ONE mode is active at a
//! time -- no nesting, no fallthrough.
//!
//! Priority order (highest first):
//!   1. Help        -- any key dismisses
//!   2. McpConfirm  -- y/n to approve/reject MCP action
//!   3. TextInput   -- search, quick-reply, new-session overlays
//!   4. Picker      -- reviewer/assignee selection overlay
//!   5. Normal / Detail / Terminal / PanePrefix -- regular key mapping

/// What mode the input handler is in.
/// Only ONE mode is active at a time -- no nesting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMode {
    /// Sidebar navigation.
    Normal,
    /// Detail pane -- comment navigation and PR actions.
    Detail,
    /// Terminal -- keys go to PTY.
    Terminal,
    /// Pane prefix -- waiting for one more key after Ctrl-w.
    PanePrefix,
    /// Text input overlay (search, quick reply, new session).
    TextInput(TextInputKind),
    /// Picker overlay (reviewer/assignee selection).
    Picker,
    /// MCP confirmation modal (y/n).
    McpConfirm,
    /// Help overlay (any key dismisses).
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextInputKind {
    Search,
    QuickReply,
    NewSession,
}

impl InputMode {
    /// The "base" mode derived from the KeyMode the app would normally be in.
    /// Used when exiting an overlay to return to the right mode.
    pub fn from_key_mode(mode: crate::keys::KeyMode) -> Self {
        match mode {
            crate::keys::KeyMode::Normal => InputMode::Normal,
            crate::keys::KeyMode::Detail => InputMode::Detail,
            crate::keys::KeyMode::Terminal => InputMode::Terminal,
            crate::keys::KeyMode::PanePrefix => InputMode::PanePrefix,
        }
    }
}
