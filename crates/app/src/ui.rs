use pilot_core::time::{self, Staleness};
use pilot_core::{ActionPriority, ActivityKind, CiStatus, ReviewStatus, TaskRole, TaskState};
use ratatui::prelude::*;
use ratatui::widgets::*;

use crate::app::{App, PendingMcpAction};
use crate::nav::{NavItem, nav_items, build_repo_groups};
use crate::picker::PickerState;
use crate::keys::KeyMode;
use pilot_core::SessionColor;

// ─── Colors (refined GitHub dark theme) ───────────────────────────────────

const C_BG: Color = Color::Rgb(15, 17, 23);
const C_BG_ALT: Color = Color::Rgb(22, 27, 34);
const C_BG_SELECTED: Color = Color::Rgb(30, 38, 50);
const C_BG_HOVER: Color = Color::Rgb(25, 32, 42);
const C_BORDER: Color = Color::Rgb(48, 54, 61);
const C_BORDER_ACTIVE: Color = Color::Rgb(88, 166, 255);
const C_TEXT: Color = Color::Rgb(201, 209, 217);
const C_TEXT_DIM: Color = Color::Rgb(125, 133, 144);
const C_TEXT_BRIGHT: Color = Color::Rgb(240, 246, 252);
const C_ACCENT: Color = Color::Rgb(88, 166, 255);
const C_RED: Color = Color::Rgb(248, 81, 73);
const C_GREEN: Color = Color::Rgb(63, 185, 80);
const C_YELLOW: Color = Color::Rgb(210, 153, 34);
const C_ORANGE: Color = Color::Rgb(219, 171, 9);
const C_MAGENTA: Color = Color::Rgb(188, 140, 255);
const C_CYAN: Color = Color::Rgb(63, 185, 208);

// ─── Main render ───────────────────────────────────────────────────────────

pub fn render(app: &mut App, frame: &mut Frame) {
    // Fill the entire background first.
    frame.render_widget(
        Block::default().style(Style::default().bg(C_BG)),
        frame.area(),
    );

    let outer = Layout::vertical([
        Constraint::Length(if app.tab_order.is_empty() { 0 } else { 1 }),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(frame.area());

    if !app.tab_order.is_empty() {
        render_tab_bar(app, frame, outer[0]);
    }
    render_main(app, frame, outer[1]);
    render_status_bar(app, frame, outer[2]);

    // MCP confirmation modal (overlay on top of everything).
    if let Some(ref action) = app.pending_mcp {
        render_mcp_confirmation(frame, frame.area(), action);
    }

    // Picker overlay.
    if let Some(ref picker) = app.picker {
        render_picker_overlay(frame, frame.area(), picker);
    }

    // New session overlay.
    if let Some(ref input) = app.new_session_input {
        render_new_session_overlay(frame, frame.area(), input);
    }

    // Quick reply overlay.
    if let Some((ref _key, ref text)) = app.quick_reply_input {
        render_quick_reply_overlay(frame, frame.area(), text);
    }

    // Help overlay.
    if app.show_help {
        render_help_overlay(frame, frame.area());
    }
}

// ─── Help overlay ─────────────────────────────────────────────────────────

fn render_help_overlay(frame: &mut Frame, area: Rect) {
    let groups = crate::keymap::all_bindings();

    // Center the help box.
    let modal_w = 70u16.min(area.width.saturating_sub(4));
    let modal_h = 28u16.min(area.height.saturating_sub(2));
    let x = (area.width.saturating_sub(modal_w)) / 2;
    let y = (area.height.saturating_sub(modal_h)) / 2;
    let modal_area = Rect::new(x, y, modal_w, modal_h);

    frame.render_widget(Clear, modal_area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::raw(""));

    for (category, items) in &groups {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(*category, Style::default().fg(C_ACCENT).bold()),
        ]));
        for (short, desc, modes) in items {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("    {short:<10}"),
                    Style::default().fg(C_TEXT_BRIGHT).bold(),
                ),
                Span::styled(*desc, Style::default().fg(C_TEXT)),
                Span::styled(format!("  [{modes}]"), Style::default().fg(C_TEXT_DIM)),
            ]));
        }
        lines.push(Line::raw(""));
    }
    lines.push(Line::from(Span::styled(
        "  Press any key to close",
        Style::default().fg(C_TEXT_DIM).italic(),
    )));

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            " Keybindings (N=Normal D=Detail T=Terminal P=PanePrefix) ",
            Style::default().fg(C_ACCENT).bold(),
        ))
        .border_style(Style::default().fg(C_ACCENT))
        .style(Style::default().bg(C_BG_ALT));

    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    frame.render_widget(para, modal_area);
}

// ─── Tab bar ──────────────────────────────────────────────────────────────

fn render_tab_bar(app: &App, frame: &mut Frame, area: Rect) {
    let tabs: Vec<Span> = app
        .tab_order
        .iter()
        .enumerate()
        .flat_map(|(i, key)| {
            let is_active = i == app.active_tab;
            let session = app.sessions.get(key);
            let label = session
                .map(|s| s.primary_task.id.key.as_str())
                .unwrap_or("?");
            let color = session
                .map(|s| session_color(s.color))
                .unwrap_or(C_TEXT_DIM);
            let style = if is_active {
                Style::default().fg(color).bg(C_BG_SELECTED).bold()
            } else {
                Style::default().fg(C_TEXT_DIM)
            };
            let unread = session.map(|s| s.unread_count()).unwrap_or(0);
            let dot = if unread > 0 { " *" } else { "" };
            vec![
                Span::styled(format!(" {} ", i + 1), Style::default().fg(C_TEXT_DIM)),
                Span::styled(format!("{label}{dot}"), style),
                Span::styled(
                    " |",
                    Style::default().fg(C_BORDER),
                ),
            ]
        })
        .collect();

    let bar = Line::from(tabs);
    frame.render_widget(
        Paragraph::new(bar).style(Style::default().bg(C_BG_ALT)),
        area,
    );
}

// ─── Main layout ──────────────────────────────────────────────────────────

fn render_main(app: &mut App, frame: &mut Frame, area: Rect) {
    let pct = app.sidebar_pct.clamp(20, 80);
    let chunks = Layout::horizontal([
        Constraint::Percentage(pct),
        Constraint::Percentage(100 - pct),
    ])
    .split(area);

    render_sidebar(app, frame, chunks[0]);
    render_right_pane(app, frame, chunks[1]);
}

// ─── Sidebar (borderless, clean table-like list) ──────────────────────────

