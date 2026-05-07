//! RightPane — the right column of the TUI. Shows the session
//! currently selected in the Sidebar: header (title, branch, CI,
//! reviewers) on top, comment list below.
//!
//! ## Why one component instead of Header + Comments
//!
//! Header and Comments share one state source (the current session)
//! and one focus (there's no "tab between header and comments"). Two
//! components with a shared source would open a desync surface
//! between them. The render is split into sub-functions for
//! readability; the *component* stays one unit.
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

use crate::{PaneId, PaneOutcome};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pilot_core::{ActivityKind, Workspace};
use pilot_ipc::{Command, Event};
use ratatui::Frame;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::collections::HashSet;

/// Which collapsible section inside the right pane is currently
/// "selected" — i.e., what `Enter`/`Space` will toggle.
///
/// The header row at the top isn't selectable; the user only ever
/// targets the activity feed today. Encoded as an enum so we can grow
/// it (e.g., a separate `Reviewers` or `CI` section) without rewiring
/// every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RightSection {
    Activity,
}

pub struct RightPane {
    id: PaneId,
    workspace: Option<Workspace>,
    /// Scroll offset into the comment list (top-of-viewport index).
    comment_scroll: usize,
    /// Highlighted comment index, for `Space`-to-select UX later.
    comment_cursor: usize,
    /// Whether the activity section is collapsed to its header row.
    /// Defaults to expanded; auto-collapses when the workspace has no
    /// activity (the empty pane is just visual noise — keeping the
    /// header alone tells the user where it would land).
    activity_collapsed: bool,
    /// User-driven override of the collapse state. Once the user
    /// explicitly toggles, we stop auto-collapsing on empty: their
    /// intent wins.
    activity_collapse_user_set: bool,
    /// Auto-mark-read timer. When the cursor lands on an unread row
    /// while the pane has focus, we record the moment in
    /// `mark_armed_at`. On the next `tick` past `MARK_READ_DELAY` we
    /// flip that activity to read and remember the index in
    /// `last_marked_read` so `z` can undo it. None means: nothing
    /// pending, or the cursor is already on a read row.
    mark_armed_at: Option<std::time::Instant>,
    last_marked_read: Option<usize>,
    /// Per-activity expand state. Default-empty: every comment renders
    /// as one header row to keep the feed scannable. The user expands
    /// individual rows with `→`/`l` and collapses with `←`/`h`. Keyed
    /// by index because activity ordering is stable within a workspace
    /// (newest-first); cleared on workspace change.
    expanded_activities: HashSet<usize>,
}

/// How long the cursor has to sit on an unread row before we
/// auto-mark it. yazi-ish: long enough to scan past, short enough
/// that the user feels in control.
const MARK_READ_DELAY: std::time::Duration = std::time::Duration::from_millis(1000);

impl RightPane {
    pub fn new(id: PaneId) -> Self {
        Self {
            id,
            workspace: None,
            comment_scroll: 0,
            comment_cursor: 0,
            // Empty workspace → collapsed; cleared on first non-empty
            // workspace landing in `set_workspace`.
            activity_collapsed: true,
            activity_collapse_user_set: false,
            mark_armed_at: None,
            last_marked_read: None,
            expanded_activities: HashSet::new(),
        }
    }

    /// Returns the auto-mark progress as `(elapsed_ratio, label)` if
    /// armed, else None. The status footer reads this to render a
    /// progress bar. `elapsed_ratio` is clamped to [0.0, 1.0].
    pub fn auto_mark_progress(&self) -> Option<f32> {
        let armed = self.mark_armed_at?;
        let elapsed = armed.elapsed();
        let ratio = elapsed.as_secs_f32() / MARK_READ_DELAY.as_secs_f32();
        Some(ratio.clamp(0.0, 1.0))
    }

    /// Whether `z` would do something useful right now. Drives the
    /// hint footer's "z undo" entry — we only show the hint when
    /// there's actually something to undo.
    pub fn can_undo_mark_read(&self) -> bool {
        self.last_marked_read.is_some()
    }

    /// Arm the auto-mark timer iff the cursor is currently on an
    /// unread activity. Called whenever cursor or workspace state
    /// changes in a way that might affect the answer (j/k/g/G,
    /// set_workspace, focus enter). Idempotent on re-arm.
    fn rearm_mark_timer(&mut self, focused: bool) {
        if !focused {
            self.mark_armed_at = None;
            return;
        }
        let Some(workspace) = &self.workspace else {
            self.mark_armed_at = None;
            return;
        };
        if workspace.is_activity_unread(self.comment_cursor) {
            self.mark_armed_at = Some(std::time::Instant::now());
        } else {
            self.mark_armed_at = None;
        }
    }

