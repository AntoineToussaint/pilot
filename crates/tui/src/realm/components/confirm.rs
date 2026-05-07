//! `Confirm` — yes/no prompt. tuirealm port of
//! `tui_kit::widgets::ConfirmModal`.
//!
//! Returns `Msg::Confirmed(true)` on Y/Enter, `Msg::Confirmed(false)`
//! on N. Esc maps to `Msg::ModalDismissed`. Unlike the tui-kit
//! version, the boolean lives inside `Msg` rather than being passed
//! via a generic `Done(Box<Any>)` payload — that's the whole point of
//! tuirealm's typed Msg approach.

use crate::realm::Msg;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyEvent, KeyModifiers};
use crate::realm::UserEvent;
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use tuirealm::state::State;

/// Y/N confirmation prompt.
pub struct Confirm {
    question: String,
    /// Whether `Enter` defaults to "yes". Drives which option renders
    /// in bold accent.
    default_yes: bool,
}

impl Confirm {
    /// Build a prompt asking `question`. Defaults `Enter` to yes.
    pub fn new(question: impl Into<String>) -> Self {
        Self {
            question: question.into(),
            default_yes: true,
        }
    }

    /// Make `Enter` default to "no". Use for destructive prompts where
    /// the safer option is to back out.
    pub fn default_no(mut self) -> Self {
        self.default_yes = false;
        self
    }
}

impl Component for Confirm {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 60u16.min(area.width.saturating_sub(4));
        let modal_h = 6u16;
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(" Confirm ", theme.modal_title()))
            .border_style(theme.modal_border());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);

        let yes_style = if self.default_yes {
            Style::default()
                .fg(theme.success)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_dim)
        };
        let no_style = if self.default_yes {
            Style::default().fg(theme.text_dim)
        } else {
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD)
        };

        let lines = vec![
            Line::raw(""),
            Line::from(Span::raw(self.question.clone())),
            Line::raw(""),
            Line::from(vec![
                Span::styled("[Y]es", yes_style),
                Span::raw("    "),
                Span::styled("[N]o", no_style),
                Span::raw("    "),
                Span::styled("Esc cancel", theme.hint()),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }),
            inner,
        );
    }

    fn query(&self, _: Attribute) -> Option<QueryResult<'_>> {
        None
    }
    fn attr(&mut self, _: Attribute, _: AttrValue) {}
    fn state(&self) -> State {
        State::None
    }
    fn perform(&mut self, _: Cmd) -> CmdResult {
        CmdResult::NoChange
    }
}

impl AppComponent<Msg, UserEvent> for Confirm {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            Event::Keyboard(KeyEvent {
                code: Key::Esc, ..
            }) => Some(Msg::ModalDismissed),
            Event::Keyboard(KeyEvent {
                code: Key::Char('c'),
                modifiers,
                ..
            }) if modifiers.contains(KeyModifiers::CONTROL) => Some(Msg::ModalDismissed),
            Event::Keyboard(KeyEvent {
                code: Key::Char('y') | Key::Char('Y'),
                ..
            }) => Some(Msg::Confirmed(true)),
            Event::Keyboard(KeyEvent {
                code: Key::Char('n') | Key::Char('N'),
                ..
            }) => Some(Msg::Confirmed(false)),
            Event::Keyboard(KeyEvent {
                code: Key::Enter, ..
            }) => Some(Msg::Confirmed(self.default_yes)),
            _ => None,
        }
    }
}
