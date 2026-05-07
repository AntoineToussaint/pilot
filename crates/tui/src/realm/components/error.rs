//! `ErrorModal` — diagnostic with a colored severity pill. tuirealm
//! port of `tui_kit::widgets::ErrorModal`.
//!
//! Any key dismisses (Esc, Enter, Space, Ctrl-C). Returns
//! `Msg::ModalDismissed`.

use crate::realm::Msg;
use ratatui::style::Color;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::Event;
use crate::realm::UserEvent;
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use tuirealm::state::State;

/// Severity tag for the modal — short label + color.
#[derive(Debug, Clone)]
pub struct Accent {
    /// Short word shown inside the pill.
    pub label: String,
    /// Pill background + border tint.
    pub color: Color,
}

impl Accent {
    /// Construct any accent.
    pub fn new(label: impl Into<String>, color: Color) -> Self {
        Self {
            label: label.into(),
            color,
        }
    }

    /// Transient hiccup — uses `theme.warn`.
    pub fn warn(label: impl Into<String>) -> Self {
        Self::new(label, crate::theme::current().warn)
    }

    /// Hard failure — uses `theme.error`.
    pub fn error(label: impl Into<String>) -> Self {
        Self::new(label, crate::theme::current().error)
    }

    /// Heads-up but not blocking — uses `theme.accent`.
    pub fn info(label: impl Into<String>) -> Self {
        Self::new(label, crate::theme::current().accent)
    }
}

/// Diagnostic modal.
pub struct ErrorModal {
    title: String,
    source: String,
    accent: Accent,
    detail: String,
}

impl ErrorModal {
    /// Build a modal showing `detail` from `source` with severity
    /// `accent`. Title defaults to "Error".
    pub fn new(
        source: impl Into<String>,
        accent: Accent,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            title: "Error".to_string(),
            source: source.into(),
            accent,
            detail: detail.into(),
        }
    }

    /// Override the title.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }
}

impl Component for ErrorModal {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 90u16.min(area.width.saturating_sub(4));
        let modal_h = 22u16.min(area.height.saturating_sub(2));
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(self.accent.color));
        let inner = block.inner(modal);
        frame.render_widget(block, modal);

        const POWERLINE_RIGHT: &str = "\u{e0b0}";
        let header = Line::from(vec![
            Span::styled(
                format!(" {} ", self.accent.label),
                Style::default()
                    .bg(self.accent.color)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(POWERLINE_RIGHT, Style::default().fg(self.accent.color)),
            Span::raw(" "),
            Span::styled(
                self.source.clone(),
                Style::default()
                    .fg(theme.text_strong)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);

        let mut lines = vec![header, Line::raw("")];
        for raw in self.detail.lines() {
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(theme.text_dim),
            )));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![Span::styled(
            "Press any key to dismiss",
            theme.hint(),
        )]));
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

impl AppComponent<Msg, UserEvent> for ErrorModal {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        // Any keyboard event dismisses.
        if matches!(ev, Event::Keyboard(_)) {
            Some(Msg::ModalDismissed)
        } else {
            None
        }
    }
}
