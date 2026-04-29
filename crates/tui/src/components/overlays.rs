//! Overlay components: modals that steal focus from the background
//! UI until dismissed. Each overlay is a normal `Component` mounted
//! dynamically in the tree; focus returns to its parent when it's
//! dismissed (`Outcome::Dismiss`).
//!
//! ## Overlays shipped today
//!
//! - `Help`: static content (keybind reference / icon legend). Any
//!   key dismisses. No user input, no validation.
//! - `NewWorktree`: prompts the user for a branch name. Esc cancels,
//!   Enter confirms — both dismiss. On confirmation AppRoot reads
//!   `confirmed_input()` via typed tree lookup and acts on it.
//!
//! ## The "can't get trapped in a dialog" invariant
//!
//! Both overlays accept `Esc` AND `Ctrl-C` as cancel. v1 had recurring
//! bugs where users got stuck inside a dialog; v2 tests lock the
//! dismiss paths down so that can never happen again.

use crate::{Component, ComponentId, Outcome};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pilot_v2_ipc::{Command, Event};
use ratatui::Frame;
use ratatui::prelude::*;
use ratatui::widgets::*;

// ── Help ───────────────────────────────────────────────────────────────

pub struct Help {
    id: ComponentId,
    /// Lines the overlay shows. Caller-configurable so the same
    /// component can render keymaps, icon legends, etc. without
    /// subclassing.
    pub lines: Vec<String>,
}

impl Help {
    pub fn new(id: ComponentId, lines: Vec<String>) -> Self {
        Self { id, lines }
    }

    /// Default keymap / icons content. The AppRoot supplies this via
    /// a render-time lookup in its own keymap registry later; for now
    /// this gives `?` a useful default.
    pub fn default_help(id: ComponentId) -> Self {
        Self::new(
            id,
            vec![
                "HELP — keys".into(),
                "".into(),
                "Sidebar".into(),
                "  j/k          navigate".into(),
                "  c            Claude Code".into(),
                "  b            shell".into(),
                "  m            mark read".into(),
                "  g            refresh".into(),
                "  z / Shift-Z  snooze 4h / archive".into(),
                "  Shift-S      Inbox <-> Snoozed".into(),
                "  Shift-X x2   delete".into(),
                "".into(),
                "Right pane".into(),
                "  j/k          scroll comments".into(),
                "  g / Shift-G  top / bottom".into(),
                "".into(),
                "Global".into(),
                "  ?            this help".into(),
                "  Tab          cycle panes".into(),
                "  N            new worktree".into(),
                "  q q          quit".into(),
                "".into(),
                "Press any key to close.".into(),
            ],
        )
    }
}

impl Component for Help {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn handle_key(&mut self, _key: KeyEvent, _cmds: &mut Vec<Command>) -> Outcome {
        // Any key dismisses. That's the contract: users press ? to
        // peek at the help and can close it with whatever finger is
        // nearest.
        Outcome::Dismiss
    }

    fn on_event(&mut self, _event: &Event) {}

    fn render(&mut self, area: Rect, frame: &mut Frame, _focused: bool) {
        // Centered modal: 60 cols wide, as tall as content + borders.
        let modal_w = 60u16.min(area.width.saturating_sub(4));
        let modal_h = (self.lines.len() as u16 + 2).min(area.height.saturating_sub(2));
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::bordered()
            .title(" Help ")
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(modal);
        frame.render_widget(block, modal);

        let text: Vec<Line> = self
            .lines
            .iter()
            .map(|l| Line::from(Span::raw(l.clone())))
            .collect();
        let para = Paragraph::new(text).wrap(Wrap { trim: false });
        frame.render_widget(para, inner);
    }
}

// ── NewWorktree ────────────────────────────────────────────────────────

/// Result the AppRoot reads back from a NewWorktree overlay after it
/// dismisses. `None` means "canceled"; `Some(branch)` means "go".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewWorktreeResult {
    Pending,
    Canceled,
    Confirmed(String),
}

pub struct NewWorktree {
    id: ComponentId,
    prompt: String,
    input: String,
    result: NewWorktreeResult,
}

impl NewWorktree {
    pub fn new(id: ComponentId, prompt: impl Into<String>) -> Self {
        Self {
            id,
            prompt: prompt.into(),
            input: String::new(),
            result: NewWorktreeResult::Pending,
        }
    }

    /// Pre-fill the input. AppRoot uses this to populate the overlay
    /// with e.g. a slugified branch name from a stuck session the
    /// user is retrying.
    pub fn with_input(mut self, text: impl Into<String>) -> Self {
        self.input = text.into();
        self
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn result(&self) -> &NewWorktreeResult {
        &self.result
    }

    /// Validate a branch name. Empty is a soft no-op (Enter does
    /// nothing until the user types). Otherwise reject names git
    /// would reject: spaces, `..`, bad characters, leading `-`.
    /// Mirrors v1's `sanitize_branch_name` rules.
    fn valid(name: &str) -> bool {
        if name.is_empty() {
            return false;
        }
        if name.starts_with('-') || name.contains("..") || name.contains("@{") {
            return false;
        }
        const BAD_CHARS: &[char] = &['~', '^', ':', '?', '*', '[', '\\', ' '];
        if name.chars().any(|c| BAD_CHARS.contains(&c)) {
            return false;
        }
        true
    }
}

impl Component for NewWorktree {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn handle_key(&mut self, key: KeyEvent, _cmds: &mut Vec<Command>) -> Outcome {
        // Universal dismiss: Esc or Ctrl-C. The "can't get trapped"
        // invariant — these always work, regardless of input state.
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.result = NewWorktreeResult::Canceled;
            return Outcome::Dismiss;
        }

        match (key.code, key.modifiers) {
            (KeyCode::Enter, _) => {
                if Self::valid(&self.input) {
                    self.result = NewWorktreeResult::Confirmed(self.input.clone());
                    Outcome::Dismiss
                } else {
                    // Empty or invalid — stay up, let the user fix it.
                    Outcome::Consumed
                }
            }
            (KeyCode::Backspace, _) => {
                self.input.pop();
                Outcome::Consumed
            }
            (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                self.input.push(c);
                Outcome::Consumed
            }
            _ => Outcome::Consumed,
        }
    }

    fn on_event(&mut self, _event: &Event) {}

    fn render(&mut self, area: Rect, frame: &mut Frame, _focused: bool) {
        let modal_w = 60u16.min(area.width.saturating_sub(4));
        let modal_h = 7u16;
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let title = " New worktree ";
        let block = Block::bordered()
            .title(title)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(modal);
        frame.render_widget(block, modal);

        let valid = Self::valid(&self.input);
        let prompt = Line::from(Span::styled(
            &*self.prompt,
            Style::default().fg(Color::DarkGray),
        ));
        let display = if self.input.is_empty() {
            "▌ type branch name".to_string()
        } else {
            format!("{}▌", self.input)
        };
        let input_line = Line::from(Span::styled(
            display,
            if self.input.is_empty() {
                Style::default().fg(Color::DarkGray).italic()
            } else if valid {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::Red)
            },
        ));
        let help = Line::from(vec![
            Span::styled("Enter", Style::default().fg(Color::Green).bold()),
            Span::raw(" create  "),
            Span::styled("Esc", Style::default().fg(Color::Red).bold()),
            Span::raw(" cancel"),
        ]);
        let para = Paragraph::new(vec![prompt, Line::raw(""), input_line, Line::raw(""), help])
            .wrap(Wrap { trim: false });
        frame.render_widget(para, inner);
    }
}