fn render_sidebar(app: &App, frame: &mut Frame, area: Rect) {
    // No border for the sidebar — use the full area.
    let total_unread: usize = app.sessions.values().map(|s| s.unread_count()).sum();
    let time_label = match app.activity_days_filter {
        0 => "all".to_string(),
        d => format!("{d}d"),
    };
    let visible_count = nav_items(app)
        .iter()
        .filter(|i| matches!(i, NavItem::Session(_)))
        .count();

    // Split: header(1) + separator(1) + search(1) + summary(optional) + list + action_bar(1)
    let mut constraints = vec![
        Constraint::Length(1), // header
        Constraint::Length(1), // separator
        Constraint::Length(1), // search bar
    ];
    constraints.push(Constraint::Min(1)); // list area
    constraints.push(Constraint::Length(1)); // action bar
    let chunks = Layout::vertical(constraints).split(area);

    let header_area = chunks[0];
    let sep_area = chunks[1];
    let search_area = chunks[2];
    let list_area = chunks[3];
    let action_area = chunks[4];

    // ── Header: PILOT brand + counts ──
    let mut header_spans = vec![
        Span::styled("  PILOT", Style::default().fg(C_ACCENT).bold()),
        Span::styled(format!("  {visible_count}", ), Style::default().fg(C_TEXT_DIM)),
    ];
    if total_unread > 0 {
        header_spans.push(Span::styled(
            format!("  {total_unread} new", ),
            Style::default().fg(C_RED).bold(),
        ));
    }
    header_spans.push(Span::styled(
        format!("  [{time_label}]"),
        Style::default().fg(C_YELLOW),
    ));
    frame.render_widget(
        Paragraph::new(Line::from(header_spans)).style(Style::default().bg(C_BG)),
        header_area,
    );

    // ── Thin separator ──
    let sep_width = sep_area.width as usize;
    let sep_line = thin_separator(sep_width);
    frame.render_widget(
        Paragraph::new(sep_line).style(Style::default().bg(C_BG)),
        sep_area,
    );

    // ── Search bar ──
    let search_style = if app.search_active {
        Style::default().fg(C_TEXT_BRIGHT).bg(C_BG_HOVER)
    } else {
        Style::default().fg(C_TEXT_DIM)
    };
    let cursor = if app.search_active { "|" } else { "" };
    let search_text = if app.search_active || !app.search_query.is_empty() {
        format!("  /{}{cursor}", app.search_query)
    } else {
        "  / filter (needs:reply ci:failed ...)".into()
    };
    frame.render_widget(
        Paragraph::new(Span::styled(search_text, search_style)).style(Style::default().bg(C_BG)),
        search_area,
    );

    // ── Build nav items list ──
    let items = nav_items(app);
    let repos = build_repo_groups(app);
    let mut lines: Vec<Line> = Vec::new();
    let w = list_area.width as usize;

    // Loading spinner.
    if !app.loaded {
        let spinner = [
            "   ", ".  ", ".. ", "...", " ..", "  .", "   ", ".  ", ".. ", "...",
        ];
        let s = spinner[(app.tick_count as usize / 2) % spinner.len()];
        lines.push(Line::from(vec![
            Span::styled(format!("  {s} "), Style::default().fg(C_ACCENT)),
            Span::styled("Loading", Style::default().fg(C_TEXT)),
        ]));
    }

    // Empty state.
    let session_count = items
        .iter()
        .filter(|i| matches!(i, NavItem::Session(_)))
        .count();
    if app.loaded && session_count == 0 {
        let reason = if !app.search_query.is_empty() {
            format!("  No matches for /{}", app.search_query)
        } else if app.activity_days_filter > 0 {
            format!("  No PRs active in last {}d", app.activity_days_filter)
        } else if app.config.providers.github.filters.is_empty() {
            "  No PRs found.".to_string()
        } else {
            "  No PRs match your filters.".to_string()
        };
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            reason,
            Style::default().fg(C_TEXT_DIM).italic(),
        )));
        lines.push(Line::raw(""));
        if app.activity_days_filter > 0 {
            lines.push(Line::from(Span::styled(
                "  Press 't' to widen the time filter.",
                Style::default().fg(C_TEXT_DIM),
            )));
        }
    }

    // ── Priority summary bar (compact triage counts) ──
    if app.loaded && session_count > 0 {
        let mut counts = std::collections::HashMap::<ActionPriority, usize>::new();
        for it in &items {
            if let NavItem::Session(k) = it
                && let Some(s) = app.sessions.get(k)
            {
                *counts.entry(s.action_priority(&app.username)).or_insert(0) += 1;
            }
        }

        let mut summary = vec![Span::styled("  ", Style::default().fg(C_TEXT_DIM))];
        let mut pushed = false;
        for (prio, label, color) in [
            (ActionPriority::NeedsReply, "reply", C_RED),
            (ActionPriority::CiFailed, "CI", C_RED),
            (ActionPriority::ChangesRequested, "chg", C_ORANGE),
            (ActionPriority::NeedsYourReview, "review", C_YELLOW),
            (ActionPriority::ApprovedReadyToMerge, "merge", C_GREEN),
        ] {
            if let Some(&n) = counts.get(&prio) {
                if pushed {
                    summary.push(Span::styled("  ", Style::default().fg(C_BORDER)));
                }
                summary.push(Span::styled(
                    format!("{n}"),
                    Style::default().fg(color).bold(),
                ));
                summary.push(Span::styled(
                    format!(" {label}"),
                    Style::default().fg(C_TEXT_DIM),
                ));
                pushed = true;
            }
        }
        if pushed {
            lines.push(Line::from(summary));
            lines.push(Line::raw(""));
        }
    }

    // Pre-compute repo stats.
    let repo_stats: std::collections::HashMap<&str, (usize, usize)> = repos
        .iter()
        .map(|(repo, keys)| {
            let unread: usize = keys
                .iter()
                .filter_map(|k| app.sessions.get(k))
                .map(|s| s.unread_count())
                .sum();
            (repo.as_str(), (keys.len(), unread))
        })
        .collect();

    // ── Column widths for session rows ──
    // Layout: accent(1) + ci(2) + pr#(7) + title(flex) + indicators(6) + status(10)
    let col_accent: usize = 1;
    let col_ci: usize = 2;
    let col_pr: usize = 7; // e.g. " #7283"
    let col_indicators: usize = 6;
    let col_status: usize = 10;
    let col_fixed = col_accent + col_ci + col_pr + col_indicators + col_status;
    let _col_title = w.saturating_sub(col_fixed);

    // Braille spinner frames for active agent.
    let _braille_frames = [
        "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}",
        "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
        "\u{2807}", "\u{280f}",
    ];

    // ── Render each nav item ──
    for (nav_idx, item) in items.iter().enumerate() {
        let is_cursor = nav_idx == app.selected;

        match item {
            NavItem::Repo(repo) => {
                let collapsed = app.collapsed_repos.contains(repo);
                let (count, unread) = repo_stats.get(repo.as_str()).copied().unwrap_or((0, 0));
                let repo_short = repo.rsplit('/').next().unwrap_or(repo);

                // Repo header: ──── repo (5) ────────────────
                let label = format!(" {repo_short} ({count})");
                let unread_part = if unread > 0 {
                    format!(" {unread} new ")
                } else {
                    " ".to_string()
                };
                let collapse_marker = if collapsed { " + " } else { "" };

                let used = 4 + label.len() + unread_part.len() + collapse_marker.len();
                let fill_len = w.saturating_sub(used);
                let fill: String = "\u{2500}".repeat(fill_len); // ─

                let bg = if is_cursor { C_BG_SELECTED } else { C_BG };
                let mut spans = vec![
                    Span::styled(
                        format!("  {}\u{2500}\u{2500}", if is_cursor { "\u{25b8}" } else { "\u{2500}" }),
                        Style::default().fg(C_BORDER).bg(bg),
                    ),
                    Span::styled(
                        label,
                        if is_cursor {
                            Style::default().fg(C_TEXT_BRIGHT).bold().bg(bg)
                        } else {
                            Style::default().fg(C_ACCENT).bold().bg(bg)
                        },
                    ),
                ];
                if unread > 0 {
                    spans.push(Span::styled(
                        unread_part,
                        Style::default().fg(C_RED).bg(bg),
                    ));
                } else {
                    spans.push(Span::styled(
                        unread_part,
                        Style::default().fg(C_BORDER).bg(bg),
                    ));
                }
                if collapsed {
                    spans.push(Span::styled(
                        collapse_marker,
                        Style::default().fg(C_TEXT_DIM).bg(bg),
                    ));
                }
                spans.push(Span::styled(
                    fill,
                    Style::default().fg(C_BORDER).bg(bg),
                ));
                lines.push(Line::from(spans));
            }
            NavItem::Session(key) => {
                let Some(session) = app.sessions.get(key) else {
                    continue;
                };
                let task = &session.primary_task;
                let unread = session.unread_count();
                let priority = session.action_priority("");
                let bg = if is_cursor { C_BG_SELECTED } else { C_BG };

                // Does this PR require MY action? Bold the whole row if yes.
                let needs_my_action = matches!(
                    priority,
                    ActionPriority::NeedsReply
                        | ActionPriority::CiFailed
                        | ActionPriority::ChangesRequested
                        | ActionPriority::NeedsYourReview
                        | ActionPriority::ApprovedReadyToMerge
                );

                // ── STATE column: composite readiness. Show the WORST blocker. ──
                let is_author = task.role == TaskRole::Author;
                let ci_ok = matches!(task.ci, CiStatus::Success | CiStatus::None);
                let ci_running = matches!(task.ci, CiStatus::Running | CiStatus::Pending);
                let review_ok = task.review == ReviewStatus::Approved;
                let no_conflicts = !task.has_conflicts;

                let (state_text, state_color) = match task.state {
                    TaskState::Draft => ("DRAFT", C_TEXT_DIM),
                    TaskState::Merged => ("MERGED", C_MAGENTA),
                    TaskState::Closed => ("CLOSED", C_RED),
                    _ if task.in_merge_queue && review_ok && ci_ok && no_conflicts => ("QUEUED", C_MAGENTA),
                    _ if ci_ok && review_ok && no_conflicts => ("READY", C_GREEN),
                    // Blockers — show the worst one.
                    _ if task.has_conflicts => ("CONFLICT", C_RED),
                    _ if task.ci == CiStatus::Failure => ("CI FAIL", C_RED),
                    _ if task.review == ReviewStatus::ChangesRequested => ("CHANGES", C_ORANGE),
                    _ if ci_running => ("CI...", C_YELLOW),
                    _ if task.review == ReviewStatus::Pending => ("REVIEW", C_YELLOW),
                    _ if ci_ok && task.review == ReviewStatus::None => ("NO REV", C_YELLOW),
                    _ => ("OPEN", C_TEXT_DIM),
                };

                // ── ACTION column: what's the ONE thing I should do right now? ──
                let (action_text, action_color) = if task.has_conflicts && is_author {
                    ("REBASE", C_RED)
                } else if task.ci == CiStatus::Failure && is_author {
                    ("FIX", C_RED)
                } else if task.needs_reply && is_author {
                    ("RESPOND", C_RED)
                } else if task.review == ReviewStatus::ChangesRequested && is_author {
                    ("PUSH", C_ORANGE)
                } else if task.role == TaskRole::Reviewer {
                    ("REVIEW", C_YELLOW)
                } else if ci_ok && review_ok && no_conflicts && is_author {
                    ("MERGE", C_GREEN)
                } else if is_author && ci_ok && task.reviewers.is_empty() && task.assignees.is_empty() {
                    ("ASSIGN", C_YELLOW)
                } else if is_author && ci_ok && matches!(task.review, ReviewStatus::None | ReviewStatus::Pending) {
                    ("NUDGE", C_YELLOW)
                } else {
                    ("", C_TEXT_DIM)
                };

                // ── TIME column (5 chars) ──
                let time_str = time_ago_short(&task.updated_at);

                // ── Unread indicator ──
                let unread_marker = if unread > 0 {
                    format!("*{unread}")
                } else {
                    String::new()
                };

                // ── Agent indicator (compact) ──
                let agent_marker = if app.terminals.contains_key(key) {
                    use crate::agent_state::AgentState;
                    match app.agent_states.get(key).copied().unwrap_or(AgentState::Active) {
                        AgentState::Active => {
                            let frames = ["\u{2807}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}", "\u{2807}", "\u{280f}"];
                            frames[(app.tick_count as usize / 2) % frames.len()].to_string()
                        }
                        AgentState::Idle => ">_".into(),
                        AgentState::Asking => "?!".into(),
                    }
                } else if session.is_monitored() {
                    "\u{25c9}".into() // ◉ = monitored
                } else {
                    String::new()
                };

                // ── CI icon ──
                let (ci_ch, ci_color) = match task.ci {
                    CiStatus::Success => ("\u{2713}", C_GREEN),
                    CiStatus::Failure => ("\u{2717}", C_RED),
                    CiStatus::Running | CiStatus::Pending => ("\u{25e6}", C_YELLOW),
                    _ => ("\u{00b7}", C_TEXT_DIM),
                };

                // ── PR number ──
                let pr_num = task.id.key.rsplit_once('#')
                    .map(|(_, n)| format!("#{n}"))
                    .unwrap_or_default();
                let pr_color = pr_number_color(&pr_num);

                // ── Title style: bold if action needed, bright if unread ──
                let title_style = if is_cursor {
                    Style::default().fg(C_TEXT_BRIGHT).bold().bg(bg)
                } else if needs_my_action {
                    Style::default().fg(C_TEXT_BRIGHT).bold().bg(bg)
                } else if unread > 0 {
                    Style::default().fg(C_TEXT_BRIGHT).bg(bg)
                } else {
                    Style::default().fg(C_TEXT).bg(bg)
                };

                // ── Compute column widths ──
                // Layout: accent(2) ci(2) pr#(6) title(fill) state(9) action(8) time(5)
                let right_cols = 5 + 9 + 8 + 6 + 2; // indicators + state + action + time + gaps
                let left_cols = 2 + 2 + 6; // accent + ci + pr#
                let title_avail = w.saturating_sub(left_cols + right_cols);
                let title_text = truncate_str(&task.title, title_avail);
                let title_pad = title_avail.saturating_sub(title_text.len());

                // ── Build the row ──
                let row = Line::from(vec![
                    // Accent bar
                    Span::styled(
                        if is_cursor { "\u{258c} " } else { "  " },
                        if is_cursor { Style::default().fg(C_ACCENT).bg(bg) } else { Style::default().bg(bg) },
                    ),
                    // CI
                    Span::styled(format!("{ci_ch} "), Style::default().fg(ci_color).bg(bg)),
                    // PR#
                    Span::styled(format!("{pr_num:<5} "), Style::default().fg(pr_color).bg(bg)),
                    // Title
                    Span::styled(title_text, title_style),
                    Span::styled(" ".repeat(title_pad), Style::default().bg(bg)),
                    // Unread + agent (compact, before state)
                    if !unread_marker.is_empty() || !agent_marker.is_empty() {
                        let combined = format!("{}{}{}", unread_marker, if !unread_marker.is_empty() && !agent_marker.is_empty() { " " } else { "" }, agent_marker);
                        Span::styled(format!("{combined:>4} "), Style::default().fg(if unread > 0 { C_RED } else { C_YELLOW }).bg(bg))
                    } else {
                        Span::styled("     ", Style::default().bg(bg))
                    },
                    // STATE
                    Span::styled(format!("{state_text:>8} "), Style::default().fg(state_color).bg(bg)),
                    // ACTION (bold if present)
                    if !action_text.is_empty() {
                        Span::styled(format!("{action_text:>7} "), Style::default().fg(action_color).bold().bg(bg))
                    } else {
                        Span::styled("        ", Style::default().bg(bg))
                    },
                    // TIME
                    Span::styled(format!(" {time_str:>5}"), Style::default().fg(C_TEXT_DIM).bg(bg)),
                ]);

                lines.push(row);
            }
        }
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(C_BG)),
        list_area,
    );

    // ── Action bar (bottom) ──
    let hints = crate::keymap::action_bar_for_mode(crate::keys::KeyMode::Normal);
    let action_bar = build_action_bar(&hints);
    frame.render_widget(
        Paragraph::new(action_bar).style(Style::default().bg(C_BG_ALT)),
        action_area,
    );
}