    /// Flip the cursor's activity to read and remember the index for
    /// undo. Returns `(session_key, index)` so the caller can persist
    /// via `Command::MarkActivityRead`.
    fn fire_auto_mark(&mut self) -> Option<(pilot_core::SessionKey, usize)> {
        let workspace = self.workspace.as_mut()?;
        let i = self.comment_cursor;
        if !workspace.is_activity_unread(i) {
            return None;
        }
        workspace.mark_activity_read(i);
        self.last_marked_read = Some(i);
        self.mark_armed_at = None;
        Some((pilot_core::SessionKey::from(&workspace.key), i))
    }

    /// Undo the most recent auto-mark, if any. Returns
    /// `(session_key, index)` for the caller to persist via
    /// `Command::UnmarkActivityRead`.
    fn undo_auto_mark(&mut self) -> Option<(pilot_core::SessionKey, usize)> {
        let i = self.last_marked_read.take()?;
        let workspace = self.workspace.as_mut()?;
        workspace.unmark_activity_read(i);
        // Re-arm if the cursor is still on this row — otherwise the
        // timer would re-fire on the next tick and undo the undo.
        // Simpler: just clear armed_at; user can re-arm by moving.
        self.mark_armed_at = None;
        Some((pilot_core::SessionKey::from(&workspace.key), i))
    }

    /// Drive the auto-mark timer. Called from the App's per-tick
    /// path. Returns `(session_key, index)` when the timer fired and
    /// an activity was just marked, so the App can persist via IPC.
    pub fn tick(&mut self, focused: bool) -> Option<(pilot_core::SessionKey, usize)> {
        if !focused {
            self.mark_armed_at = None;
            return None;
        }
        let armed = self.mark_armed_at?;
        if armed.elapsed() < MARK_READ_DELAY {
            return None;
        }
        self.fire_auto_mark()
    }

    /// Called by the App when this pane's focus state flips. On focus
    /// gain we re-arm the auto-mark timer (so the user just landing
    /// on the activity feed kicks off the countdown). On focus loss
    /// we disarm and clear the undo target — switching panes is a
    /// natural "I'm done here" boundary.
    pub fn notify_focus_changed(&mut self, focused: bool) {
        if focused {
            self.rearm_mark_timer(true);
        } else {
            self.mark_armed_at = None;
            self.last_marked_read = None;
        }
    }

    /// Collapse / expand the activity section. Visible for tests and
    /// for click-to-toggle (mouse handler can call this directly).
    pub fn set_activity_collapsed(&mut self, collapsed: bool) {
        self.activity_collapsed = collapsed;
        self.activity_collapse_user_set = true;
    }

    pub fn activity_collapsed(&self) -> bool {
        self.activity_collapsed
    }

