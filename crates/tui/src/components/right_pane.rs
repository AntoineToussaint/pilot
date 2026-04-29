//! RightPane — the right column of the TUI. Shows the session
//! currently selected in the Sidebar: header (title, branch, CI,
//! reviewers) on top, comment list below.
//!
//! ## Why one component instead of Header + Comments
//!
//! Header and Comments share one state source (the current session)
//! and one focus (there's no "tab between header and comments"). Two
//! components with a shared source would just recreate v1's
//! pane-tree / selection desync surface. The render is split into
//! sub-functions for readability; the *component* stays one unit.
//!
//! Dashboard tiles (task #70) are a separate case — they each have
//! an independent data source and can make sense as individual
//! components in a TileStack container.
//!
//! ## Data flow
//!
//! AppRoot reads `sidebar.selected_workspace()` after every key event
//! and calls `right_pane.set_workspace(...)`. The RightPane doesn't
//! track every workspace the daemon knows about — only the currently
//! selected one. This keeps the component simple and its event
//! handler a no-op.

use crate::{Component, ComponentId, Outcome};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pilot_core::{ActivityKind, Workspace};
use pilot_v2_ipc::{Command, Event};
use ratatui::Frame;
use ratatui::prelude::*;
use ratatui::widgets::*;

pub struct RightPane {
    id: ComponentId,
    workspace: Option<Workspace>,
    /// Scroll offset into the comment list (top-of-viewport index).
    comment_scroll: usize,
    /// Highlighted comment index, for `Space`-to-select UX later.
    comment_cursor: usize,
}

impl RightPane {
    pub fn new(id: ComponentId) -> Self {
        Self {
            id,
            workspace: None,
            comment_scroll: 0,
            comment_cursor: 0,
        }
    }

    /// AppRoot calls this whenever the Sidebar's selection changes.
    /// Resets the comment cursor because what was "row 3" on the
    /// previous workspace is meaningless on the new one.
    pub fn set_workspace(&mut self, workspace: Option<Workspace>) {
        let same = match (&self.workspace, &workspace) {
            (Some(a), Some(b)) => a.key == b.key,
            _ => false,
        };
        self.workspace = workspace;
        if !same {
            self.comment_scroll = 0;
            self.comment_cursor = 0;
        }
    }

    pub fn selected_workspace(&self) -> Option<&Workspace> {
        self.workspace.as_ref()
    }

    pub fn comment_cursor(&self) -> usize {
        self.comment_cursor
    }