// ─── Right pane (detail or terminal or both) ───────────────────────────────

fn render_right_pane(app: &mut App, frame: &mut Frame, area: Rect) {
    let selected_key = app.selected_session_key();

    let Some(key) = selected_key else {
        render_welcome(app, frame, area);
        return;
    };

    let has_terminal = app.terminals.contains_key(&key);

    if has_terminal {
        let chunks = Layout::vertical([
            Constraint::Percentage(30), // detail header + comments
            Constraint::Percentage(70), // terminal
        ])
        .split(area);
        render_detail(app, frame, chunks[0], &key);
        render_terminal(app, frame, chunks[1], &key);
    } else {
        render_detail(app, frame, area, &key);
    }
}

// ─── Detail pane (PR header + selectable comment thread) ──────────────────

fn render_detail(app: &App, frame: &mut Frame, area: Rect, key: &str) {
    let Some(session) = app.sessions.get(key) else {
        return;
    };
    let task = &session.primary_task;
    let is_focused = app.key_mode == KeyMode::Detail;
    let border_color = if is_focused { C_BORDER_ACTIVE } else { C_BORDER };

    // Left border only to separate from sidebar.
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(C_BG))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let header_height = 4 // state+title line, separator, CI/review line, reviewers line
        + if !task.labels.is_empty() { 1 } else { 0 }
        + if session.monitor.is_some() { 1 } else { 0 };
    let chunks = Layout::vertical([
        Constraint::Length(header_height), // PR header
        Constraint::Min(3),               // comment thread
        Constraint::Length(1),            // action bar
    ])
    .split(inner);

    // ── PR header ──
    let staleness = time::staleness(&task.updated_at, &task.updated_at);
    let stale_span = match staleness {
        Staleness::Stale { idle_days } => Span::styled(
            format!("  idle {idle_days}d"),
            Style::default().fg(C_YELLOW),
        ),
        Staleness::Abandoned { idle_days, .. } => Span::styled(
            format!("  idle {idle_days}d"),
            Style::default().fg(C_RED),
        ),
        Staleness::Fresh => Span::raw(""),
    };

    let ago = time::time_ago(&task.updated_at);
    let sep_width = chunks[0].width as usize;

    let mut header = vec![
        // Title line: STATE  Title                               2h ago
        Line::from(vec![
            state_pill(&task.state),
            Span::raw("  "),
            Span::styled(&task.title, Style::default().fg(C_TEXT_BRIGHT).bold()),
            stale_span,
            Span::styled(format!("  {ago}"), Style::default().fg(C_TEXT_DIM)),
        ]),
        // Thin separator.
        thin_separator(sep_width),
        // Status line with clear field labels.
        {
            let mut spans = vec![
                Span::styled("CI: ", Style::default().fg(C_TEXT_DIM)),
                ci_span(&task.ci),
                Span::styled("   Review: ", Style::default().fg(C_TEXT_DIM)),
                review_span(&task.review),
                Span::styled("   Role: ", Style::default().fg(C_TEXT_DIM)),
                role_span(&task.role),
            ];
            if task.has_conflicts {
                spans.push(Span::styled("   CONFLICT", Style::default().fg(C_RED).bold()));
            }
            if let Some(ref branch) = task.branch {
                spans.push(Span::styled(
                    format!("   Branch: {branch}"),
                    Style::default().fg(C_TEXT_DIM),
                ));
            }
            Line::from(spans)
        },
    ];

    // Reviewers / Assignees line.
    let reviewers_str = if task.reviewers.is_empty() {
        "none".to_string()
    } else {
        task.reviewers.join(", ")
    };
    let assignees_str = if task.assignees.is_empty() {
        "none".to_string()
    } else {
        task.assignees.join(", ")
    };
    header.push(Line::from(vec![
        Span::styled("Reviewers: ", Style::default().fg(C_TEXT_DIM)),
        Span::styled(&reviewers_str, Style::default().fg(C_CYAN)),
        Span::raw("    "),
        Span::styled("Assignees: ", Style::default().fg(C_TEXT_DIM)),
        Span::styled(&assignees_str, Style::default().fg(C_MAGENTA)),
        if task.additions > 0 || task.deletions > 0 {
            Span::styled(
                format!("    +{} -{}", task.additions, task.deletions),
                Style::default().fg(C_TEXT_DIM),
            )
        } else {
            Span::raw("")
        },
    ]));

    // Labels as colored pills.
    if !task.labels.is_empty() {
        let mut label_spans: Vec<Span> = Vec::new();
        for label in &task.labels {
            let lc = label_pill_color(label);
            label_spans.push(Span::styled(
                format!(" {label} "),
                Style::default().fg(Color::Rgb(15, 17, 23)).bg(lc).bold(),
            ));
            label_spans.push(Span::raw(" "));
        }
        header.push(Line::from(label_spans));
    }

    // Monitor label.
    if let Some(label) = session.monitor_label() {
        header.push(Line::from(vec![
            Span::styled(
                " MONITOR ",
                Style::default()
                    .fg(Color::Rgb(15, 17, 23))
                    .bg(C_CYAN)
                    .bold(),
            ),
            Span::styled(format!(" {label}"), Style::default().fg(C_CYAN)),
        ]));
    }
    frame.render_widget(
        Paragraph::new(header).style(Style::default().bg(C_BG)),
        chunks[0],
    );

    // ── PR description (if present) ──
    let mut comment_lines: Vec<Line> = Vec::new();

    if let Some(ref body) = task.body {
        let body_trimmed = body.trim();
        if !body_trimmed.is_empty() {
            // Compact description — just 3 lines, dimmed. Activity is the focus.
            let md_lines = render_markdown(body_trimmed);
            let preview_lines = md_lines.len().min(3);
            for line in md_lines.into_iter().take(preview_lines) {
                let mut padded = vec![Span::styled("  ", Style::default().fg(C_TEXT_DIM))];
                for span in line.spans {
                    padded.push(Span::styled(span.content.to_string(), span.style.fg(C_TEXT_DIM)));
                }
                comment_lines.push(Line::from(padded));
            }
            comment_lines.push(Line::raw(""));
        }
    }

    // ── Comment thread (card-like blocks) ──

    if session.activity.is_empty() {
        comment_lines.push(Line::from(Span::styled(
            "  No activity yet",
            Style::default().fg(C_TEXT_DIM).italic(),
        )));
    } else {
        let any_selected = !app.selected_comments.is_empty();
        let comment_width = chunks[1].width as usize;

        for (i, a) in session.activity.iter().enumerate() {
            let is_cursor = is_focused && i == app.detail_cursor;
            let is_checked = app.selected_comments.contains(&i);
            let is_unread = session.is_activity_unread(i);

            let bg = if is_cursor { C_BG_SELECTED } else { C_BG };
            let bar_color = if is_cursor {
                C_ACCENT
            } else if is_unread {
                C_CYAN
            } else {
                C_BORDER
            };

            // Comment header: | author . 2h ago . Review
            let icon = match a.kind {
                ActivityKind::Comment => "comment",
                ActivityKind::Review => "review",
                ActivityKind::StatusChange => "status",
                ActivityKind::CiUpdate => "ci",
            };

            let ago_str = time::time_ago(&a.created_at);

            // Checkbox or read indicator.
            let prefix = if any_selected || is_checked {
                if is_checked {
                    Span::styled("[x] ", Style::default().fg(C_GREEN).bold().bg(bg))
                } else {
                    Span::styled("[ ] ", Style::default().fg(C_TEXT_DIM).bg(bg))
                }
            } else if is_unread {
                Span::styled("* ", Style::default().fg(C_RED).bg(bg))
            } else {
                Span::styled("  ", Style::default().fg(C_TEXT_DIM).bg(bg))
            };

            let author_style = if is_unread {
                Style::default().fg(C_TEXT_BRIGHT).bold().bg(bg)
            } else {
                Style::default().fg(C_TEXT_DIM).bg(bg)
            };

            // Top border of comment card.
            let card_top_len = comment_width.saturating_sub(6);
            let card_top_fill: String = "\u{2500}".repeat(card_top_len);
            comment_lines.push(Line::from(vec![
                Span::styled(
                    " \u{250c} ",
                    Style::default().fg(bar_color).bg(bg),
                ),
                prefix,
                Span::styled(&a.author, author_style),
                Span::styled(
                    format!(" \u{00b7} {ago_str} \u{00b7} {icon} "),
                    Style::default().fg(C_TEXT_DIM).bg(bg),
                ),
                Span::styled(card_top_fill, Style::default().fg(C_BORDER).bg(bg)),
            ]));

            // Body line(s).
            let body_style = if is_unread {
                Style::default().fg(C_TEXT).bg(bg)
            } else {
                Style::default().fg(C_TEXT_DIM).bg(bg)
            };
            if !a.body.is_empty() {
                let bar_prefix = format!(" \u{2502} ");
                let bar_w = bar_prefix.len();
                let wrap_width = comment_width.saturating_sub(bar_w + 1);

                // Render comment body as markdown, then word-wrap long lines.
                let md_lines = render_markdown(&a.body);
                let mut body_line_count = 0;
                for md_line in md_lines.into_iter() {
                    if body_line_count >= 12 { break; }
                    // Concatenate all spans into plain text for wrapping.
                    let full_text: String = md_line.spans.iter()
                        .map(|s| s.content.as_ref())
                        .collect();
                    if full_text.is_empty() {
                        comment_lines.push(Line::from(vec![
                            Span::styled(bar_prefix.clone(), Style::default().fg(bar_color).bg(bg)),
                        ]));
                        body_line_count += 1;
                        continue;
                    }
                    // Get the style from the first span (simplified — preserves bold/color).
                    let line_style = md_line.spans.first()
                        .map(|s| s.style)
                        .unwrap_or(body_style);

                    // Word-wrap.
                    let mut remaining = full_text.as_str();
                    while !remaining.is_empty() && body_line_count < 12 {
                        let chunk = if remaining.len() <= wrap_width {
                            remaining
                        } else {
                            // Find last space before wrap_width.
                            let cut = remaining[..wrap_width]
                                .rfind(' ')
                                .unwrap_or(wrap_width);
                            &remaining[..cut]
                        };
                        comment_lines.push(Line::from(vec![
                            Span::styled(bar_prefix.clone(), Style::default().fg(bar_color).bg(bg)),
                            Span::styled(chunk.to_string(), line_style),
                        ]));
                        remaining = remaining[chunk.len()..].trim_start();
                        body_line_count += 1;
                    }
                }
            }

            // Bottom border of comment card.
            let card_bot_len = comment_width.saturating_sub(4);
            let card_bot_fill: String = "\u{2500}".repeat(card_bot_len);
            comment_lines.push(Line::from(vec![
                Span::styled(
                    " \u{2514}",
                    Style::default().fg(bar_color).bg(bg),
                ),
                Span::styled(card_bot_fill, Style::default().fg(C_BORDER).bg(bg)),
            ]));
        }
    }

    // CI checks — ONLY show failing checks. Nobody cares about green or pending.
    {
        let failed: Vec<_> = task.checks.iter()
            .filter(|c| matches!(c.status, CiStatus::Failure))
            .collect();
        if !failed.is_empty() {
            let total = task.checks.len();
            let passed = total - failed.len();
            comment_lines.insert(0, Line::raw(""));
            for check in failed.iter().rev().take(5) {
                comment_lines.insert(0, Line::from(vec![
                    Span::styled("    \u{2717} ", Style::default().fg(C_RED)),
                    Span::styled(
                        check.name.clone(),
                        Style::default().fg(C_RED),
                    ),
                ]));
            }
            let summary = format!(
                "  {} failed, {} passed",
                failed.len(),
                passed,
            );
            comment_lines.insert(0, Line::from(
                Span::styled(summary, Style::default().fg(C_RED).bold()),
            ));
        }
    }

    frame.render_widget(
        Paragraph::new(comment_lines)
            .scroll((app.detail_scroll, 0))
            .style(Style::default().bg(C_BG)),
        chunks[1],
    );

    // ── Action bar ──
    let n_selected = app.selected_comments.len();

    let action_bar = if !is_focused {
        let hints = crate::keymap::action_bar_for_mode(crate::keys::KeyMode::Normal);
        build_action_bar(&hints)
    } else if n_selected > 0 {
        let mut spans = vec![
            Span::styled(
                format!(" {n_selected} selected "),
                Style::default()
                    .fg(Color::Rgb(15, 17, 23))
                    .bg(C_ACCENT)
                    .bold(),
            ),
            Span::styled(" ", Style::default()),
        ];
        let hints = crate::keymap::action_bar_for_mode(crate::keys::KeyMode::Detail);
        for (short, label) in &hints {
            spans.push(Span::styled(
                short.to_string(),
                Style::default().fg(C_ACCENT).bold(),
            ));
            spans.push(Span::styled(
                format!(":{label} "),
                Style::default().fg(C_TEXT_DIM),
            ));
        }
        Line::from(spans)
    } else {
        let hints = crate::keymap::action_bar_for_mode(crate::keys::KeyMode::Detail);
        build_action_bar(&hints)
    };
    frame.render_widget(
        Paragraph::new(action_bar).style(Style::default().bg(C_BG_ALT)),
        chunks[2],
    );
}

