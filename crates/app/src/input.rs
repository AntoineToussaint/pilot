//! Input mode state machine for key handling.
//!
//! Replaces the nested if/else chain in the `Action::Key` handler with a
//! single `match app.input_mode` dispatch. Only ONE mode is active at a
//! time -- no nesting, no fallthrough.
//!
//! Priority order (highest first):
//!   1. Help        -- any key dismisses
//!   2. TextInput   -- search, quick-reply, new-session overlays
//!   3. Picker      -- reviewer/assignee selection overlay
//!   4. Normal / Detail / Terminal / PanePrefix -- regular key mapping

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
    /// Convert to `KeyMode` for keybinding lookup.
    /// Overlay modes map to the "base" KeyMode they'd return to.
    pub fn to_key_mode(&self) -> crate::keys::KeyMode {
        match self {
            InputMode::Normal => crate::keys::KeyMode::Normal,
            InputMode::Detail => crate::keys::KeyMode::Detail,
            InputMode::Terminal => crate::keys::KeyMode::Terminal,
            InputMode::PanePrefix => crate::keys::KeyMode::PanePrefix,
            // Overlay modes don't map to keybindings directly,
            // but if asked, return Normal as a safe default.
            InputMode::TextInput(_) | InputMode::Picker | InputMode::Help => {
                crate::keys::KeyMode::Normal
            }
        }
    }

    /// Whether this is an overlay mode (Help, TextInput, Picker).
    pub fn is_overlay(&self) -> bool {
        matches!(
            self,
            InputMode::Help | InputMode::TextInput(_) | InputMode::Picker
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::KeyMode;

    #[test]
    fn to_key_mode_base_modes() {
        assert_eq!(InputMode::Normal.to_key_mode(), KeyMode::Normal);
        assert_eq!(InputMode::Detail.to_key_mode(), KeyMode::Detail);
        assert_eq!(InputMode::Terminal.to_key_mode(), KeyMode::Terminal);
        assert_eq!(InputMode::PanePrefix.to_key_mode(), KeyMode::PanePrefix);
    }

    #[test]
    fn to_key_mode_overlays_fall_back_to_normal() {
        assert_eq!(
            InputMode::TextInput(TextInputKind::Search).to_key_mode(),
            KeyMode::Normal
        );
        assert_eq!(InputMode::Picker.to_key_mode(), KeyMode::Normal);
        assert_eq!(InputMode::Help.to_key_mode(), KeyMode::Normal);
    }

    #[test]
    fn is_overlay_matches_doc() {
        assert!(!InputMode::Normal.is_overlay());
        assert!(!InputMode::Detail.is_overlay());
        assert!(!InputMode::Terminal.is_overlay());
        assert!(!InputMode::PanePrefix.is_overlay());

        assert!(InputMode::Help.is_overlay());
        assert!(InputMode::Picker.is_overlay());
        assert!(InputMode::TextInput(TextInputKind::Search).is_overlay());
        assert!(InputMode::TextInput(TextInputKind::QuickReply).is_overlay());
        assert!(InputMode::TextInput(TextInputKind::NewSession).is_overlay());
    }
}
