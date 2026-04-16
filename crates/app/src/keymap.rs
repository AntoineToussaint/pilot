//! Single source of truth for all keybindings.
//!
//! Each binding defines: key, mode(s), action, and description.
//! The keys module, UI action bars, and help page all read from here.

use crate::action::{Action, ShellKind};
use crate::keys::KeyMode;
use crossterm::event::{KeyCode, KeyModifiers};

/// A single keybinding definition.
pub struct Binding {
    pub key: KeyCode,
    pub modifiers: KeyModifiers,
    pub modes: &'static [KeyMode],
    pub action: fn() -> Action,
    pub short: &'static str,  // e.g. "f"
    pub label: &'static str,  // e.g. "fix"
    pub description: &'static str, // e.g. "Send selected comments to Claude for fixing"
}

/// All keybindings, grouped by category.
pub static BINDINGS: &[(&str, &[Binding])] = &[
    ("Navigation", &[
        Binding { key: KeyCode::Char('j'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal], action: || Action::SelectNext, short: "j/↓", label: "next", description: "Move to next item" },
        Binding { key: KeyCode::Char('k'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal], action: || Action::SelectPrev, short: "k/↑", label: "prev", description: "Move to previous item" },
        Binding { key: KeyCode::Char('j'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Detail], action: || Action::DetailCursorDown, short: "j/↓", label: "next", description: "Move to next comment" },
        Binding { key: KeyCode::Char('k'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Detail], action: || Action::DetailCursorUp, short: "k/↑", label: "prev", description: "Move to previous comment" },
        Binding { key: KeyCode::Tab, modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal, KeyMode::Detail], action: || Action::FocusPaneNext, short: "Tab", label: "pane", description: "Switch pane focus" },
        Binding { key: KeyCode::Esc, modifiers: KeyModifiers::NONE, modes: &[KeyMode::Detail], action: || Action::FocusPaneNext, short: "Esc", label: "back", description: "Back to sidebar" },
        Binding { key: KeyCode::Left, modifiers: KeyModifiers::NONE, modes: &[KeyMode::Detail], action: || Action::FocusPaneLeft, short: "←", label: "sidebar", description: "Back to sidebar" },
    ]),
    ("Session Actions", &[
        Binding { key: KeyCode::Enter, modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal], action: || Action::FocusPaneNext, short: "Enter", label: "open", description: "Open detail pane" },
        Binding { key: KeyCode::Char('c'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal, KeyMode::Detail], action: || Action::OpenSession(ShellKind::Claude), short: "c", label: "claude", description: "Open Claude Code in worktree" },
        Binding { key: KeyCode::Char('b'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal], action: || Action::OpenSession(ShellKind::Shell), short: "b", label: "shell", description: "Open shell in worktree" },
        Binding { key: KeyCode::Char('o'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal, KeyMode::Detail], action: || Action::OpenInBrowser, short: "o", label: "open", description: "Open PR in browser" },
        Binding { key: KeyCode::Char('M'), modifiers: KeyModifiers::SHIFT, modes: &[KeyMode::Normal, KeyMode::Detail], action: || Action::MergePr, short: "M", label: "merge", description: "Merge PR (requires double-press)" },
        Binding { key: KeyCode::Char('w'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal, KeyMode::Detail], action: || Action::ToggleMonitor, short: "w", label: "watch", description: "Toggle automatic monitor (CI fix + rebase)" },
        Binding { key: KeyCode::Char('N'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal], action: || Action::NewSession, short: "N", label: "new", description: "Create new standalone session" },
        Binding { key: KeyCode::Char('z'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal], action: || Action::Snooze, short: "z", label: "snooze", description: "Snooze session for 4 hours" },
    ]),
    ("PR Actions", &[
        Binding { key: KeyCode::Char('R'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal, KeyMode::Detail], action: || Action::EditReviewers, short: "R", label: "reviewers", description: "Edit PR reviewers" },
        Binding { key: KeyCode::Char('A'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal, KeyMode::Detail], action: || Action::EditAssignees, short: "A", label: "assignees", description: "Edit PR assignees" },
        Binding { key: KeyCode::Char('S'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal, KeyMode::Detail], action: || Action::SlackNudge, short: "S", label: "slack", description: "Send Slack reminder to reviewers" },
    ]),
    ("Comments", &[
        Binding { key: KeyCode::Char(' '), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Detail], action: || Action::ToggleCommentSelect, short: "Spc", label: "select", description: "Select comment / mark as read" },
        Binding { key: KeyCode::Enter, modifiers: KeyModifiers::NONE, modes: &[KeyMode::Detail], action: || Action::ToggleCommentSelect, short: "Enter", label: "select", description: "Select/deselect comment for batch action" },
        Binding { key: KeyCode::Char('f'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal, KeyMode::Detail], action: || Action::FixWithClaude, short: "f", label: "fix", description: "Send selected to Claude for fix" },
        Binding { key: KeyCode::Char('r'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Detail], action: || Action::ReplyWithClaude, short: "r", label: "reply", description: "Send selected to Claude for reply" },
        Binding { key: KeyCode::Char('e'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal, KeyMode::Detail], action: || Action::QuickReply, short: "e", label: "reply", description: "Quick reply (post comment directly)" },
    ]),
    ("Sidebar", &[
        Binding { key: KeyCode::Char('m'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal], action: || Action::MarkRead, short: "m", label: "read", description: "Mark session as read" },
        Binding { key: KeyCode::Left, modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal], action: || Action::CollapseSelected, short: "←", label: "collapse", description: "Collapse repo/session" },
        Binding { key: KeyCode::Right, modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal], action: || Action::ExpandSelected, short: "→", label: "expand", description: "Expand repo/session" },
        Binding { key: KeyCode::Char('t'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal], action: || Action::CycleTimeFilter, short: "t", label: "time", description: "Cycle time filter (1d/3d/7d/30d/all)" },
        Binding { key: KeyCode::Char('/'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal], action: || Action::SearchActivate, short: "/", label: "search", description: "Search/filter sessions" },
        Binding { key: KeyCode::Char('g'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal], action: || Action::Refresh, short: "g", label: "refresh", description: "Refresh — fetch latest from GitHub" },
    ]),
    ("Tabs", &[
        Binding { key: KeyCode::Char('n'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal], action: || Action::NextTab, short: "n", label: "next tab", description: "Next tab" },
        Binding { key: KeyCode::Char('p'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal], action: || Action::PrevTab, short: "p", label: "prev tab", description: "Previous tab" },
        Binding { key: KeyCode::Char('x'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal], action: || Action::CloseTab, short: "x", label: "close tab", description: "Close active tab" },
    ]),
    ("Terminal", &[
        Binding { key: KeyCode::Char(']'), modifiers: KeyModifiers::CONTROL, modes: &[KeyMode::Terminal], action: || Action::FocusPaneNext, short: "^]/^o", label: "exit", description: "Exit terminal mode (Ctrl-] or Ctrl-o)" },
        Binding { key: KeyCode::Char('w'), modifiers: KeyModifiers::CONTROL, modes: &[KeyMode::Terminal, KeyMode::Normal, KeyMode::Detail], action: || Action::WaitingPrefix, short: "^w", label: "panes", description: "Pane operations prefix" },
    ]),
    ("Global", &[
        Binding { key: KeyCode::Char('q'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal, KeyMode::Detail], action: || Action::Quit, short: "q", label: "quit", description: "Quit pilot" },
        Binding { key: KeyCode::Char('?'), modifiers: KeyModifiers::NONE, modes: &[KeyMode::Normal, KeyMode::Detail], action: || Action::ToggleHelp, short: "?", label: "help", description: "Show all keybindings" },
    ]),
];