// ─── Terminal ─────────────────────────────────────────────────────────────

fn render_terminal(app: &mut App, frame: &mut Frame, area: Rect, key: &str) {
    let is_focused = app.key_mode == KeyMode::Terminal;
    let border_color = if is_focused { C_GREEN } else { C_BORDER };

    let shell_label = match app.terminal_kinds.get(key) {
        Some(crate::action::ShellKind::Claude) => "Claude Code",
        Some(crate::action::ShellKind::Shell) => "Shell",
        None => "Terminal",
    };
    let hint = if is_focused { "Ctrl-] exit" } else { "Tab focus" };
    let scrolled = app
        .terminals
        .get(key)
        .map(|t| t.is_scrolled())
        .unwrap_or(false);
    let scroll_indicator = if scrolled {
        Span::styled(
            " [SCROLLBACK] ",
            Style::default().fg(C_YELLOW).bold(),
        )
    } else {
        Span::raw("")
    };

    let block = Block::bordered()
        .title(Line::from(vec![
            Span::styled(
                format!(" {shell_label} "),
                Style::default().fg(C_GREEN).bold(),
            ),
            scroll_indicator,
            Span::styled(hint, Style::default().fg(C_TEXT_DIM)),
            Span::raw(" "),
        ]))
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let new_size = (inner.width, inner.height);
    if new_size != app.last_term_area && new_size.0 > 0 && new_size.1 > 0 {
        app.last_term_area = new_size;
        if let Some(term) = app.terminals.get_mut(key) {
            let _ = term.resize(pilot_tui_term::PtySize {
                rows: new_size.1,
                cols: new_size.0,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    if let Some(term) = app.terminals.get_mut(key) {
        render_term_content(term, frame, inner);
    }
}

// ─── Welcome / Loading ────────────────────────────────────────────────────

fn render_welcome(app: &App, frame: &mut Frame, area: Rect) {
    // Left border only.
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(C_BORDER))
        .style(Style::default().bg(C_BG))
        .padding(Padding::new(2, 2, 1, 0));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let spinner = [".", "..", "...", ".. ", ".  "];
    let s = spinner[(app.tick_count as usize / 2) % spinner.len()];

    let lines = if !app.loaded {
        let filter = app
            .config
            .providers
            .github
            .filters
            .iter()
            .filter_map(|f| f.org.as_ref().map(|o| format!("org:{o}")))
            .collect::<Vec<_>>()
            .join(", ");
        let filter = if filter.is_empty() {
            "all repos".into()
        } else {
            filter
        };
        vec![
            Line::raw(""),
            Line::from(Span::styled(
                "PILOT",
                Style::default().fg(C_ACCENT).bold(),
            )),
            Line::raw(""),
            Line::from(vec![
                Span::styled(format!("{s} "), Style::default().fg(C_ACCENT)),
                Span::styled(
                    "Fetching PRs and issues",
                    Style::default().fg(C_TEXT),
                ),
            ]),
            Line::raw(""),
            Line::from(vec![
                Span::styled("Scope: ", Style::default().fg(C_TEXT_DIM)),
                Span::styled(filter, Style::default().fg(C_TEXT)),
            ]),
            Line::from(vec![
                Span::styled("Auth:  ", Style::default().fg(C_TEXT_DIM)),
                Span::styled(&app.status, Style::default().fg(C_TEXT)),
            ]),
        ]
    } else if app.sessions.is_empty() {
        vec![
            Line::raw(""),
            Line::from(Span::styled(
                "PILOT",
                Style::default().fg(C_ACCENT).bold(),
            )),
            Line::raw(""),
            Line::from(Span::styled(
                "No PRs found.",
                Style::default().fg(C_TEXT),
            )),
            Line::raw(""),
            Line::from(Span::styled(
                "Check your ~/.pilot/config.yaml filters.",
                Style::default().fg(C_TEXT_DIM),
            )),
        ]
    } else {
        vec![
            Line::raw(""),
            Line::from(Span::styled(
                "PILOT",
                Style::default().fg(C_ACCENT).bold(),
            )),
            Line::raw(""),
            Line::from(Span::styled(
                "Select a PR from the sidebar.",
                Style::default().fg(C_TEXT),
            )),
            Line::raw(""),
            Line::from(vec![
                Span::styled("j/k   ", Style::default().fg(C_ACCENT)),
                Span::styled("navigate sessions", Style::default().fg(C_TEXT_DIM)),
            ]),
            Line::from(vec![
                Span::styled("Enter ", Style::default().fg(C_ACCENT)),
                Span::styled("open detail pane", Style::default().fg(C_TEXT_DIM)),
            ]),
            Line::from(vec![
                Span::styled("c     ", Style::default().fg(C_ACCENT)),
                Span::styled(
                    "open Claude in worktree",
                    Style::default().fg(C_TEXT_DIM),
                ),
            ]),
            Line::from(vec![
                Span::styled("/     ", Style::default().fg(C_ACCENT)),
                Span::styled("search/filter", Style::default().fg(C_TEXT_DIM)),
            ]),
            Line::from(vec![
                Span::styled("?     ", Style::default().fg(C_ACCENT)),
                Span::styled(
                    "show all keybindings",
                    Style::default().fg(C_TEXT_DIM),
                ),
            ]),
        ]
    };

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(C_BG)),
        inner,
    );
}

// ─── Status bar ───────────────────────────────────────────────────────────

fn render_status_bar(app: &App, frame: &mut Frame, area: Rect) {
    let (label, label_bg) = match app.key_mode {
        KeyMode::Normal => ("INBOX", C_ACCENT),
        KeyMode::Detail => ("DETAIL", C_MAGENTA),
        KeyMode::PanePrefix => ("PANE", C_ORANGE),
        KeyMode::Terminal => ("TERM", C_GREEN),
    };

    let total_unread: usize = app.sessions.values().map(|s| s.unread_count()).sum();
    let notif = app
        .notifications
        .first()
        .map(|s| truncate_str(s, 40))
        .unwrap_or("");

    let user_label = if app.username.is_empty() {
        String::new()
    } else {
        format!("{} (gh)", app.username)
    };

    let bar = Line::from(vec![
        Span::styled(
            format!(" {label} "),
            Style::default()
                .bg(label_bg)
                .fg(Color::Rgb(15, 17, 23))
                .bold(),
        ),
        Span::raw(" "),
        if app.status.starts_with("Error") {
            Span::styled(
                format!("{} ", app.status),
                Style::default().fg(C_RED).bold(),
            )
        } else {
            Span::styled(
                format!("{} ", app.status),
                Style::default().fg(C_TEXT_DIM),
            )
        },
        if !user_label.is_empty() {
            Span::styled(
                format!("{user_label}  "),
                Style::default().fg(C_TEXT_DIM),
            )
        } else {
            Span::raw("")
        },
        if !app.tab_order.is_empty() {
            Span::styled(
                format!("{} tabs  ", app.tab_order.len()),
                Style::default().fg(C_TEXT_DIM),
            )
        } else {
            Span::raw("")
        },
        if !notif.is_empty() {
            Span::styled(notif, Style::default().fg(C_TEXT_DIM))
        } else {
            Span::raw("")
        },
        // Push unread count to the right conceptually (just append).
        if total_unread > 0 {
            Span::styled(
                format!("  * {total_unread} unread"),
                Style::default().fg(C_RED),
            )
        } else {
            Span::raw("")
        },
        if !app.credentials_ok {
            Span::styled(
                "  !! No GitHub credentials",
                Style::default().fg(C_RED).bold(),
            )
        } else {
            Span::raw("")
        },
    ]);

    frame.render_widget(
        Paragraph::new(bar).style(Style::default().bg(C_BG_ALT)),
        area,
    );
}

/// Render terminal content using whatever backend is compiled.
fn render_term_content(term: &mut pilot_tui_term::TermSession, frame: &mut Frame, area: Rect) {
    pilot_tui_term::render_to_frame(term, frame, area);
}

// ─── MCP Confirmation Modal ──────────────────────────────────────────────

fn render_mcp_confirmation(frame: &mut Frame, area: Rect, action: &PendingMcpAction) {
    let modal_w = 60u16.min(area.width.saturating_sub(4));
    let modal_h = 8u16;
    let x = (area.width.saturating_sub(modal_w)) / 2;
    let y = (area.height.saturating_sub(modal_h)) / 2;
    let modal_area = Rect::new(x, y, modal_w, modal_h);

    frame.render_widget(Clear, modal_area);

    let icon = match action.tool.as_str() {
        "pilot_push" => "^",
        "pilot_reply" => ">",
        "pilot_merge" => "M",
        "pilot_approve" => "+",
        "pilot_resolve_thread" => "v",
        "pilot_request_changes" => "x",
        _ => "!",
    };

    let lines = vec![
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                format!("  {icon} Claude wants to: "),
                Style::default().fg(C_TEXT_BRIGHT).bold(),
            ),
            Span::styled(&action.tool, Style::default().fg(C_ACCENT).bold()),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            format!(
                "  {}",
                truncate_str(&action.display, modal_w as usize - 6)
            ),
            Style::default().fg(C_TEXT),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  y/Enter", Style::default().fg(C_GREEN).bold()),
            Span::styled(" approve   ", Style::default().fg(C_TEXT_DIM)),
            Span::styled("n/Esc", Style::default().fg(C_RED).bold()),
            Span::styled(" reject", Style::default().fg(C_TEXT_DIM)),
        ]),
    ];

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            " Confirm Action ",
            Style::default().fg(C_YELLOW).bold(),
        ))
        .border_style(Style::default().fg(C_YELLOW))
        .style(Style::default().bg(C_BG_ALT));

    frame.render_widget(Paragraph::new(lines).block(block), modal_area);
}

