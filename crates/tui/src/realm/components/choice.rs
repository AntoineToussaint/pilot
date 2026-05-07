//! `Choice<T>` — single- or multi-select picker. tuirealm port of
//! `tui_kit::widgets::ChoiceModal`.
//!
//! The big API change vs tui-kit: the picked-`Vec<T>` doesn't fit
//! cleanly inside `Msg` (which must be `PartialEq + Clone`). So this
//! port reports the picked **indices** as `Msg::ChoicePicked(Vec<usize>)`;
//! the calling flow already owns the source `Vec<T>` and indexes back
//! into it.
//!
//! Modes:
//! - `Choice::single(prompt, items)` — Enter picks one, returns
//!   `ChoicePicked(vec![i])`.
//! - `Choice::multi(prompt, items)` — Space toggles, Enter confirms,
//!   returns `ChoicePicked(vec![i, j, ...])`.
//!
//! `with_back(true)` enables Backspace → `Msg::ChoiceBack`.
//! `with_refresh(true)` enables `r` → `Msg::ChoiceRefresh`.

use crate::realm::Msg;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyModifiers};
use crate::realm::UserEvent;
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use tuirealm::state::State;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Single,
    Multi,
}

type LabelFn<T> = Box<dyn Fn(&T) -> String + Send>;
type SectionFn<T> = Box<dyn Fn(&T) -> &'static str + Send>;
type SelectableFn<T> = Box<dyn Fn(&T) -> bool + Send>;

/// Single- or multi-select picker.
pub struct Choice<T: Clone + 'static + Send> {
    title: String,
    prompt: String,
    items: Vec<T>,
    selected: Vec<bool>,
    cursor: usize,
    mode: Mode,
    label_for: LabelFn<T>,
    can_back: bool,
    section_for: Option<SectionFn<T>>,
    selectable: Option<SelectableFn<T>>,
    can_refresh: bool,
    require_one: bool,
    show_empty_hint: bool,
}

impl<T: Clone + 'static + Send> Choice<T> {
    /// Single-pick mode.
    pub fn single(prompt: impl Into<String>, items: Vec<T>) -> Self {
        let len = items.len();
        Self {
            title: "Pick one".into(),
            prompt: prompt.into(),
            items,
            selected: vec![false; len],
            cursor: 0,
            mode: Mode::Single,
            label_for: Box::new(|_| String::new()),
            can_back: false,
            section_for: None,
            selectable: None,
            can_refresh: false,
            require_one: true,
            show_empty_hint: false,
        }
    }

    /// Multi-pick mode.
    pub fn multi(prompt: impl Into<String>, items: Vec<T>) -> Self {
        let len = items.len();
        Self {
            title: "Pick any".into(),
            prompt: prompt.into(),
            items,
            selected: vec![false; len],
            cursor: 0,
            mode: Mode::Multi,
            label_for: Box::new(|_| String::new()),
            can_back: false,
            section_for: None,
            selectable: None,
            can_refresh: false,
            require_one: true,
            show_empty_hint: false,
        }
    }

    /// Override modal title.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Display formatter for each item.
    pub fn label<F>(mut self, f: F) -> Self
    where
        F: Fn(&T) -> String + Send + 'static,
    {
        self.label_for = Box::new(f);
        self
    }

    /// Enable Backspace → `Msg::ChoiceBack`.
    pub fn with_back(mut self, enabled: bool) -> Self {
        self.can_back = enabled;
        self
    }

    /// Optional section grouper.
    pub fn section_for<F>(mut self, f: F) -> Self
    where
        F: Fn(&T) -> &'static str + Send + 'static,
    {
        self.section_for = Some(Box::new(f));
        self
    }

    /// Optional selectability predicate.
    pub fn selectable<F>(mut self, f: F) -> Self
    where
        F: Fn(&T) -> bool + Send + 'static,
    {
        self.selectable = Some(Box::new(f));
        self
    }

    /// Enable `r` → `Msg::ChoiceRefresh`.
    pub fn with_refresh(mut self, enabled: bool) -> Self {
        self.can_refresh = enabled;
        self
    }

    /// Multi-select: allow Enter on empty selection.
    pub fn allow_empty(mut self, allowed: bool) -> Self {
        self.require_one = !allowed;
        self
    }

    /// Pre-tick items matching the predicate. Used by setup steps that
    /// remount the same picker after a refresh / back navigation —
    /// keeps the user's prior selection visible.
    pub fn with_selected_by<F>(mut self, f: F) -> Self
    where
        F: Fn(&T) -> bool,
    {
        for (i, item) in self.items.iter().enumerate() {
            self.selected[i] = f(item);
        }
        self
    }

    fn is_selectable(&self, idx: usize) -> bool {
        match self.items.get(idx) {
            None => false,
            Some(item) => self
                .selectable
                .as_ref()
                .map(|f| f(item))
                .unwrap_or(true),
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let last = self.items.len() as isize - 1;
        let cur = self.cursor as isize;
        self.cursor = (cur + delta).clamp(0, last) as usize;
    }

    fn confirm_picks(&mut self) -> ConfirmResult {
        let picked: Vec<usize> = match self.mode {
            Mode::Single => {
                if self.items.is_empty() {
                    return ConfirmResult::Cancel;
                }
                if !self.is_selectable(self.cursor) {
                    return ConfirmResult::Stay;
                }
                vec![self.cursor]
            }
            Mode::Multi => self
                .selected
                .iter()
                .enumerate()
                .filter(|(_, s)| **s)
                .map(|(i, _)| i)
                .collect(),
        };
        if self.mode == Mode::Multi && self.require_one && picked.is_empty() {
            self.show_empty_hint = true;
            return ConfirmResult::Stay;
        }
        ConfirmResult::Picked(picked)
    }

    fn build_lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let theme = crate::theme::current();
        let mut lines: Vec<Line> = Vec::with_capacity(self.items.len() + 4);
        // Prompt
        lines.push(Line::from(Span::styled(
            self.prompt.clone(),
            Style::default().fg(theme.text_dim),
        )));
        lines.push(Line::raw(""));

        // Section grouping — if a `section_for` exists, walk the
        // items printing the section header before the first item of
        // each group.
        let mut last_section: Option<&'static str> = None;
        for (i, item) in self.items.iter().enumerate() {
            if let Some(sec_fn) = self.section_for.as_ref() {
                let section = sec_fn(item);
                if !section.is_empty() && Some(section) != last_section {
                    if last_section.is_some() {
                        lines.push(Line::raw(""));
                    }
                    lines.push(Line::from(Span::styled(
                        section.to_string(),
                        Style::default()
                            .fg(theme.warn)
                            .add_modifier(Modifier::BOLD),
                    )));
                    last_section = Some(section);
                }
            }
            let is_cursor = i == self.cursor;
            let selectable = self.is_selectable(i);
            let selected = self.selected.get(i).copied().unwrap_or(false);
            let prefix = match (self.mode, selected, selectable) {
                (Mode::Multi, true, true) => "[x] ",
                (Mode::Multi, false, true) => "[ ] ",
                (Mode::Multi, _, false) => "[·] ",
                (Mode::Single, _, true) => "    ",
                (Mode::Single, _, false) => "    ",
            };
            let cursor_caret = if is_cursor { "▸ " } else { "  " };
            let mut style = if !selectable {
                Style::default().fg(theme.text_dim)
            } else if is_cursor {
                Style::default()
                    .fg(theme.text_strong)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text_strong)
            };
            if is_cursor {
                style = style.bg(theme.fill);
            }
            let label = (self.label_for)(item);
            let line = format!("{cursor_caret}{prefix}{label}");
            // Truncate to width.
            let truncated = if line.chars().count() > width as usize {
                let mut s: String = line.chars().take(width as usize - 1).collect();
                s.push('…');
                s
            } else {
                line
            };
            lines.push(Line::from(Span::styled(truncated, style)));
        }
        // Empty hint
        if self.show_empty_hint {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "  pick at least one (Space to toggle)",
                Style::default().fg(theme.error),
            )));
        }
        lines
    }
}

