use pilot_core::time::{self, Staleness};
use pilot_core::{ActionPriority, CiStatus, ReviewStatus, TaskRole, TaskState};
use ratatui::prelude::*;
use ratatui::widgets::*;

use crate::app::App;
use crate::input::InputMode;
use crate::nav::{NavItem, nav_items, build_repo_groups};
use crate::picker::PickerState;
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
    // Sample wall time ONCE per render and thread it through every sub-
    // function. Avoids per-row `Utc::now()` syscalls (dozens of them for a
    // sidebar with many sessions) and keeps all "N ago" columns consistent
    // within a single frame.
    let now = chrono::Utc::now();
    // Fill the entire background first.
    frame.render_widget(
        Block::default().style(Style::default().bg(C_BG)),
        frame.area(),
    );

    let outer = Layout::vertical([
        Constraint::Length(if app.terminals.tab_order().is_empty() { 0 } else { 1 }),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(frame.area());

    if !app.terminals.tab_order().is_empty() {
        render_tab_bar(app, frame, outer[0]);
    }
    render_main(app, frame, outer[1], now);
    render_status_bar(app, frame, outer[2]);

    // Picker overlay.
    if let Some(ref picker) = app.state.picker {
        render_picker_overlay(frame, frame.area(), picker);
    }

    // New session overlay.
    if let Some(ref input) = app.state.new_session_input {
        render_new_session_overlay(frame, frame.area(), input);
    }

    // Quick reply overlay.
    if let Some((ref _key, ref text, _)) = app.state.quick_reply_input {
        render_quick_reply_overlay(frame, frame.area(), text);
    }

    // Help overlay.
    if app.state.show_help {
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

    for (category, items) in groups {
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
        .terminals
        .tab_order()
        .iter()
        .enumerate()
        .flat_map(|(i, key)| {
            let is_active = i == app.terminals.active_tab();
            let session = app.state.sessions.get(key);
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

fn render_main(app: &mut App, frame: &mut Frame, area: Rect, now: chrono::DateTime<chrono::Utc>) {
    let pct = app.state.sidebar_pct.clamp(20, 80);
    let chunks = Layout::horizontal([
        Constraint::Percentage(pct),
        Constraint::Percentage(100 - pct),
    ])
    .split(area);

    render_sidebar(app, frame, chunks[0], now);
    render_right_pane(app, frame, chunks[1], now);
}

// ─── Sidebar (borderless, clean table-like list) ──────────────────────────

fn render_sidebar(app: &App, frame: &mut Frame, area: Rect, now: chrono::DateTime<chrono::Utc>) {
    // No border for the sidebar — use the full area.
    let total_unread: usize = app.state.sessions.values().map(|s| s.unread_count()).sum();
    let time_label = match app.state.activity_days_filter {
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
    let asking_count = app
        .state
        .agent_states
        .iter()
        .filter(|(_, s)| matches!(s, crate::agent_state::AgentState::Asking))
        .count();
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
    if asking_count > 0 {
        // Blink red↔yellow every ~400ms so it pulls the eye.
        let tick = app.state.tick_count;
        let col = if tick.is_multiple_of(8) || (tick % 8) >= 4 {
            C_RED
        } else {
            C_YELLOW
        };
        header_spans.push(Span::styled(
            format!("  ? {asking_count} input (/)"),
            Style::default().fg(col).bold(),
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
    let search_style = if app.state.search_active {
        Style::default().fg(C_TEXT_BRIGHT).bg(C_BG_HOVER)
    } else {
        Style::default().fg(C_TEXT_DIM)
    };
    let cursor = if app.state.search_active { "|" } else { "" };
    let search_text = if app.state.search_active || !app.state.search_query.is_empty() {
        format!("  s {}{cursor}", app.state.search_query)
    } else {
        "  s  filter (needs:reply ci:failed ...)".into()
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
    if !app.state.loaded {
        let spinner = [
            "   ", ".  ", ".. ", "...", " ..", "  .", "   ", ".  ", ".. ", "...",
        ];
        let s = spinner[(app.state.tick_count as usize / 2) % spinner.len()];
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
    if app.state.loaded && session_count == 0 {
        let reason = if !app.state.search_query.is_empty() {
            format!("  No matches for /{}", app.state.search_query)
        } else if app.state.activity_days_filter > 0 {
            format!("  No PRs active in last {}d", app.state.activity_days_filter)
        } else if app.state.config.providers.github.filters.is_empty() {
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
        if app.state.activity_days_filter > 0 {
            lines.push(Line::from(Span::styled(
                "  Press 't' to widen the time filter.",
                Style::default().fg(C_TEXT_DIM),
            )));
        }
    }

    // ── Priority summary bar (compact triage counts) ──
    if app.state.loaded && session_count > 0 {
        let render_now = now;
        let mut counts = std::collections::HashMap::<ActionPriority, usize>::new();
        for it in &items {
            if let NavItem::Session(k) = it
                && let Some(s) = app.state.sessions.get(k)
            {
                *counts
                    .entry(s.action_priority(&app.state.username, render_now))
                    .or_insert(0) += 1;
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

    // Column header — matches the fixed-width layout below.
    // Left(9) + Title(flex) + Right(20) = w
    if app.state.loaded && session_count > 0 {
        let title_w = w.saturating_sub(9 + 20);
        let header_title = format!("{:<width$}", "Title", width = title_w);
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<5}  ", "#"), Style::default().fg(C_TEXT_DIM)),
            Span::styled(header_title, Style::default().fg(C_TEXT_DIM)),
            Span::styled("        Status  Time", Style::default().fg(C_TEXT_DIM)),
        ]));
    }

    // Pre-compute repo stats.
    let repo_stats: std::collections::HashMap<&str, (usize, usize)> = repos
        .iter()
        .map(|(repo, keys)| {
            let unread: usize = keys
                .iter()
                .filter_map(|k| app.state.sessions.get(k))
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

    // Pre-compute which sessions are stacked on another open session.
    // A PR is stacked when its base branch matches another PR's head
    // branch in the same repo — i.e. it can't merge until the parent does.
    let stacked = crate::nav::compute_stacked_sessions(&app.state);
    // Also compute which sessions ARE parents (something stacks on them)
    // so we can mark the base of a stack too.
    let is_parent: std::collections::HashSet<&String> =
        stacked.values().collect();

    // ── Render each nav item ──
    for (nav_idx, item) in items.iter().enumerate() {
        let is_cursor = nav_idx == app.state.selected;

        match item {
            NavItem::Repo(repo) => {
                let collapsed = app.state.collapsed_repos.contains(repo);
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
                let Some(session) = app.state.sessions.get(key) else { continue };
                let task = &session.primary_task;
                let unread = session.unread_count();
                let bg = if is_cursor { C_BG_SELECTED } else { C_BG };

                // ── Status label ──
                let tag = pilot_core::StatusTag::for_task(task);
                let (label_text, mut label_fg, label_bg) = status_tag_colors(tag, bg);
                // Slow blink for CI-running so it visibly separates
                // from static statuses when glancing down the list.
                // Cycle every ~1.6s (16 ticks at 100ms).
                if matches!(tag, pilot_core::StatusTag::CiRunning) {
                    let tick = app.state.tick_count;
                    label_fg = if (tick % 16) < 8 {
                        C_YELLOW
                    } else {
                        C_TEXT_DIM
                    };
                }

                // ── PR number ──
                let pr_num = task.id.key.rsplit_once('#')
                    .map(|(_, n)| format!("#{n}"))
                    .unwrap_or_default();
                let pr_color = pr_number_color(&pr_num);

                // ── Title style ──
                let title_style = if is_cursor {
                    Style::default().fg(C_TEXT_BRIGHT).bold().bg(bg)
                } else if unread > 0 {
                    Style::default().fg(C_TEXT_BRIGHT).bg(bg)
                } else {
                    Style::default().fg(C_TEXT).bg(bg)
                };

                // ── Time ──
                let time_str = time_ago_short(&task.updated_at, now);

                // ── Session state indicator ──
                // ●  (green) pilot has an active terminal tab for this session
                // ⚡ (yellow) tmux session is alive on disk; press `c` to attach
                // (blank) no session running
                let tmux_name = key.replace([':', '/'], "_");
                let has_live_terminal = app.terminals.contains_key(key);
                let tmux_alive_detached =
                    app.state.live_tmux_sessions.contains(&tmux_name) && !has_live_terminal;

                // ── Build right side FIRST with fixed total width ──
                // unread(4) + status(10) + time(6) = 20 chars always
                const RIGHT_W: usize = 20;
                let unread_str = if unread > 0 { format!("*{unread}") } else { String::new() };
                let status_str = if label_text.is_empty() {
                    String::new()
                } else {
                    format!(" {label_text} ")
                };
                let time_part = format!("{:>5}", time_str);

                // ── Left side: cursor(2) + pr#(5) + role(2) + tmux(2) = 11 ──
                const LEFT_W: usize = 11;

                // ── Stack indicator ──
                // `↳` = stacked on another PR (can't merge until parent does).
                // `⇡` = another PR is stacked on top of this one (the base).
                // Space otherwise. Prefixed to the title so it nests visually.
                let stack_prefix: &str = if stacked.contains_key(key) {
                    "↳ "
                } else if is_parent.contains(key) {
                    "⇡ "
                } else {
                    ""
                };

                // ── Title fills the gap ──
                let title_w = w.saturating_sub(LEFT_W + RIGHT_W);
                let prefix_w = stack_prefix.chars().count();
                let title_budget = title_w.saturating_sub(prefix_w);
                let title_text = truncate_str(&task.title, title_budget);
                let padded_title = format!(
                    "{stack_prefix}{title_text:<width$}",
                    width = title_budget
                );

                // Build right-side spans with fixed widths.
                let unread_span = Span::styled(
                    format!("{:>4}", unread_str),
                    Style::default().fg(if unread > 0 { C_RED } else { bg }).bg(bg),
                );
                let status_span = Span::styled(
                    format!("{:>10}", status_str),
                    Style::default().fg(label_fg).bg(label_bg).bold(),
                );
                let time_span = Span::styled(
                    format!(" {time_part}"),
                    Style::default().fg(C_TEXT_DIM).bg(bg),
                );

                let row = Line::from(vec![
                    // Cursor accent (2 chars)
                    Span::styled(
                        if is_cursor { "\u{258c} " } else { "  " },
                        if is_cursor { Style::default().fg(C_ACCENT).bg(bg) } else { Style::default().bg(bg) },
                    ),
                    // PR# (5 chars)
                    Span::styled(format!("{pr_num:<5}"), Style::default().fg(pr_color).bg(bg)),
                    // Role indicator (2 chars)
                    match task.role {
                        TaskRole::Author => Span::styled("@ ", Style::default().fg(C_CYAN).bg(bg)),
                        TaskRole::Reviewer => Span::styled("R ", Style::default().fg(C_MAGENTA).bg(bg)),
                        TaskRole::Assignee => Span::styled("A ", Style::default().fg(C_GREEN).bg(bg)),
                        TaskRole::Mentioned => Span::styled("  ", Style::default().bg(bg)),
                    },
                    // Session indicator (2 chars — glyph + trailing space).
                    // Encodes three orthogonal states in one cell:
                    //   - no terminal at all (blank)
                    //   - tmux alive, detached (●  yellow — "c to attach")
                    //   - terminal attached + Claude Active (braille spinner, cyan)
                    //   - terminal attached + Claude Idle   (●  green)
                    //   - terminal attached + Claude Asking (blinking ? red/yellow)
                    // Single-cell glyphs keep column alignment predictable.
                    {
                        use crate::agent_state::AgentState;
                        let claude_state = app.state.agent_states.get(key).copied();
                        let tick = app.state.tick_count;

                        let (ch, color) = if has_live_terminal {
                            match claude_state {
                                Some(AgentState::Asking) => {
                                    // Blink between bright red and yellow
                                    // every ~400ms so it catches the eye.
                                    let col = if tick.is_multiple_of(8) || (tick % 8) >= 4 {
                                        C_RED
                                    } else {
                                        C_YELLOW
                                    };
                                    ("?", col)
                                }
                                Some(AgentState::Active) => {
                                    // Braille spinner — tight 10-frame cycle
                                    // at 100ms per tick = 1s full rotation.
                                    const FRAMES: [&str; 10] = [
                                        "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}",
                                        "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
                                        "\u{2807}", "\u{280f}",
                                    ];
                                    let f = FRAMES[(tick as usize) % FRAMES.len()];
                                    (f, C_CYAN)
                                }
                                _ => ("●", C_GREEN),
                            }
                        } else if tmux_alive_detached {
                            ("●", C_YELLOW)
                        } else {
                            (" ", C_TEXT_DIM)
                        };
                        Span::styled(
                            format!("{ch} "),
                            Style::default().fg(color).bg(bg),
                        )
                    },
                    // Title (padded to fill)
                    Span::styled(padded_title, title_style),
                    // Right side: unread(4) + status(10) + time(6) = 20
                    unread_span,
                    status_span,
                    time_span,
                ]);

                lines.push(row);
            }
        }
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(C_BG)),
        list_area,
    );

    // ── Action bar (bottom) — contextual based on selected PR ──
    let hints = if let Some(key) = app.selected_session_key() {
        if let Some(session) = app.state.sessions.get(&key) {
            let task = &session.primary_task;
            let ci_ok = matches!(task.ci, CiStatus::Success | CiStatus::None);
            let review_ok = task.review == ReviewStatus::Approved;
            crate::keymap::contextual_hints(
                crate::keys::KeyMode::Normal,
                &crate::keymap::HintContext {
                    ci: task.ci,
                    review: task.review,
                    role: task.role,
                    has_conflicts: task.has_conflicts,
                    has_unread: session.unread_count() > 0,
                    is_ready: ci_ok && review_ok && !task.has_conflicts,
                    has_terminal: app.terminals.contains_key(&key),
                    needs_reply: task.needs_reply,
                    auto_merge_enabled: task.auto_merge_enabled,
                    has_reviewers: !task.reviewers.is_empty() || !task.assignees.is_empty(),
                },
            )
        } else {
            crate::keymap::action_bar_for_mode(crate::keys::KeyMode::Normal)
        }
    } else {
        crate::keymap::action_bar_for_mode(crate::keys::KeyMode::Normal)
    };
    let action_bar = build_action_bar(&hints);
    frame.render_widget(
        Paragraph::new(action_bar).style(Style::default().bg(C_BG_ALT)),
        action_area,
    );
}

// ─── Right pane (detail or terminal or both) ───────────────────────────────

fn render_right_pane(
    app: &mut App,
    frame: &mut Frame,
    area: Rect,
    now: chrono::DateTime<chrono::Utc>,
) {
    // Both panes follow the sidebar cursor: if you j/k over a session,
    // the right side swaps to that session's header/comments/terminal.
    // The pane tree's Terminal leaf is kept in sync by update_detail_pane
    // whenever `selected` moves.
    let detail_key = app.selected_session_key();
    let term_key = detail_key
        .as_ref()
        .filter(|k| app.terminals.contains_key(k.as_str()))
        .cloned();
    // True when the selected session is mid-worktree-checkout (git clone /
    // fetch / worktree add in flight). Checkout can take several seconds
    // the first time a repo is opened, so we render a spinner in the
    // terminal slot instead of leaving a silent black hole.
    let checking_out = detail_key
        .as_ref()
        .and_then(|k| app.state.sessions.get(k))
        .map(|s| matches!(s.state, pilot_core::SessionState::CheckingOut))
        .unwrap_or(false);

    // Hide Detail when the selected PR has no comments — the header
    // alone isn't worth screen real estate, and the user wants Claude
    // Code to dominate the right side.
    let has_comments = detail_key
        .as_ref()
        .and_then(|k| app.state.sessions.get(k))
        .map(|s| !s.activity.is_empty())
        .unwrap_or(false);

    match (term_key, detail_key) {
        (Some(term), Some(detail)) if has_comments => {
            // Three-way stack: header (PR metadata), comments, then
            // Claude Code at the bottom.
            let header_h = detail_header_height(&app.state, &detail);
            let chunks = Layout::vertical([
                Constraint::Length(header_h),
                Constraint::Percentage(30), // comments
                Constraint::Min(8),         // terminal (bottom)
            ])
            .split(area);
            render_detail_header(app, frame, chunks[0], &detail, now);
            render_comments(app, frame, chunks[1], &detail, now);
            render_terminal(app, frame, chunks[2], &term);
        }
        (Some(term), Some(detail)) => {
            let header_h = detail_header_height(&app.state, &detail);
            let chunks = Layout::vertical([
                Constraint::Length(header_h),
                Constraint::Min(8),
            ])
            .split(area);
            render_detail_header(app, frame, chunks[0], &detail, now);
            render_terminal(app, frame, chunks[1], &term);
        }
        (Some(term), None) => {
            render_terminal(app, frame, area, &term);
        }
        // Checkout in progress → show header + spinner where the
        // terminal will appear. Give the user visible feedback that
        // something is happening; `f` can take 5–10s on a cold repo.
        (None, Some(detail)) if checking_out => {
            let header_h = detail_header_height(&app.state, &detail);
            if has_comments {
                let chunks = Layout::vertical([
                    Constraint::Length(header_h),
                    Constraint::Percentage(30),
                    Constraint::Min(5),
                ])
                .split(area);
                render_detail_header(app, frame, chunks[0], &detail, now);
                render_comments(app, frame, chunks[1], &detail, now);
                render_checking_out(app, frame, chunks[2]);
            } else {
                let chunks = Layout::vertical([
                    Constraint::Length(header_h),
                    Constraint::Min(5),
                ])
                .split(area);
                render_detail_header(app, frame, chunks[0], &detail, now);
                render_checking_out(app, frame, chunks[1]);
            }
        }
        (None, Some(detail)) => {
            render_detail(app, frame, area, &detail, now);
        }
        (None, None) => {
            render_welcome(app, frame, area);
        }
    }
}

/// Draw a centered "checking out worktree…" spinner in `area`. Used
/// while the async CheckoutWorktree command is running.
fn render_checking_out(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::LEFT | Borders::TOP)
        .border_style(Style::default().fg(C_BORDER))
        .style(Style::default().bg(C_BG))
        .title(Span::styled(
            " Preparing worktree ",
            Style::default().fg(C_YELLOW).bold(),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Braille spinner — one full rotation per second at 100ms tick.
    const FRAMES: [&str; 10] = [
        "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}",
        "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
        "\u{2807}", "\u{280f}",
    ];
    let f = FRAMES[(app.state.tick_count as usize) % FRAMES.len()];
    let msg = format!(" {f}  Checking out worktree…");
    let hint = "    This takes a few seconds on first open.";
    let lines = vec![
        Line::raw(""),
        Line::raw(""),
        Line::from(Span::styled(msg, Style::default().fg(C_YELLOW).bold())),
        Line::raw(""),
        Line::from(Span::styled(hint, Style::default().fg(C_TEXT_DIM))),
    ];
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(C_BG)),
        inner,
    );
}

/// Computed height in rows for the PR-header pane. Depends on whether
/// this session has labels or an active monitor — they each add a row.
fn detail_header_height(state: &crate::state::State, key: &str) -> u16 {
    let Some(session) = state.sessions.get(key) else {
        return 5;
    };
    let task = &session.primary_task;
    // Block's padding adds 1 top line; then title + separator + status
    // + reviewers = 4. Labels and monitor are optional.
    let mut h: u16 = 1 + 4;
    if !task.labels.is_empty() {
        h += 1;
    }
    if session.monitor.is_some() {
        h += 1;
    }
    h
}

// ─── Detail header pane (PR metadata only — no comments) ──────────────────

fn render_detail_header(
    app: &App,
    frame: &mut Frame,
    area: Rect,
    key: &str,
    now: chrono::DateTime<chrono::Utc>,
) {
    let Some(session) = app.state.sessions.get(key) else {
        return;
    };
    let task = &session.primary_task;
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(C_BORDER))
        .style(Style::default().bg(C_BG))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = build_header_lines(app, task, session, inner.width, now);
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(C_BG)),
        inner,
    );
}

// ─── Comments-only pane ──────────────────────────────────────────────────

fn render_comments(
    app: &App,
    frame: &mut Frame,
    area: Rect,
    key: &str,
    now: chrono::DateTime<chrono::Utc>,
) {
    let Some(session) = app.state.sessions.get(key) else {
        return;
    };
    let task = &session.primary_task;
    let is_focused = app.state.input_mode == InputMode::Detail;
    let border_color = if is_focused { C_BORDER_ACTIVE } else { C_BORDER };
    let block = Block::default()
        .borders(Borders::LEFT | Borders::TOP)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(C_BG))
        .padding(Padding::new(1, 1, 0, 0))
        .title(Span::styled(
            " Comments ",
            Style::default().fg(C_TEXT_DIM),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = build_comment_lines(app, task, session, inner.width, now, is_focused);
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((app.state.detail_scroll, 0))
            .style(Style::default().bg(C_BG)),
        inner,
    );
}

// Build the lines that make up the PR-metadata header: title+state,
// CI/review/role/branch, reviewers/assignees, labels, monitor.
fn build_header_lines<'a>(
    _app: &'a App,
    task: &'a pilot_core::Task,
    session: &'a pilot_core::Session,
    width: u16,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<Line<'a>> {
    let staleness = time::staleness(&task.updated_at, &task.updated_at, now);
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

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            state_pill(&task.state),
            Span::raw("  "),
            Span::styled(&task.title, Style::default().fg(C_TEXT_BRIGHT).bold()),
            stale_span,
            Span::styled(format!("  {ago}"), Style::default().fg(C_TEXT_DIM)),
        ]),
        thin_separator(width as usize),
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
    lines.push(Line::from(vec![
        Span::styled("Reviewers: ", Style::default().fg(C_TEXT_DIM)),
        Span::styled(reviewers_str, Style::default().fg(C_CYAN)),
        Span::raw("    "),
        Span::styled("Assignees: ", Style::default().fg(C_TEXT_DIM)),
        Span::styled(assignees_str, Style::default().fg(C_MAGENTA)),
        if task.additions > 0 || task.deletions > 0 {
            Span::styled(
                format!("    +{} -{}", task.additions, task.deletions),
                Style::default().fg(C_TEXT_DIM),
            )
        } else {
            Span::raw("")
        },
    ]));

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
        lines.push(Line::from(label_spans));
    }
    if let Some(label) = session.monitor_label() {
        lines.push(Line::from(vec![
            Span::styled(
                " MONITOR ",
                Style::default().fg(Color::Rgb(15, 17, 23)).bg(C_CYAN).bold(),
            ),
            Span::styled(format!(" {label}"), Style::default().fg(C_CYAN)),
        ]));
    }
    lines
}

// Build the lines that make up the activity/comments pane.
fn build_comment_lines<'a>(
    app: &'a App,
    task: &'a pilot_core::Task,
    session: &'a pilot_core::Session,
    width: u16,
    now: chrono::DateTime<chrono::Utc>,
    is_focused: bool,
) -> Vec<Line<'a>> {
    let comment_width = width as usize;
    let mut comment_lines: Vec<Line> = Vec::new();

    if session.activity.is_empty() {
        comment_lines.push(Line::from(Span::styled(
            "  No activity yet",
            Style::default().fg(C_TEXT_DIM).italic(),
        )));
    } else {
        let any_selected = !app.state.selected_comments.is_empty();
        for (i, a) in session.activity.iter().enumerate() {
            let is_cursor = is_focused && i == app.state.detail_cursor;
            let is_checked = app.state.selected_comments.contains(&i);
            let is_unread = session.is_activity_unread(i);
            let bg = if is_cursor { C_BG_SELECTED } else { C_BG };
            let ago_str = time_ago_short(&a.created_at, now);
            let prefix = if any_selected || is_checked {
                if is_checked { "[x] " } else { "[ ] " }
            } else if is_unread {
                " *  "
            } else {
                "    "
            };
            let prefix_color = if is_checked {
                C_GREEN
            } else if is_unread {
                C_RED
            } else {
                C_TEXT_DIM
            };
            let clean_body = strip_html(&a.body);
            let body_summary: String = clean_body
                .chars()
                .take(comment_width.saturating_sub(25))
                .collect();
            let author_color = if is_unread { C_TEXT_BRIGHT } else { C_TEXT_DIM };
            let body_color = if is_unread { C_TEXT } else { C_TEXT_DIM };
            let mut summary_spans = vec![
                Span::styled(prefix, Style::default().fg(prefix_color).bg(bg)),
                Span::styled(&a.author, Style::default().fg(author_color).bold().bg(bg)),
                Span::styled(format!(" {ago_str} "), Style::default().fg(C_TEXT_DIM).bg(bg)),
            ];
            if let Some(path) = &a.path {
                let loc = match a.line {
                    Some(n) => format!("{path}:{n} "),
                    None => format!("{path} "),
                };
                summary_spans.push(Span::styled(loc, Style::default().fg(C_ACCENT).bg(bg)));
            }
            summary_spans.push(Span::styled(body_summary, Style::default().fg(body_color).bg(bg)));
            comment_lines.push(Line::from(summary_spans));

            if is_cursor && !a.body.is_empty() {
                if let Some(hunk) = &a.diff_hunk
                    && !hunk.is_empty()
                {
                    let hunk_width = comment_width.saturating_sub(6);
                    for raw in hunk.lines().take(10) {
                        let color = match raw.chars().next() {
                            Some('+') => C_GREEN,
                            Some('-') => C_RED,
                            Some('@') => C_ACCENT,
                            _ => C_TEXT_DIM,
                        };
                        let shown: String = raw.chars().take(hunk_width).collect();
                        comment_lines.push(Line::from(vec![
                            Span::styled("    \u{2502} ", Style::default().fg(C_ACCENT).bg(bg)),
                            Span::styled(shown, Style::default().fg(color).bg(bg)),
                        ]));
                    }
                    comment_lines.push(Line::from(vec![
                        Span::styled("    \u{2502}", Style::default().fg(C_ACCENT).bg(bg)),
                    ]));
                }
                let wrap_width = comment_width.saturating_sub(6);
                let cleaned = strip_html(&a.body);
                let md_lines = render_markdown(&cleaned);
                let mut count = 0;
                for md_line in md_lines.into_iter() {
                    if count >= 15 {
                        break;
                    }
                    let text: String = md_line.spans.iter().map(|s| s.content.as_ref()).collect();
                    if text.is_empty() {
                        comment_lines.push(Line::from(Span::styled(
                            "    \u{2502}",
                            Style::default().fg(C_ACCENT).bg(bg),
                        )));
                        count += 1;
                        continue;
                    }
                    let style = md_line
                        .spans
                        .first()
                        .map(|s| s.style)
                        .unwrap_or(Style::default().fg(C_TEXT));
                    let mut remaining = text.as_str();
                    while !remaining.is_empty() && count < 15 {
                        let chunk = if remaining.len() <= wrap_width {
                            remaining
                        } else {
                            remaining[..wrap_width]
                                .rfind(' ')
                                .map(|p| &remaining[..p])
                                .unwrap_or(&remaining[..wrap_width])
                        };
                        comment_lines.push(Line::from(vec![
                            Span::styled("    \u{2502} ", Style::default().fg(C_ACCENT).bg(bg)),
                            Span::styled(chunk.to_string(), style.bg(bg)),
                        ]));
                        remaining = remaining[chunk.len()..].trim_start();
                        count += 1;
                    }
                }
                comment_lines.push(Line::raw(""));
            }
        }
    }

    let failed: Vec<_> = task
        .checks
        .iter()
        .filter(|c| matches!(c.status, CiStatus::Failure))
        .collect();
    if !failed.is_empty() {
        let total = task.checks.len();
        let passed = total - failed.len();
        comment_lines.insert(0, Line::raw(""));
        for check in failed.iter().rev().take(5) {
            comment_lines.insert(
                0,
                Line::from(vec![
                    Span::styled("    \u{2717} ", Style::default().fg(C_RED)),
                    Span::styled(check.name.clone(), Style::default().fg(C_RED)),
                ]),
            );
        }
        let summary = format!("  {} failed, {} passed", failed.len(), passed);
        comment_lines.insert(
            0,
            Line::from(Span::styled(summary, Style::default().fg(C_RED).bold())),
        );
    }
    comment_lines
}

// ─── Detail pane (PR header + selectable comment thread) ──────────────────

fn render_detail(
    app: &App,
    frame: &mut Frame,
    area: Rect,
    key: &str,
    now: chrono::DateTime<chrono::Utc>,
) {
    let Some(session) = app.state.sessions.get(key) else {
        return;
    };
    let task = &session.primary_task;
    let is_focused = app.state.input_mode == InputMode::Detail;
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
    let staleness = time::staleness(&task.updated_at, &task.updated_at, now);
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

    let mut comment_lines: Vec<Line> = Vec::new();

    // ── Activity thread — comments and reviews are the focus ──

    if session.activity.is_empty() {
        comment_lines.push(Line::from(Span::styled(
            "  No activity yet",
            Style::default().fg(C_TEXT_DIM).italic(),
        )));
    } else {
        let any_selected = !app.state.selected_comments.is_empty();
        let comment_width = chunks[1].width as usize;

        for (i, a) in session.activity.iter().enumerate() {
            let is_cursor = is_focused && i == app.state.detail_cursor;
            let is_checked = app.state.selected_comments.contains(&i);
            let is_unread = session.is_activity_unread(i);

            let bg = if is_cursor { C_BG_SELECTED } else { C_BG };

            let ago_str = time_ago_short(&a.created_at, now);

            // Prefix: checkbox or read indicator.
            let prefix = if any_selected || is_checked {
                if is_checked { "[x] " } else { "[ ] " }
            } else if is_unread {
                " *  "
            } else {
                "    "
            };
            let prefix_color = if is_checked { C_GREEN } else if is_unread { C_RED } else { C_TEXT_DIM };

            // First line of body (summary) — strip HTML/markdown noise.
            let clean_body = strip_html(&a.body);
            let body_summary: String = clean_body.chars()
                .take(comment_width.saturating_sub(25))
                .collect();

            let author_color = if is_unread { C_TEXT_BRIGHT } else { C_TEXT_DIM };
            let body_color = if is_unread { C_TEXT } else { C_TEXT_DIM };

            // Compact: one line per comment. Inline review comments append file:line.
            let mut summary_spans = vec![
                Span::styled(prefix, Style::default().fg(prefix_color).bg(bg)),
                Span::styled(&a.author, Style::default().fg(author_color).bold().bg(bg)),
                Span::styled(format!(" {ago_str} ", ), Style::default().fg(C_TEXT_DIM).bg(bg)),
            ];
            if let Some(path) = &a.path {
                let loc = match a.line {
                    Some(n) => format!("{path}:{n} "),
                    None => format!("{path} "),
                };
                summary_spans.push(Span::styled(loc, Style::default().fg(C_ACCENT).bg(bg)));
            }
            summary_spans.push(Span::styled(body_summary, Style::default().fg(body_color).bg(bg)));
            comment_lines.push(Line::from(summary_spans));

            // Expanded: if cursor is on this comment, show full body below.
            if is_cursor && !a.body.is_empty() {
                // Diff hunk — show code context first.
                if let Some(hunk) = &a.diff_hunk
                    && !hunk.is_empty() {
                        let hunk_width = comment_width.saturating_sub(6);
                        for raw in hunk.lines().take(10) {
                            let color = match raw.chars().next() {
                                Some('+') => C_GREEN,
                                Some('-') => C_RED,
                                Some('@') => C_ACCENT,
                                _ => C_TEXT_DIM,
                            };
                            let shown: String = raw.chars().take(hunk_width).collect();
                            comment_lines.push(Line::from(vec![
                                Span::styled("    \u{2502} ", Style::default().fg(C_ACCENT).bg(bg)),
                                Span::styled(shown, Style::default().fg(color).bg(bg)),
                            ]));
                        }
                        comment_lines.push(Line::from(vec![
                            Span::styled("    \u{2502}", Style::default().fg(C_ACCENT).bg(bg)),
                        ]));
                    }
                let wrap_width = comment_width.saturating_sub(6);
                let cleaned = strip_html(&a.body);
                let md_lines = render_markdown(&cleaned);
                let mut count = 0;
                for md_line in md_lines.into_iter() {
                    if count >= 15 { break; }
                    let text: String = md_line.spans.iter()
                        .map(|s| s.content.as_ref())
                        .collect();
                    if text.is_empty() {
                        comment_lines.push(Line::from(Span::styled(
                            "    \u{2502}",
                            Style::default().fg(C_ACCENT).bg(bg),
                        )));
                        count += 1;
                        continue;
                    }
                    let style = md_line.spans.first()
                        .map(|s| s.style)
                        .unwrap_or(Style::default().fg(C_TEXT));
                    let mut remaining = text.as_str();
                    while !remaining.is_empty() && count < 15 {
                        let chunk = if remaining.len() <= wrap_width {
                            remaining
                        } else {
                            remaining[..wrap_width].rfind(' ').map(|p| &remaining[..p]).unwrap_or(&remaining[..wrap_width])
                        };
                        comment_lines.push(Line::from(vec![
                            Span::styled("    \u{2502} ", Style::default().fg(C_ACCENT).bg(bg)),
                            Span::styled(chunk.to_string(), style.bg(bg)),
                        ]));
                        remaining = remaining[chunk.len()..].trim_start();
                        count += 1;
                    }
                }
                comment_lines.push(Line::raw(""));
            }
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
            .scroll((app.state.detail_scroll, 0))
            .style(Style::default().bg(C_BG)),
        chunks[1],
    );

    // ── Action bar ──
    let n_selected = app.state.selected_comments.len();

    // Contextual action bar for detail pane.
    let detail_hints = {
        let ci_ok = matches!(task.ci, CiStatus::Success | CiStatus::None);
        let review_ok = task.review == ReviewStatus::Approved;
        crate::keymap::contextual_hints(
            crate::keys::KeyMode::Detail,
            &crate::keymap::HintContext {
                ci: task.ci,
                review: task.review,
                role: task.role,
                has_conflicts: task.has_conflicts,
                has_unread: session.unread_count() > 0,
                is_ready: ci_ok && review_ok && !task.has_conflicts,
                has_terminal: app.terminals.contains_key(key),
                needs_reply: task.needs_reply,
                auto_merge_enabled: task.auto_merge_enabled,
                has_reviewers: !task.reviewers.is_empty() || !task.assignees.is_empty(),
            },
        )
    };

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
        // When comments are selected, show fix/reply actions.
        spans.push(Span::styled("f", Style::default().fg(C_ACCENT).bold()));
        spans.push(Span::styled(":fix ", Style::default().fg(C_TEXT_DIM)));
        spans.push(Span::styled("r", Style::default().fg(C_ACCENT).bold()));
        spans.push(Span::styled(":reply ", Style::default().fg(C_TEXT_DIM)));
        spans.push(Span::styled("Esc", Style::default().fg(C_ACCENT).bold()));
        spans.push(Span::styled(":clear ", Style::default().fg(C_TEXT_DIM)));
        Line::from(spans)
    } else {
        build_action_bar(&detail_hints)
    };
    frame.render_widget(
        Paragraph::new(action_bar).style(Style::default().bg(C_BG_ALT)),
        chunks[2],
    );
}

// ─── Terminal ─────────────────────────────────────────────────────────────

fn render_terminal(app: &mut App, frame: &mut Frame, area: Rect, key: &str) {
    let is_focused = app.state.input_mode == InputMode::Terminal;
    let border_color = if is_focused { C_GREEN } else { C_BORDER };

    let shell_label = match app.terminals.kind(key) {
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

    // Claude state in the title bar — matches the sidebar spinner/dot/?
    // so the user sees the same signal whether looking at the list or the
    // terminal itself.
    let claude_indicator = {
        use crate::agent_state::AgentState;
        let state = app.state.agent_states.get(key).copied();
        let is_claude = matches!(
            app.terminals.kind(key),
            Some(crate::action::ShellKind::Claude)
        );
        let tick = app.state.tick_count;
        if !is_claude {
            Span::raw("")
        } else {
            match state {
                Some(AgentState::Asking) => {
                    let col = if tick.is_multiple_of(8) || (tick % 8) >= 4 {
                        C_RED
                    } else {
                        C_YELLOW
                    };
                    Span::styled(" ? INPUT NEEDED ", Style::default().fg(col).bold())
                }
                Some(AgentState::Active) => {
                    const FRAMES: [&str; 10] = [
                        "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}",
                        "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
                        "\u{2807}", "\u{280f}",
                    ];
                    let f = FRAMES[(tick as usize) % FRAMES.len()];
                    Span::styled(
                        format!(" {f} working "),
                        Style::default().fg(C_CYAN).bold(),
                    )
                }
                Some(AgentState::Idle) => {
                    Span::styled(" ● idle ", Style::default().fg(C_GREEN).bold())
                }
                None => Span::raw(""),
            }
        }
    };

    let block = Block::bordered()
        .title(Line::from(vec![
            Span::styled(
                format!(" {shell_label} "),
                Style::default().fg(C_GREEN).bold(),
            ),
            claude_indicator,
            scroll_indicator,
            Span::styled(hint, Style::default().fg(C_TEXT_DIM)),
            Span::raw(" "),
        ]))
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let new_size = (inner.width, inner.height);
    if new_size != app.state.last_term_area && new_size.0 > 0 && new_size.1 > 0 {
        app.state.last_term_area = new_size;
        if let Some(term) = app.terminals.get_mut(key)
            && let Err(e) = term.resize(pilot_tui_term::PtySize {
                rows: new_size.1,
                cols: new_size.0,
                pixel_width: 0,
                pixel_height: 0,
            }) {
                tracing::error!("Terminal resize failed: {e}");
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
    let s = spinner[(app.state.tick_count as usize / 2) % spinner.len()];

    let lines = if !app.state.loaded {
        let filter = app.state
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
                Span::styled(&app.state.status, Style::default().fg(C_TEXT)),
            ]),
        ]
    } else if app.state.sessions.is_empty() {
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
    // Derive the display mode from the pane tree for core modes so the label
    // can never show "TERM" when no terminal pane is visible. Overlay modes
    // (Help, TextInput, etc.) take precedence because they own the screen.
    let effective_mode = if app.state.input_mode.is_overlay() {
        app.state.input_mode.clone()
    } else {
        crate::app::determine_mode(app)
    };
    let (label, label_bg) = match effective_mode {
        InputMode::Normal => ("INBOX", C_ACCENT),
        InputMode::Detail => ("DETAIL", C_MAGENTA),
        InputMode::PanePrefix => ("PANE", C_ORANGE),
        InputMode::Terminal => ("TERM", C_GREEN),
        InputMode::TextInput(_) => ("INPUT", C_YELLOW),
        InputMode::Picker => ("PICKER", C_MAGENTA),
        InputMode::Help => ("HELP", C_ACCENT),
    };

    let total_unread: usize = app.state.sessions.values().map(|s| s.unread_count()).sum();
    let notif = app.state
        .notifications
        .first()
        .map(|s| truncate_str(s, 40))
        .unwrap_or("");

    let user_label = if app.state.username.is_empty() {
        String::new()
    } else {
        format!("{} (gh)", app.state.username)
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
        if app.state.status.starts_with("Error") {
            Span::styled(
                format!("{} ", app.state.status),
                Style::default().fg(C_RED).bold(),
            )
        } else {
            Span::styled(
                format!("{} ", app.state.status),
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
        if !app.terminals.tab_order().is_empty() {
            Span::styled(
                format!("{} tabs  ", app.terminals.tab_order().len()),
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
        if !app.state.credentials_ok {
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
/// Takes `now` explicitly so render can sample wall time once and every row
/// uses the same reference point (no per-row `Utc::now()` syscalls).
fn time_ago_short(dt: &chrono::DateTime<chrono::Utc>, now: chrono::DateTime<chrono::Utc>) -> String {
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

fn status_tag_colors(tag: pilot_core::StatusTag, row_bg: Color) -> (&'static str, Color, Color) {
    use pilot_core::StatusTag;
    let dark_fg = Color::Rgb(15, 17, 23);
    match tag {
        StatusTag::Conflict => ("CONFLICT", dark_fg, C_RED),
        StatusTag::CiFailed => ("CI FAIL", dark_fg, C_RED),
        StatusTag::ChangesRequested => ("CHANGES", dark_fg, C_ORANGE),
        StatusTag::Queued => ("QUEUED", dark_fg, C_MAGENTA),
        StatusTag::Ready => ("READY", dark_fg, C_GREEN),
        StatusTag::AutoMerge => ("AUTO", dark_fg, C_MAGENTA),
        StatusTag::ReviewPending => ("REVIEW", dark_fg, C_YELLOW),
        StatusTag::CiRunning => ("CI...", C_YELLOW, row_bg),
        StatusTag::Draft => ("DRAFT", C_TEXT_DIM, row_bg),
        StatusTag::None => ("", C_TEXT_DIM, row_bg),
    }
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
                    if style.add_modifier.contains(Modifier::BOLD)
                        && style.fg == Some(C_TEXT) {
                            style = style.fg(C_TEXT_BRIGHT);
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

/// Strip HTML tags and clean up markdown image syntax for terminal display.
fn strip_html(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            in_tag = true;
        } else if c == '>' && in_tag {
            in_tag = false;
        } else if !in_tag {
            // Skip markdown image syntax: ![alt](url)
            if c == '!' && chars.peek() == Some(&'[') {
                // Skip until closing )
                let mut depth = 0;
                for ic in chars.by_ref() {
                    if ic == '(' { depth += 1; }
                    if ic == ')' { depth -= 1; if depth <= 0 { break; } }
                }
            } else {
                result.push(c);
            }
        }
    }
    // Collapse multiple spaces/newlines.
    let collapsed: String = result.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
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