// ─── Picker Overlay ──────────────────────────────────────────────────────

fn render_new_session_overlay(frame: &mut Frame, area: Rect, input: &str) {
    let modal_w = 50u16.min(area.width.saturating_sub(4));
    let modal_h = 7u16;
    let x = (area.width.saturating_sub(modal_w)) / 2;
    let y = (area.height.saturating_sub(modal_h)) / 2;
    let modal_area = Rect::new(x, y, modal_w, modal_h);

    frame.render_widget(Clear, modal_area);

    let display_text = if input.is_empty() {
        "type a description...".to_string()
    } else {
        format!("{input}|")
    };

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "  Description:",
            Style::default().fg(C_TEXT_DIM),
        )),
        Line::from(Span::styled(
            format!("  {display_text}"),
            if input.is_empty() {
                Style::default().fg(C_TEXT_DIM).italic()
            } else {
                Style::default().fg(C_TEXT_BRIGHT)
            },
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  Enter", Style::default().fg(C_GREEN).bold()),
            Span::styled(" create  ", Style::default().fg(C_TEXT_DIM)),
            Span::styled("Esc", Style::default().fg(C_RED).bold()),
            Span::styled(" cancel", Style::default().fg(C_TEXT_DIM)),
        ]),
    ];

    let block = Block::bordered()
        .title(Span::styled(
            " New Session ",
            Style::default().fg(C_ACCENT).bold(),
        ))
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_ACCENT))
        .style(Style::default().bg(C_BG_ALT));

    frame.render_widget(Paragraph::new(lines).block(block), modal_area);
}