enum ConfirmResult {
    Stay,
    Cancel,
    Picked(Vec<usize>),
}

impl<T: Clone + 'static + Send> Component for Choice<T> {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 80u16.min(area.width.saturating_sub(4));
        let modal_h = 24u16.min(area.height.saturating_sub(4));
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(
                format!(" {} ", self.title),
                theme.modal_title(),
            ))
            .border_style(theme.modal_border());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);

        let lines = self.build_lines(inner.width);
        // Help footer
        let mut help_spans = vec![
            Span::styled("j/k", Style::default().fg(theme.accent).bold()),
            Span::raw(" navigate  "),
        ];
        if self.mode == Mode::Multi {
            help_spans.push(Span::styled(
                "Space",
                Style::default().fg(theme.accent).bold(),
            ));
            help_spans.push(Span::raw(" toggle  "));
        }
        help_spans.push(Span::styled(
            "Enter",
            Style::default().fg(theme.success).bold(),
        ));
        help_spans.push(Span::raw(" confirm  "));
        if self.can_refresh {
            help_spans.push(Span::styled("r", Style::default().fg(theme.warn).bold()));
            help_spans.push(Span::raw(" refresh  "));
        }
        if self.can_back {
            help_spans.push(Span::styled(
                "Backspace",
                Style::default().fg(theme.warn).bold(),
            ));
            help_spans.push(Span::raw(" back  "));
        }
        help_spans.push(Span::styled(
            "Esc",
            Style::default().fg(theme.error).bold(),
        ));
        help_spans.push(Span::raw(" cancel"));

        // Layout: lines occupy inner.height-2 rows; help at bottom
        let help_area = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };
        let body_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height - 2,
        };
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }),
            body_area,
        );
        frame.render_widget(Paragraph::new(Line::from(help_spans)), help_area);
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

impl<T: Clone + 'static + Send> AppComponent<Msg, UserEvent> for Choice<T> {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        let Event::Keyboard(key) = ev else {
            return None;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if matches!(key.code, Key::Esc) || (ctrl && matches!(key.code, Key::Char('c'))) {
            return Some(Msg::ModalDismissed);
        }
        match key.code {
            Key::Down | Key::Char('j') => {
                self.move_cursor(1);
                self.show_empty_hint = false;
                None
            }
            Key::Up | Key::Char('k') => {
                self.move_cursor(-1);
                self.show_empty_hint = false;
                None
            }
            Key::Char(' ') if self.mode == Mode::Multi => {
                if !self.items.is_empty() && self.is_selectable(self.cursor) {
                    self.selected[self.cursor] = !self.selected[self.cursor];
                }
                self.show_empty_hint = false;
                None
            }
            Key::Char('r') if self.can_refresh => Some(Msg::ChoiceRefresh),
            Key::Backspace if self.can_back => Some(Msg::ChoiceBack),
            Key::Enter => match self.confirm_picks() {
                ConfirmResult::Stay => None,
                ConfirmResult::Cancel => Some(Msg::ModalDismissed),
                ConfirmResult::Picked(picks) => Some(Msg::ChoicePicked(picks)),
            },
            _ => None,
        }
    }
}
