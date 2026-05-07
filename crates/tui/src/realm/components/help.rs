//! `Help` — yazi-style which-key panel pinned to the bottom. tuirealm
//! port of `tui_kit::widgets::HelpModal`.
//!
//! Any keyboard event dismisses.

use crate::pane::Binding;
use crate::realm::Msg;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::Event;
use crate::realm::UserEvent;
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::{Constraint, Layout, Rect};
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, Clear, Paragraph};
use tuirealm::state::State;

/// One section of the help panel — title + bindings under it.
pub struct HelpSection {
    /// Section title (rendered today flatten into one grid; reserved
    /// for future per-section styling).
    pub title: &'static str,
    /// Bindings for this section.
    pub bindings: &'static [Binding],
}

/// Yazi-style which-key panel.
pub struct Help {
    sections: Vec<HelpSection>,
}

impl Help {
    /// Build from a list of sections.
    pub fn new(sections: Vec<HelpSection>) -> Self {
        Self { sections }
    }

    fn flat(&self) -> Vec<&Binding> {
        self.sections
            .iter()
            .flat_map(|s| s.bindings.iter())
            .collect()
    }
}

const COLS: usize = 3;
const PADDING_Y: u16 = 1;
const PADDING_X: u16 = 1;

impl Component for Help {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let bindings = self.flat();
        if bindings.is_empty() {
            return;
        }
        let rows = bindings.len().div_ceil(COLS) as u16;
        let panel_h = (rows + PADDING_Y * 2).min(area.height);
        let panel = Rect {
            x: area.x.saturating_add(PADDING_X.min(area.width)),
            y: area
                .y
                .saturating_add(area.height.saturating_sub(panel_h + 1)),
            width: area.width.saturating_sub(PADDING_X * 2),
            height: panel_h,
        };

        if panel.y > area.y {
            let mask = Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: panel.y - area.y,
            };
            frame.render_widget(
                Block::default().style(Style::default().bg(Color::Black)),
                mask,
            );
        }

        frame.render_widget(Clear, panel);
        let panel_bg = Style::default().bg(theme.surface);
        frame.render_widget(Block::default().style(panel_bg), panel);

        let col_constraints: Vec<Constraint> = (0..COLS)
            .map(|_| Constraint::Ratio(1, COLS as u32))
            .collect();
        let cols = Layout::horizontal(col_constraints).split(panel);

        for (idx, b) in bindings.iter().enumerate() {
            let col_idx = idx % COLS;
            let row_idx = (idx / COLS) as u16;
            let col = cols[col_idx];
            let cell = Rect {
                x: col.x,
                y: col.y + PADDING_Y + row_idx,
                width: col.width,
                height: 1,
            };
            if cell.y >= panel.y + panel.height {
                break;
            }

            const KEY_PAD: usize = 14;
            let mut key = b.keys.to_string();
            if key.chars().count() < KEY_PAD {
                key.push_str(&" ".repeat(KEY_PAD - key.chars().count()));
            }

            let key_style = Style::default()
                .bg(theme.surface)
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD);
            let sep_style = Style::default()
                .bg(theme.surface)
                .fg(theme.text_dim);
            let label_style = Style::default()
                .bg(theme.surface)
                .fg(theme.text_strong);
            let line = Line::from(vec![
                Span::styled(" ", panel_bg),
                Span::styled(key, key_style),
                Span::styled("  ", sep_style),
                Span::styled(b.label, label_style),
            ]);
            frame.render_widget(Paragraph::new(line), cell);
        }
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

impl AppComponent<Msg, UserEvent> for Help {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        if matches!(ev, Event::Keyboard(_)) {
            Some(Msg::ModalDismissed)
        } else {
            None
        }
    }
}