fn render_quick_reply_overlay(frame: &mut Frame, area: Rect, input: &str) {
    let modal_w = 60u16.min(area.width.saturating_sub(4));
    let modal_h = 7u16;
    let x = (area.width.saturating_sub(modal_w)) / 2;
    let y = (area.height.saturating_sub(modal_h)) / 2;
    let modal_area = Rect::new(x, y, modal_w, modal_h);

    frame.render_widget(Clear, modal_area);

    let display_text = if input.is_empty() {
        "type your reply...".to_string()
    } else {
        format!("{input}|")
    };

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "  Comment:",
            Style::default().fg(C_TEXT_DIM),
        )),
        Line::from(Span::styled(
            format!("  {display_text}"),
            if input.is_empty() {
                Style::default().fg(C_TEXT_DIM).italic()
            } else {
                Style::default().fg(C_TEXT_BRIGHT)
            },
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  Enter", Style::default().fg(C_GREEN).bold()),
            Span::styled(" post  ", Style::default().fg(C_TEXT_DIM)),
            Span::styled("Esc", Style::default().fg(C_RED).bold()),
            Span::styled(" cancel", Style::default().fg(C_TEXT_DIM)),
        ]),
    ];

    let block = Block::bordered()
        .title(Span::styled(
            " Quick Reply ",
            Style::default().fg(C_ACCENT).bold(),
        ))
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_ACCENT))
        .style(Style::default().bg(C_BG_ALT));

    frame.render_widget(Paragraph::new(lines).block(block), modal_area);
}