    /// Apply the auto-collapse-on-empty rule. Honours the user
    /// override: once they've toggled explicitly we don't fight them.
    fn auto_collapse_for_workspace(&mut self) {
        if self.activity_collapse_user_set {
            return;
        }
        let empty = self
            .workspace
            .as_ref()
            .map(|w| w.activity.is_empty())
            .unwrap_or(true);
        self.activity_collapsed = empty;
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
            // New workspace selection — drop the user's collapse
            // override so we re-apply the empty-aware default. Without
            // this, toggling once would stick across every workspace.
            self.activity_collapse_user_set = false;
            // Auto-mark state belongs to whatever workspace was last
            // displayed. A stale `last_marked_read` would point at an
            // index in workspace A; pressing `z` on workspace B would
            // un-read a different activity entirely. Disarm + forget.
            self.mark_armed_at = None;
            self.last_marked_read = None;
            // Indices are workspace-relative; an "expanded row 3" on
            // PR A points at a wholly different comment on PR B.
            self.expanded_activities.clear();
        }
        self.auto_collapse_for_workspace();
    }

    pub fn selected_workspace(&self) -> Option<&Workspace> {
        self.workspace.as_ref()
    }

    pub fn comment_cursor(&self) -> usize {
        self.comment_cursor
    }

    fn render_header(&self, area: Rect, frame: &mut Frame) {
        let theme = crate::theme::current();
        let Some(workspace) = &self.workspace else {
            let placeholder = Paragraph::new(Line::from(Span::styled(
                " (no session selected) ",
                theme.hint(),
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
                    Style::default()
                        .bg(theme.chrome)
                        .fg(theme.text_strong)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(&workspace.name, Style::default().bold()),
            ])];
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
            return;
        };

        let mut lines: Vec<Line> = Vec::new();

        use crate::components::icons;
        use crate::pilot_theme::{self, StatePill};

        // Breadcrumb above the title: `repo · #1234`. Dim, separated
        // by a `›`. Orients the user to "where am I?" before they
        // read the title — yazi's cwd line plays the same role.
        if let Some(repo) = task.repo.as_deref() {
            let pr_num = crate::components::task_label::pr_number(task);
            let mut crumbs: Vec<Span> = vec![
                Span::styled(
                    format!("{} ", icons::REPO),
                    Style::default().fg(theme.warn),
                ),
                Span::styled(repo.to_string(), Style::default().fg(theme.text_strong)),
            ];
            if let Some(n) = pr_num {
                crumbs.push(Span::styled(
                    "  ›  ",
                    Style::default().fg(theme.chrome),
                ));
                crumbs.push(Span::styled(
                    format!("#{n}"),
                    Style::default()
                        .fg(crate::components::task_label::pr_number_color(n))
                        .add_modifier(Modifier::BOLD),
                ));
            }
            lines.push(Line::from(crumbs));
        }

        // State pill — yazi-style powerline segment: solid block in
        // state color, prefixed by a Nerd-Font glyph, closed by a
        // triangle that "flows" into the page background.
        let (icon, label, bucket) = match task.state {
            pilot_core::TaskState::Open => (icons::PR_OPEN, "OPEN", StatePill::Open),
            pilot_core::TaskState::Draft => (icons::PR_DRAFT, "DRAFT", StatePill::Draft),
            pilot_core::TaskState::Merged => (icons::PR_MERGED, "MERGED", StatePill::Merged),
            pilot_core::TaskState::Closed => (icons::PR_CLOSED, "CLOSED", StatePill::Closed),
            pilot_core::TaskState::InProgress => (icons::PR_WIP, "WIP", StatePill::InProgress),
            pilot_core::TaskState::InReview => (icons::PR_REVIEW, "REVIEW", StatePill::InReview),
        };
        let (bg, fg) = pilot_theme::state_pill(theme, bucket);
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {icon} {label} "),
                Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(icons::POWERLINE_RIGHT, Style::default().fg(bg)),
            Span::raw(" "),
            Span::styled(
                &task.title,
                Style::default()
                    .fg(theme.text_strong)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

        // Branch line — confirms which worktree pilot will spawn an
        // agent into. Cyan accent on a "Branch:" dim label.
        let branch = task.branch.as_deref().unwrap_or("-");
        lines.push(Line::from(vec![
            Span::styled("Branch: ", Style::default().fg(theme.text_dim)),
            Span::styled(branch, Style::default().fg(theme.accent)),
        ]));

        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(para, area);
    }

    /// Render the activity section. Always renders the header row;
    /// only renders the list body when `!activity_collapsed`. The
    /// header carries: collapse glyph, "Activity", total count, and
    /// an "● N new" badge when there's unread content.
    ///
    /// Returns the row count consumed (header + optional body), so
    /// future sections stacked below can offset themselves.
    fn render_activity(&self, area: Rect, frame: &mut Frame, focused: bool) -> u16 {
        let theme = crate::theme::current();
        let title_color = if focused { theme.accent } else { theme.chrome };

        let total = self
            .workspace
            .as_ref()
            .map(|w| w.activity.len())
            .unwrap_or(0);
        let unread = self
            .workspace
            .as_ref()
            .map(|w| w.unread_count())
            .unwrap_or(0);
        // Triangle glyph mirrors the sidebar selection caret. ▸ when
        // collapsed (points right, "expand into me"), ▾ when expanded.
        let glyph = if self.activity_collapsed { "▸" } else { "▾" };

        let mut header_spans: Vec<Span> = vec![
            Span::styled(format!("{glyph} "), Style::default().fg(title_color)),
            Span::styled("Activity", theme.title(focused)),
            Span::raw("  "),
            Span::styled(format!("{total}"), Style::default().fg(theme.text_dim)),
        ];
        if unread > 0 {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled(
                format!("● {unread} new"),
                theme.badge_unread(),
            ));
        }

        let title_area = Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(2),
            1.min(area.height),
        );
        frame.render_widget(Paragraph::new(Line::from(header_spans)), title_area);

        if self.activity_collapsed {
            return 1;
        }

        if area.height < 2 {
            return 1;
        }

        let div_area = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(div_area.width as usize),
                Style::default().fg(Color::DarkGray),
            ))),
            div_area,
        );

        let inner = Rect {
            x: area.x + 1,
            y: area.y + 3,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(3),
        };

        let Some(workspace) = &self.workspace else {
            return 3;
        };

        if workspace.activity.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled("(no activity)", theme.hint()))),
                inner,
            );
            return area.height;
        }

        // Each activity renders as one card. Default shape (collapsed):
        //
        //     │ [review] alice  ›  one-line teaser of the body
        //
        // Expanded (`→` on the focused row) extends the card with up
        // to 12 wrapped body lines beneath the header, indented past
        // the marker bar so the cluster reads as one block.
        //
        // The leading `│` is the unread/cursor indicator (1 colored
        // cell, no full-row bg flood — yazi's marker_symbol pattern).
        const BODY_LINES_PER_CARD: usize = 12;
        // Indent of body content past the marker bar. 1 cell for the
        // bar itself + 3 spaces of breathing room.
        const BODY_INDENT: u16 = 4;
        // How many cells of the body to inline next to the header for
        // a collapsed row. Anything past this gets the `…` ellipsis;
        // expanding (`→`) shows the full body.
        const TEASER_CELLS: usize = 60;

        let body_width = inner.width.saturating_sub(BODY_INDENT);
        let mut cards: Vec<Line<'static>> = Vec::new();
        for (i, activity) in workspace
            .activity
            .iter()
            .enumerate()
            .skip(self.comment_scroll)
        {
            if cards.len() >= inner.height as usize {
                break;
            }

            let is_cursor = i == self.comment_cursor;
            let is_unread = workspace.is_activity_unread(i);
            let is_expanded = self.expanded_activities.contains(&i);
            let (kind_icon, kind_label) = match activity.kind {
                ActivityKind::Comment => (crate::components::icons::COMMENT, "Message"),
                ActivityKind::Review => (crate::components::icons::REVIEW, "Review"),
                ActivityKind::StatusChange => (
                    crate::components::icons::STATUS_CHANGE,
                    "Status",
                ),
                ActivityKind::CiUpdate => (crate::components::icons::CI, "CI"),
            };
            // Marker bar color encodes both unread + cursor state.
            // Cursor on a focused pane → strong accent; unread → warn;
            // otherwise the bar is invisible (chrome) so read items
            // visually retreat.
            let bar_color = if is_cursor && focused {
                theme.accent
            } else if is_unread {
                theme.warn
            } else {
                theme.chrome
            };

            // Header line: marker bar + "[kind] author" + optional
            // teaser. The teaser is one flat line of plain text — the
            // styled markdown lives in the expanded body — so the user
            // sees the gist of every comment without it taking 6 rows.
            let header_style = if is_cursor && focused {
                theme.row_focused()
            } else if is_cursor {
                theme.row_unfocused().add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            let mut header_spans: Vec<Span<'static>> = Vec::with_capacity(6);
            // Cursor caret on the focused row, plain bar otherwise.
            // Reuses the same glyph the sidebar uses so navigation
            // feels consistent across panes.
            let bar_glyph = if is_cursor && focused {
                if is_expanded { "▾ " } else { "▸ " }
            } else {
                "│ "
            };
            header_spans.push(Span::styled(
                bar_glyph,
                Style::default().fg(bar_color).add_modifier(Modifier::BOLD),
            ));
            header_spans.push(Span::styled(
                format!("{kind_icon}  {kind_label}  "),
                Style::default().fg(theme.text_dim),
            ));
            header_spans.push(Span::styled(activity.author.clone(), header_style));
            if !is_expanded {
                let teaser = teaser_text(&activity.body, TEASER_CELLS);
                if !teaser.is_empty() {
                    header_spans.push(Span::styled(
                        "  ›  ",
                        Style::default().fg(theme.chrome),
                    ));
                    header_spans.push(Span::styled(teaser, Style::default().fg(theme.text_dim)));
                }
            }
            cards.push(Line::from(header_spans));

            // Body lines only render when the user has expanded this
            // specific card. Otherwise the teaser on the header line
            // is the whole comment.
            if !is_expanded {
                continue;
            }
            // Expanding is a "I want to read this whole comment" signal,
            // so the truncation cap doesn't apply — pass usize::MAX and
            // let the Paragraph clip if a single comment is genuinely
            // taller than the viewport. The collapsed view stays at
            // BODY_LINES_PER_CARD because that's the per-card budget
            // the layout is sized for.
            let max_lines = if is_expanded {
                usize::MAX
            } else {
                BODY_LINES_PER_CARD
            };
            let body_lines = crate::components::comment_render::render_body(
                &activity.body,
                body_width,
                max_lines,
            );
            for line in body_lines {
                let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
                spans.push(Span::styled(
                    "│ ",
                    Style::default().fg(bar_color),
                ));
                // Body indent past the bar — `BODY_INDENT - 2` because
                // the bar + space already consumed 2 cells.
                spans.push(Span::raw(" ".repeat((BODY_INDENT - 2) as usize)));
                spans.extend(line.spans);
                cards.push(Line::from(spans));
            }
        }

        frame.render_widget(Paragraph::new(cards), inner);
        area.height
    }
}

