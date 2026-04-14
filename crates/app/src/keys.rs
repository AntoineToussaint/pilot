use crate::action::Action;
use crate::keymap::BINDINGS;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Key input state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMode {
    Normal,
    Detail,
    PanePrefix,
    Terminal,
}

pub fn map_key(key: KeyEvent, mode: KeyMode) -> Action {
    // Global Ctrl-C quits (except terminal).
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if mode != KeyMode::Terminal {
            return Action::Quit;
        }
    }

    // Terminal mode: only a few escape keys, everything else → PTY.
    if mode == KeyMode::Terminal {
        return map_terminal(key);
    }

    // PanePrefix mode: one-shot pane operation.
    if mode == KeyMode::PanePrefix {
        return map_pane_prefix(key);
    }

    // Number keys 1-9: switch tabs (Normal and Detail).
    if let KeyCode::Char(c @ '1'..='9') = key.code {
        if key.modifiers.is_empty() {
            return Action::GoToTab((c as usize) - ('0' as usize));
        }
    }

    // Look up in the keymap. First match for the current mode wins.
    for (_category, bindings) in BINDINGS {
        for b in *bindings {
            if b.modes.contains(&mode) && matches_key(key, b.key, b.modifiers) {
                return (b.action)();
            }
        }
    }

    // Detail mode j/k → DetailCursorDown/Up (override generic SelectNext/Prev).
    // This is handled by having Detail-specific bindings in the keymap above.

    Action::None
}

fn matches_key(event: KeyEvent, code: KeyCode, modifiers: KeyModifiers) -> bool {
    if event.code != code {
        // Also match Down/Up arrows to j/k bindings.
        let alt_match = matches!(
            (event.code, code),
            (KeyCode::Down, KeyCode::Char('j'))
                | (KeyCode::Up, KeyCode::Char('k'))
                | (KeyCode::Left, KeyCode::Left)
                | (KeyCode::Right, KeyCode::Right)
        );
        if !alt_match {
            return false;
        }
    }
    // For NONE modifiers, accept any modifier state (so Shift doesn't block).
    if modifiers == KeyModifiers::NONE {
        // But for chars with shift (like 'M'), we need exact match.
        if let KeyCode::Char(c) = code {
            if c.is_uppercase() {
                return event.modifiers.contains(KeyModifiers::SHIFT)
                    || event.code == KeyCode::Char(c);
            }
        }
        true
    } else {
        event.modifiers.contains(modifiers)
    }
}

/// Terminal mode: almost everything → PTY.
fn map_terminal(key: KeyEvent) -> Action {
    // Tab — always cycles panes, never sent to PTY.
    if key.code == KeyCode::Tab {
        return Action::FocusPaneNext;
    }
    if key.code == KeyCode::Char('w') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::WaitingPrefix;
    }
    // Ctrl-] — escape terminal (may arrive as Char(']') or raw byte).
    if key.code == KeyCode::Char(']') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::FocusPaneNext;
    }
    // Ctrl-o — reliable alternative escape from terminal mode.
    if key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::FocusPaneNext;
    }
    if let KeyCode::Char(c @ '1'..='9') = key.code {
        if key.modifiers.contains(KeyModifiers::ALT) {
            return Action::GoToTab((c as usize) - ('0' as usize));
        }
    }
    Action::None
}

/// Pane prefix (Ctrl-w + ...).
fn map_pane_prefix(key: KeyEvent) -> Action {
    use crate::action::ShellKind;
    match key.code {
        KeyCode::Char('v') => Action::SplitHorizontal,
        KeyCode::Char('s') => Action::SplitVertical,
        KeyCode::Char('c') | KeyCode::Char('q') => Action::ClosePane,
        KeyCode::Char('h') | KeyCode::Left => Action::FocusPaneLeft,
        KeyCode::Char('j') | KeyCode::Down => Action::FocusPaneDown,
        KeyCode::Char('k') | KeyCode::Up => Action::FocusPaneUp,
        KeyCode::Char('l') | KeyCode::Right => Action::FocusPaneRight,
        KeyCode::Char('+') | KeyCode::Char('=') => Action::ResizePane(5),
        KeyCode::Char('-') => Action::ResizePane(-5),
        KeyCode::Char('z') => Action::FullscreenToggle,
        KeyCode::Char('x') => Action::CloseTab,
        KeyCode::Char('t') => Action::OpenSession(ShellKind::Claude),
        _ => Action::None,
    }
}

/// Encode a crossterm KeyEvent as raw bytes for the PTY.
pub fn key_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(vec![(c as u8) & 0x1f])
        }
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        _ => None,
    }
}
