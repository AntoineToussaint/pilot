//! `Footer` — single-row status line at the bottom of the screen.
//!
//! Three zones, all on one row:
//!
//! - **Left**: keymap hints from the currently focused pane. Same
//!   bindings the help modal lists; this is the always-on quick
//!   reference.
//! - **Center**: background polling status — spinner + "Pulling
//!   tasks from github · PR query: …". Empty when no poll is in
//!   flight.
//! - **Right**: most recent notice / error. Retryable hiccups
//!   auto-fade; permanent + auth errors stay until dismissed.
//!
//! Pure render — state lives on `Model` and gets passed in.

use crate::pane::Binding;
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::Paragraph;

/// Severity of a footer notice — drives its color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeSeverity {
    /// Transient hiccup. `theme.warn`. Auto-fades.
    Retryable,
    /// Auth or other actionable error. `theme.warn`. Sticky.
    Auth,
    /// Hard failure. `theme.error`. Sticky.
    Permanent,
    /// Plain informational. `theme.text_dim`.
    Info,
}

/// One footer notice — message + severity + when it was set
/// (for auto-fade).
#[derive(Debug, Clone)]
pub struct Notice {
    pub message: String,
    pub severity: NoticeSeverity,
    pub set_at: std::time::Instant,
}

impl Notice {
    pub fn new(message: impl Into<String>, severity: NoticeSeverity) -> Self {
        Self {
            message: message.into(),
            severity,
            set_at: std::time::Instant::now(),
        }
    }
}

/// Pure render. The orchestrator passes in everything Footer needs:
/// the focused pane's keymap, the optional polling status, and the
/// optional notice. Returns nothing — paints directly.
pub fn render(
    f: &mut Frame,
    area: Rect,
    keymap: &'static [Binding],
    polling_status: Option<(&str, &str)>, // (spinner, label)
    notice: Option<&Notice>,
) {
    let theme = crate::theme::current();

    // Background fill so the line stands out.
    let bg = Style::default().bg(theme.surface);
    f.render_widget(
        Paragraph::new(Line::raw("")).style(bg),
        area,
    );

    // Reserve the right-most segment for the notice (or polling
    // status if no notice). Keymap fills the rest of the line.
    let right_text = if let Some(n) = notice {
        let sev_color = match n.severity {
            NoticeSeverity::Retryable | NoticeSeverity::Auth => theme.warn,
            NoticeSeverity::Permanent => theme.error,
            NoticeSeverity::Info => theme.text_dim,
        };
        Some(Line::from(vec![
            Span::styled(" ", bg),
            Span::styled(
                format!(" {} ", n.message),
                Style::default()
                    .bg(sev_color)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", bg),
        ]))
    } else if let Some((spinner, label)) = polling_status {
        Some(Line::from(vec![
            Span::styled(
                format!(" {spinner} "),
                Style::default()
                    .bg(theme.surface)
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(label.to_string(), Style::default().bg(theme.surface).fg(theme.text_dim)),
            Span::styled(" ", bg),
        ]))
    } else {
        None
    };

    let right_width = right_text
        .as_ref()
        .map(|l| l.width() as u16)
        .unwrap_or(0);
    let right_rect = Rect {
        x: area.x + area.width.saturating_sub(right_width),
        y: area.y,
        width: right_width.min(area.width),
        height: 1,
    };
    let left_rect = Rect {
        x: area.x,
        y: area.y,
        width: area.width.saturating_sub(right_width),
        height: 1,
    };

    // Left zone: focused-pane keymap on the left, then a separator,
    // then the always-on globals (`,` settings, `?` help, `q q`
    // quit). Globals come last so the per-pane hints sit closest to
    // where the user's eye lands.
    const GLOBALS: &[Binding] = &[
        Binding { keys: ",", label: "settings" },
        Binding { keys: "?", label: "help" },
        Binding { keys: "q q", label: "quit" },
    ];

    let mut spans: Vec<Span> = Vec::with_capacity((keymap.len() + GLOBALS.len()) * 4 + 2);
    spans.push(Span::styled(" ", bg));
    let key_style = Style::default().bg(theme.surface).fg(theme.accent).add_modifier(Modifier::BOLD);
    let label_style = Style::default().bg(theme.surface).fg(theme.text_dim);
    let sep_style = Style::default().bg(theme.surface).fg(theme.chrome);
    let global_key_style = Style::default()
        .bg(theme.surface)
        .fg(theme.warn)
        .add_modifier(Modifier::BOLD);
    for (i, b) in keymap.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ·  ", sep_style));
        }
        spans.push(Span::styled(b.keys, key_style));
        spans.push(Span::styled(" ", bg));
        spans.push(Span::styled(b.label, label_style));
    }
    if !keymap.is_empty() {
        spans.push(Span::styled("    ║    ", sep_style));
    }
    for (i, b) in GLOBALS.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ·  ", sep_style));
        }
        spans.push(Span::styled(b.keys, global_key_style));
        spans.push(Span::styled(" ", bg));
        spans.push(Span::styled(b.label, label_style));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(bg), left_rect);

    if let Some(line) = right_text {
        f.render_widget(Paragraph::new(line).style(bg), right_rect);
    }
}