/// Flatten a comment body into a single-line teaser. Strips Markdown
/// noise (HTML comments, leading hashes/quotes/bullets), collapses
/// whitespace, and clips to `max_cells` cells with `…`. Used in the
/// collapsed activity card so the user gets the gist without reading
/// six wrapped lines.
fn teaser_text(body: &str, max_cells: usize) -> String {
    let cleaned = crate::components::comment_render::strip_html_comments(body);
    // Take the first non-empty content line. Strip common Markdown
    // block markers so e.g. `### Title` reads as `Title`.
    let raw = cleaned
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let stripped = raw
        .trim_start_matches('#')
        .trim_start_matches('>')
        .trim_start_matches(|c: char| matches!(c, '-' | '*' | '+'))
        .trim();
    // Collapse runs of whitespace and drop simple inline emphasis
    // markers so the teaser stays plain text.
    let mut out = String::with_capacity(stripped.len());
    let mut last_space = false;
    for ch in stripped.chars() {
        if ch == '*' || ch == '`' {
            continue;
        }
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    let out = out.trim().to_string();
    if out.chars().count() <= max_cells {
        return out;
    }
    let mut clipped: String = out.chars().take(max_cells.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

/// Inherent methods. Lifted from the legacy `tui_kit::Pane` trait
/// impl so the realm wrappers can call them directly without UFCS.
impl RightPane {
    /// Stable pane id.
    pub fn id(&self) -> PaneId {
        self.id
    }

    /// Border title.
    pub fn title(&self) -> &str {
        "Activity"
    }

    /// Bindings shown in the hint bar.
    pub fn keymap(&self) -> &'static [crate::Binding] {
        use crate::Binding;
        &[
            Binding { keys: "j/k", label: "scroll" },
            Binding { keys: "→/←", label: "expand/collapse" },
            Binding { keys: "r", label: "reply" },
            Binding { keys: "g/G", label: "top/bottom" },
            Binding { keys: "Enter/o", label: "toggle section" },
            Binding { keys: "Tab", label: "next pane" },
        ]
    }

    pub fn detachable(&self) -> Option<crate::DetachSpec> {
        // The activity feed itself isn't detachable — but the workspace
        // it belongs to is. The detail layout will host the same data
        // alongside additional terminals + a code viewer.
        let workspace = self.workspace.as_ref()?;
        Some(crate::DetachSpec {
            layout: "detail",
            args: vec![
                "--workspace".to_string(),
                workspace.key.as_str().to_string(),
            ],
        })
    }

    pub fn handle_key(&mut self, key: KeyEvent, cmds: &mut Vec<Command>) -> PaneOutcome {
        // Toggle keys work without a workspace too — collapse state is
        // owned by the pane, not the workspace.
        match (key.code, key.modifiers) {
            (KeyCode::Enter, _)
            | (KeyCode::Char(' '), KeyModifiers::NONE)
            | (KeyCode::Char('o'), KeyModifiers::NONE) => {
                self.set_activity_collapsed(!self.activity_collapsed);
                return PaneOutcome::Consumed;
            }
            _ => {}
        }

        // `z` undoes the most recent auto-mark-read, regardless of
        // collapse state. Idempotent: pressing it twice in a row only
        // un-reads once. Disarms the auto-mark timer too — otherwise
        // it would just re-fire on the next tick. Persists via
        // `Command::UnmarkActivityRead` so a restart sees the row
        // back in the unread set.
        if key.code == KeyCode::Char('z') && key.modifiers == KeyModifiers::NONE {
            if let Some((session_key, index)) = self.undo_auto_mark() {
                cmds.push(Command::UnmarkActivityRead { session_key, index });
            }
            return PaneOutcome::Consumed;
        }

        // Comment navigation requires a workspace AND an expanded
        // section — collapsed bodies don't render rows.
        let Some(workspace) = &self.workspace else {
            return PaneOutcome::Pass;
        };
        if self.activity_collapsed {
            return PaneOutcome::Pass;
        }
        let last = workspace.activity.len().saturating_sub(1);

        let result = match (key.code, key.modifiers) {
            (KeyCode::Char('j'), KeyModifiers::NONE) | (KeyCode::Down, _) => {
                if workspace.activity.is_empty() {
                    return PaneOutcome::Consumed;
                }
                if self.comment_cursor < last {
                    self.comment_cursor += 1;
                }
                PaneOutcome::Consumed
            }
            (KeyCode::Char('k'), KeyModifiers::NONE) | (KeyCode::Up, _) => {
                self.comment_cursor = self.comment_cursor.saturating_sub(1);
                PaneOutcome::Consumed
            }
            // `→`/`l` expand the focused comment, `←`/`h` collapse it.
            // Per-row state lives in `expanded_activities` so other
            // rows stay condensed while one is being read.
            (KeyCode::Right, _) | (KeyCode::Char('l'), KeyModifiers::NONE) => {
                if !workspace.activity.is_empty() {
                    self.expanded_activities.insert(self.comment_cursor);
                }
                PaneOutcome::Consumed
            }
            (KeyCode::Left, _) | (KeyCode::Char('h'), KeyModifiers::NONE) => {
                self.expanded_activities.remove(&self.comment_cursor);
                PaneOutcome::Consumed
            }
            (KeyCode::Char('g'), KeyModifiers::NONE) => {
                self.comment_cursor = 0;
                self.comment_scroll = 0;
                PaneOutcome::Consumed
            }
            (KeyCode::Char('G'), m) if m.contains(KeyModifiers::SHIFT) => {
                self.comment_cursor = last;
                PaneOutcome::Consumed
            }
            // `m` marks the focused activity row as read — the
            // explicit per-row counterpart to the sidebar's bulk
            // `m` (which marks the whole workspace). Auto-mark-on-
            // hover already does this passively; this binding is
            // for the user who wants to clear without the timer.
            (KeyCode::Char('m'), KeyModifiers::NONE) => {
                if !workspace.activity.is_empty()
                    && workspace.is_activity_unread(self.comment_cursor)
                {
                    cmds.push(Command::MarkActivityRead {
                        session_key: workspace.key.clone().into(),
                        index: self.comment_cursor,
                    });
                    self.last_marked_read = Some(self.comment_cursor);
                }
                PaneOutcome::Consumed
            }
            _ => PaneOutcome::Pass,
        };

        // Cursor moves invalidate the previous undo target (you don't
        // get to undo a mark-read after navigating elsewhere — saves a
        // surprising "z reverts the comment two rows up" footgun) and
        // re-arm the timer for the new row.
        if result == PaneOutcome::Consumed {
            self.last_marked_read = None;
            self.rearm_mark_timer(true);
        }
        result
    }

    pub fn on_event(&mut self, event: &Event) {
        // When the currently-selected workspace is upserted, refresh
        // our local copy so comment-cursor offsets stay in range.
        let Event::WorkspaceUpserted(workspace) = event else {
            return;
        };
        let Some(current) = self.workspace.as_ref() else {
            return;
        };
        if current.key == workspace.key {
            let prev_len = current.activity.len();
            let new_len = workspace.activity.len();
            let last = new_len.saturating_sub(1);
            self.workspace = Some((**workspace).clone());
            self.comment_cursor = self.comment_cursor.min(last);
            if self.comment_scroll > last {
                self.comment_scroll = last;
            }
            // Activity is sorted newest-first; an inserted comment
            // shifts every existing index. Drop expansion state when
            // the count changes — preserving it would un-expand row N
            // while expanding the row that took its place.
            if prev_len != new_len {
                self.expanded_activities.clear();
            }
        }
    }

    pub fn render(&mut self, area: Rect, frame: &mut Frame, focused: bool) {
        // Split vertically: header (always visible), separator, then
        // the activity section. The activity section's height varies
        // with collapse state — `Min(0)` gives it whatever's left,
        // including just the header row when collapsed.
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

        let _ = self.render_activity(chunks[2], frame, focused);
    }
}