fn render_picker_overlay(frame: &mut Frame, area: Rect, picker: &PickerState) {
    let title = match picker.kind {
        crate::action::PickerKind::Reviewer => " Edit Reviewers ",
        crate::action::PickerKind::Assignee => " Edit Assignees ",
    };

    let filtered = picker.filtered_indices();
    let item_count = filtered.len();
    let modal_h = (item_count as u16 + 6)
        .min(area.height.saturating_sub(4))
        .max(8);
    let modal_w = 44u16.min(area.width.saturating_sub(4));
    let x = (area.width.saturating_sub(modal_w)) / 2;
    let y = (area.height.saturating_sub(modal_h)) / 2;
    let modal_area = Rect::new(x, y, modal_w, modal_h);

    frame.render_widget(Clear, modal_area);

    let mut lines: Vec<Line> = Vec::new();

    // Filter input line.
    let filter_display = if picker.filter.is_empty() {
        "  type to filter".to_string()
    } else {
        format!("  /{}", picker.filter)
    };
    lines.push(Line::from(Span::styled(
        filter_display,
        Style::default().fg(C_TEXT_DIM).italic(),
    )));
    lines.push(Line::raw(""));

    // Items.
    for (display_idx, &real_idx) in filtered.iter().enumerate() {
        let item = &picker.items[real_idx];
        let is_cursor = display_idx == picker.cursor;
        let checkbox = if item.selected { "[x]" } else { "[ ]" };
        let cursor_mark = if is_cursor { ">" } else { " " };
        let bg = if is_cursor { C_BG_SELECTED } else { C_BG_ALT };
        let fg = if item.selected { C_GREEN } else { C_TEXT };

        lines.push(Line::from(vec![
            Span::styled(
                format!("  {cursor_mark} {checkbox} "),
                Style::default().fg(fg).bg(bg),
            ),
            Span::styled(
                item.login.clone(),
                Style::default().fg(fg).bold().bg(bg),
            ),
        ]));
    }

    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No matches",
            Style::default().fg(C_TEXT_DIM).italic(),
        )));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("  Space", Style::default().fg(C_ACCENT).bold()),
        Span::styled(" toggle  ", Style::default().fg(C_TEXT_DIM)),
        Span::styled("Enter", Style::default().fg(C_GREEN).bold()),
        Span::styled(" confirm  ", Style::default().fg(C_TEXT_DIM)),
        Span::styled("Esc", Style::default().fg(C_RED).bold()),
        Span::styled(" cancel", Style::default().fg(C_TEXT_DIM)),
    ]));

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(title, Style::default().fg(C_ACCENT).bold()))
        .border_style(Style::default().fg(C_ACCENT))
        .style(Style::default().bg(C_BG_ALT));

    frame.render_widget(Paragraph::new(lines).block(block), modal_area);
}

// ─── Helpers ──────────────────────────────────────────────────────────────