/// Static hints for modes without context.
pub fn action_bar_for_mode(mode: KeyMode) -> Vec<(&'static str, &'static str)> {
    match mode {
        KeyMode::Normal | KeyMode::Detail => vec![("?", "help")],
        KeyMode::Terminal => vec![("Tab", "exit"), ("?", "help")],
        KeyMode::PanePrefix => vec![("h/j/k/l", "focus"), ("v/s", "split"), ("z", "zoom")],
    }
}

/// Contextual action hints based on the current PR state.
/// Shows only what's relevant RIGHT NOW.
pub fn contextual_hints(
    mode: KeyMode,
    ci: pilot_core::CiStatus,
    _review: pilot_core::ReviewStatus,
    role: pilot_core::TaskRole,
    has_conflicts: bool,
    has_unread: bool,
    is_ready: bool,
    has_terminal: bool,
    needs_reply: bool,
    in_merge_queue: bool,
    has_reviewers: bool,
) -> Vec<(&'static str, &'static str)> {
    let mut hints = Vec::with_capacity(8);
    let is_author = matches!(role, pilot_core::TaskRole::Author);

    match mode {
        KeyMode::Normal => {
            // Always: navigation basics.
            hints.push(("j/k", "navigate"));
            hints.push(("Enter", "detail"));

            // Contextual actions based on PR state.
            if is_ready && is_author && !in_merge_queue {
                hints.push(("M", "MERGE"));
            }
            if has_conflicts && is_author {
                hints.push(("f", "REBASE"));
            } else if ci == pilot_core::CiStatus::Failure && is_author {
                hints.push(("f", "FIX CI"));
            } else if needs_reply && is_author {
                hints.push(("e", "REPLY"));
            }
            if matches!(role, pilot_core::TaskRole::Reviewer) {
                hints.push(("o", "review"));
            }
            if is_author && !has_reviewers {
                hints.push(("A", "assign"));
            }
            if !has_terminal {
                hints.push(("c", "claude"));
            }
            hints.push(("o", "browser"));
            hints.push(("?", "help"));
        }
        KeyMode::Detail => {
            hints.push(("j/k", "comments"));

            // Contextual for detail.
            if has_unread {
                hints.push(("Space", "read"));
            }
            if ci == pilot_core::CiStatus::Failure || has_conflicts {
                hints.push(("f", "FIX"));
            }
            if needs_reply {
                hints.push(("e", "REPLY"));
            }
            if is_ready && is_author {
                hints.push(("M", "MERGE"));
            }
            hints.push(("R", "reviewers"));
            hints.push(("A", "assignees"));
            hints.push(("Esc", "back"));
            hints.push(("?", "help"));
        }
        KeyMode::Terminal => {
            hints.push(("Tab", "exit terminal"));
        }
        KeyMode::PanePrefix => {
            hints.push(("h/j/k/l", "focus"));
            hints.push(("v/s", "split"));
            hints.push(("z", "zoom"));
        }
    }

    hints
}

/// Get all bindings for the help page.
pub fn all_bindings() -> Vec<(&'static str, Vec<(&'static str, &'static str, &'static str)>)> {
    BINDINGS
        .iter()
        .map(|(category, bindings)| {
            let items: Vec<_> = bindings
                .iter()
                .map(|b| {
                    let modes: String = b.modes.iter().map(|m| match m {
                        KeyMode::Normal => "N",
                        KeyMode::Detail => "D",
                        KeyMode::Terminal => "T",
                        KeyMode::PanePrefix => "P",
                    }).collect::<Vec<_>>().join("");
                    (b.short, b.description, modes.leak() as &'static str)
                })
                .collect();
            (*category, items)
        })
        .collect()
}