    fn render_header(&self, area: Rect, frame: &mut Frame) {
        let Some(workspace) = &self.workspace else {
            let placeholder = Paragraph::new(Line::from(Span::styled(
                " (no session selected) ",
                Style::default().fg(Color::DarkGray).italic(),
            )));
            frame.render_widget(placeholder, area);
            return;
        };
        let Some(task) = workspace.primary_task() else {
            // Workspace exists but no task attached yet (created from
            // scratch). Show a minimal header so the user knows where
            // they are and what branch the next agent will spawn into.
            let lines = vec![Line::from(vec![
                Span::styled(
                    " EMPTY ",
                    Style::default().bg(Color::DarkGray).fg(Color::Black).bold(),
                ),
                Span::raw(" "),
                Span::styled(&workspace.name, Style::default().bold()),
            ])];
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
            return;
        };

        let mut lines: Vec<Line> = Vec::new();

        // Line 1: state tag + title
        let state_tag = match task.state {
            pilot_core::TaskState::Open => ("OPEN", Color::Green),
            pilot_core::TaskState::Draft => ("DRAFT", Color::DarkGray),
            pilot_core::TaskState::Merged => ("MERGED", Color::Magenta),
            pilot_core::TaskState::Closed => ("CLOSED", Color::Red),
            pilot_core::TaskState::InProgress => ("WIP", Color::Yellow),
            pilot_core::TaskState::InReview => ("REVIEW", Color::Yellow),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} ", state_tag.0),
                Style::default().bg(state_tag.1).fg(Color::Black).bold(),
            ),
            Span::raw(" "),
            Span::styled(&task.title, Style::default().bold()),
        ]));

        // Line 2: branch only. Role / CI / Review / Reviewers used to
        // sit here; they're either redundant with the sidebar tag
        // (state) or low-signal in the inbox flow. The branch is
        // genuinely useful — confirms which worktree pilot will spawn
        // an agent into.
        let branch = task.branch.as_deref().unwrap_or("-");
        lines.push(Line::from(vec![
            Span::raw("Branch: "),
            Span::styled(branch, Style::default().fg(Color::Cyan)),
        ]));

        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(para, area);
    }

    fn render_comments(&self, area: Rect, frame: &mut Frame, focused: bool) {
        let border_color = if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        };
        let Some(workspace) = &self.workspace else {
            let block = Block::bordered()
                .border_style(Style::default().fg(border_color))
                .title(" Activity ");
            frame.render_widget(block, area);
            return;
        };

        let count = workspace.activity.len();
        let title = format!(" Activity ({count}) ");
        let block = Block::bordered()
            .border_style(Style::default().fg(border_color))
            .title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if workspace.activity.is_empty() {
            let empty = Paragraph::new(Line::from(Span::styled(
                "  (no activity)",
                Style::default().fg(Color::DarkGray).italic(),
            )));
            frame.render_widget(empty, inner);
            return;
        }

        // Render a window of activities starting at comment_scroll.
        let visible = workspace
            .activity
            .iter()
            .enumerate()
            .skip(self.comment_scroll)
            .take(inner.height as usize)
            .map(|(i, activity)| {
                let is_cursor = i == self.comment_cursor;
                let is_unread = workspace.is_activity_unread(i);
                let kind_marker = match activity.kind {
                    ActivityKind::Comment => "[c]",
                    ActivityKind::Review => "[r]",
                    ActivityKind::StatusChange => "[s]",
                    ActivityKind::CiUpdate => "[C]",
                };
                let unread_marker = if is_unread { "*" } else { " " };
                let row_style = if is_cursor && focused {
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold()
                } else if is_cursor {
                    Style::default().fg(Color::White)
                } else {
                    Style::default()
                };
                // Single-line summary: [kind] author: body first line
                let body_first = activity.body.lines().next().unwrap_or("");
                let summary = format!(
                    "{unread_marker}{kind_marker} {}: {body_first}",
                    activity.author
                );
                Line::from(Span::styled(summary, row_style))
            })
            .collect::<Vec<_>>();

        let para = Paragraph::new(visible);
        frame.render_widget(para, inner);
    }
}

impl Component for RightPane {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn handle_key(&mut self, key: KeyEvent, _cmds: &mut Vec<Command>) -> Outcome {
        // Comment navigation. Requires a workspace, otherwise there's
        // nothing to scroll through.
        let Some(workspace) = &self.workspace else {
            return Outcome::BubbleUp;
        };
        let last = workspace.activity.len().saturating_sub(1);

        match (key.code, key.modifiers) {
            (KeyCode::Char('j'), KeyModifiers::NONE) | (KeyCode::Down, _) => {
                if workspace.activity.is_empty() {
                    return Outcome::Consumed;
                }
                if self.comment_cursor < last {
                    self.comment_cursor += 1;
                }
                Outcome::Consumed
            }
            (KeyCode::Char('k'), KeyModifiers::NONE) | (KeyCode::Up, _) => {
                self.comment_cursor = self.comment_cursor.saturating_sub(1);
                Outcome::Consumed
            }
            (KeyCode::Char('g'), KeyModifiers::NONE) => {
                self.comment_cursor = 0;
                self.comment_scroll = 0;
                Outcome::Consumed
            }
            (KeyCode::Char('G'), m) if m.contains(KeyModifiers::SHIFT) => {
                self.comment_cursor = last;
                Outcome::Consumed
            }
            _ => Outcome::BubbleUp,
        }
    }

    fn on_event(&mut self, event: &Event) {
        // When the currently-selected workspace is upserted, refresh
        // our local copy so comment-cursor offsets stay in range.
        let Event::WorkspaceUpserted(workspace) = event else {
            return;
        };
        let Some(current) = self.workspace.as_ref() else {
            return;
        };
        if current.key == workspace.key {
            let last = workspace.activity.len().saturating_sub(1);
            self.workspace = Some((**workspace).clone());
            self.comment_cursor = self.comment_cursor.min(last);
            if self.comment_scroll > last {
                self.comment_scroll = last;
            }
        }
    }

    fn render(&mut self, area: Rect, frame: &mut Frame, focused: bool) {
        // Split vertically: 4 rows for header, rest for comments.
        let chunks = Layout::vertical([
            Constraint::Length(4),
            Constraint::Length(1), // separator line
            Constraint::Min(0),
        ])
        .split(area);

        self.render_header(chunks[0], frame);

        // Thin separator.
        let sep = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray));
        frame.render_widget(sep, chunks[1]);

        self.render_comments(chunks[2], frame, focused);
    }
}