/// Short time-ago string without "ago" suffix: "2h", "3d", "1mo", "now".
fn time_ago_short(dt: &chrono::DateTime<chrono::Utc>) -> String {
    let now = chrono::Utc::now();
    let diff = now.signed_duration_since(dt);
    let secs = diff.num_seconds();
    if secs < 60 {
        return "now".to_string();
    }
    let mins = diff.num_minutes();
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = diff.num_hours();
    if hours < 24 {
        return format!("{hours}h");
    }
    let days = diff.num_days();
    if days < 30 {
        return format!("{days}d");
    }
    let months = days / 30;
    if months < 12 {
        return format!("{months}mo");
    }
    let years = days / 365;
    format!("{years}y")
}

/// Hash a PR number string to a distinctive color for visual identification.
fn pr_number_color(pr_num: &str) -> Color {
    const PALETTE: &[Color] = &[
        Color::Rgb(139, 148, 158),  // gray-blue
        Color::Rgb(121, 192, 255),  // light blue
        Color::Rgb(188, 140, 255),  // purple
        Color::Rgb(63, 185, 208),   // teal
        Color::Rgb(210, 153, 34),   // amber
        Color::Rgb(219, 171, 9),    // gold
        Color::Rgb(163, 190, 140),  // sage green
        Color::Rgb(235, 203, 139),  // sand
        Color::Rgb(180, 142, 173),  // mauve
        Color::Rgb(143, 188, 187),  // seafoam
    ];
    let hash: u32 = pr_num.bytes().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    PALETTE[(hash as usize) % PALETTE.len()]
}

/// Render markdown to styled lines, applying our dark theme colors.
fn render_markdown(input: &str) -> Vec<Line<'static>> {
    let md = tui_markdown::from_str(input);
    md.lines
        .into_iter()
        .map(|line| {
            let styled_spans: Vec<Span<'static>> = line
                .spans
                .into_iter()
                .map(|span| {
                    let mut style = span.style;
                    // Force foreground to be visible on dark bg.
                    let fg = style.fg.unwrap_or(Color::Reset);
                    if matches!(fg, Color::Reset | Color::Black) {
                        style = style.fg(C_TEXT);
                    }
                    // Make bold text bright.
                    if style.add_modifier.contains(Modifier::BOLD) {
                        if style.fg == Some(C_TEXT) {
                            style = style.fg(C_TEXT_BRIGHT);
                        }
                    }
                    // Style code spans distinctively.
                    if span.style.bg.is_some() {
                        style = style.bg(C_BG_ALT).fg(C_CYAN);
                    }
                    // Own the string to get 'static lifetime.
                    Span::styled(span.content.to_string(), style)
                })
                .collect();
            Line::from(styled_spans)
        })
        .collect()
}

/// Hash a label name to a colored background for pill rendering.
fn label_pill_color(label: &str) -> Color {
    const LABEL_COLORS: &[Color] = &[
        Color::Rgb(163, 113, 247),  // purple
        Color::Rgb(56, 132, 255),   // blue
        Color::Rgb(63, 185, 80),    // green
        Color::Rgb(210, 153, 34),   // yellow
        Color::Rgb(248, 81, 73),    // red
        Color::Rgb(63, 185, 208),   // teal
        Color::Rgb(219, 171, 9),    // amber
        Color::Rgb(188, 140, 255),  // light purple
        Color::Rgb(139, 148, 158),  // gray
        Color::Rgb(235, 203, 139),  // sand
    ];
    let hash: u32 = label.bytes().fold(0u32, |acc, b| acc.wrapping_mul(37).wrapping_add(b as u32));
    LABEL_COLORS[(hash as usize) % LABEL_COLORS.len()]
}

fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

/// Build an action bar Line from (short, label) hint pairs.
fn build_action_bar(hints: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::styled(" ", Style::default())];
    for (short, label) in hints {
        spans.push(Span::styled(
            short.to_string(),
            Style::default().fg(C_ACCENT).bold(),
        ));
        spans.push(Span::styled(
            format!(" {label}  "),
            Style::default().fg(C_TEXT_DIM),
        ));
    }
    Line::from(spans)
}

/// Thin horizontal separator line.
fn thin_separator(width: usize) -> Line<'static> {
    let fill: String = "\u{2500}".repeat(width);
    Line::from(Span::styled(fill, Style::default().fg(C_BORDER)))
}

fn session_color(c: SessionColor) -> Color {
    match c {
        SessionColor::Blue => C_ACCENT,
        SessionColor::Green => C_GREEN,
        SessionColor::Yellow => C_YELLOW,
        SessionColor::Red => C_RED,
        SessionColor::Magenta => C_MAGENTA,
        SessionColor::Cyan => C_CYAN,
        SessionColor::Orange => C_ORANGE,
        SessionColor::Purple => Color::Rgb(160, 80, 240),
    }
}

/// Render a state label as a colored pill.
fn state_pill(s: &TaskState) -> Span<'static> {
    match s {
        TaskState::Open => Span::styled(
            " OPEN ",
            Style::default()
                .fg(Color::Rgb(15, 17, 23))
                .bg(C_GREEN)
                .bold(),
        ),
        TaskState::InProgress => Span::styled(
            " WIP ",
            Style::default()
                .fg(Color::Rgb(15, 17, 23))
                .bg(C_YELLOW)
                .bold(),
        ),
        TaskState::InReview => Span::styled(
            " REVIEW ",
            Style::default()
                .fg(Color::Rgb(15, 17, 23))
                .bg(C_CYAN)
                .bold(),
        ),
        TaskState::Closed => Span::styled(
            " CLOSED ",
            Style::default()
                .fg(Color::Rgb(15, 17, 23))
                .bg(C_RED)
                .bold(),
        ),
        TaskState::Merged => Span::styled(
            " MERGED ",
            Style::default()
                .fg(Color::Rgb(15, 17, 23))
                .bg(C_MAGENTA)
                .bold(),
        ),
        TaskState::Draft => Span::styled(
            " DRAFT ",
            Style::default()
                .fg(Color::Rgb(15, 17, 23))
                .bg(C_TEXT_DIM)
                .bold(),
        ),
    }
}

fn ci_span(ci: &CiStatus) -> Span<'static> {
    match ci {
        CiStatus::Success => Span::styled("CI passing", Style::default().fg(C_GREEN)),
        CiStatus::Failure => Span::styled("CI failed", Style::default().fg(C_RED).bold()),
        CiStatus::Running => Span::styled("CI running", Style::default().fg(C_YELLOW)),
        CiStatus::Pending => Span::styled("CI pending", Style::default().fg(C_YELLOW)),
        CiStatus::Mixed => Span::styled("CI mixed", Style::default().fg(C_YELLOW)),
        CiStatus::None => Span::styled("CI --", Style::default().fg(C_TEXT_DIM)),
    }
}

fn review_span(r: &ReviewStatus) -> Span<'static> {
    match r {
        ReviewStatus::Approved => Span::styled("Approved", Style::default().fg(C_GREEN)),
        ReviewStatus::ChangesRequested => {
            Span::styled("Changes requested", Style::default().fg(C_ORANGE).bold())
        }
        ReviewStatus::Pending => Span::styled("Review pending", Style::default().fg(C_YELLOW)),
        ReviewStatus::None => Span::raw(""),
    }
}

fn role_span(r: &TaskRole) -> Span<'static> {
    match r {
        TaskRole::Author => Span::styled("Author", Style::default().fg(C_CYAN)),
        TaskRole::Reviewer => Span::styled("Reviewer", Style::default().fg(C_MAGENTA)),
        TaskRole::Assignee => Span::styled("Assignee", Style::default().fg(C_GREEN)),
        TaskRole::Mentioned => Span::styled("Mentioned", Style::default().fg(C_TEXT_DIM)),
    }
}
