//! Sidebar behavior tests. Covers:
//!
//! - Event handling (Snapshot / SessionUpserted / SessionRemoved).
//! - Visibility filtering (Inbox vs Snoozed, merged/closed hidden).
//! - Sort order (updated_at desc).
//! - Cursor preservation across re-sort / upsert / remove.
//! - All keybindings — each emits the expected Command.
//! - Kill two-press confirmation semantics.
//! - Render output via ratatui's TestBackend (smoke + cursor marker).

use chrono::{DateTime, Duration, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pilot_core::{CiStatus, ReviewStatus, Session, SessionKey, Task, TaskId, TaskRole, TaskState};
use pilot_v2_ipc::{Command, Event, TerminalKind};
use pilot_v2_tui::components::{Mailbox, Sidebar};
use pilot_v2_tui::{Component, ComponentId};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::prelude::Rect;

// ── Fixtures ───────────────────────────────────────────────────────────

fn make_task(key: &str, updated: DateTime<Utc>) -> Task {
    Task {
        id: TaskId {
            source: "github".into(),
            key: key.into(),
        },
        title: format!("task: {key}"),
        body: None,
        state: TaskState::Open,
        role: TaskRole::Author,
        ci: CiStatus::None,
        review: ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: format!("https://github.com/{key}"),
        repo: Some("owner/repo".into()),
        branch: Some("main".into()),
        base_branch: None,
        updated_at: updated,
        labels: vec![],
        reviewers: vec![],
        assignees: vec![],
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        has_conflicts: false,
        is_behind_base: false,
        node_id: None,
        needs_reply: false,
        last_commenter: None,
        recent_activity: vec![],
        additions: 0,
        deletions: 0,
    }
}

fn make_session(key: &str, updated: DateTime<Utc>) -> Session {
    Session::new_at(make_task(key, updated), updated)
}

fn key_code(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn shift_char(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
}

fn session_key_ref(key: &str) -> SessionKey {
    key.into()
}

// ── Event handling ─────────────────────────────────────────────────────

#[test]
fn snapshot_populates_sessions() {
    let mut s = Sidebar::new(ComponentId::new(1));
    let now = Utc::now();
    s.on_event(&Event::Snapshot {
        sessions: vec![
            make_session("o/r#1", now),
            make_session("o/r#2", now - Duration::hours(1)),
        ],
        terminals: vec![],
    });
    assert_eq!(s.visible_count(), 2);
    assert_eq!(s.cursor(), 0);
    assert_eq!(
        s.selected_session_key(),
        Some(&session_key_ref("github:o/r#1"))
    );
}

#[test]
fn session_upserted_inserts_and_updates() {
    let mut s = Sidebar::new(ComponentId::new(1));
    let now = Utc::now();
    s.on_event(&Event::SessionUpserted(Box::new(make_session(
        "o/r#1", now,
    ))));
    assert_eq!(s.visible_count(), 1);

    // Upsert with newer timestamp — stays as one entry, moves to top.
    let mut updated = make_session("o/r#1", now + Duration::minutes(5));
    updated.display_name = "renamed".into();
    s.on_event(&Event::SessionUpserted(Box::new(updated)));
    assert_eq!(s.visible_count(), 1);
    assert_eq!(
        s.selected_session().map(|sess| sess.display_name.as_str()),
        Some("renamed")
    );
}

#[test]
fn session_removed_prunes_and_clamps_cursor() {
    let mut s = Sidebar::new(ComponentId::new(1));
    let now = Utc::now();
    s.on_event(&Event::Snapshot {
        sessions: vec![
            make_session("o/r#1", now),
            make_session("o/r#2", now - Duration::hours(1)),
        ],
        terminals: vec![],
    });
    // Move cursor to row 1.
    s.handle_key(key_code(KeyCode::Char('j')), &mut Vec::new());
    assert_eq!(s.cursor(), 1);

    // Remove the currently-selected session; cursor clamps to 0.
    s.on_event(&Event::SessionRemoved(session_key_ref("github:o/r#2")));
    assert_eq!(s.visible_count(), 1);
    assert_eq!(s.cursor(), 0);
}

#[test]
fn cursor_preserved_across_upsert_of_other_session() {
    let mut s = Sidebar::new(ComponentId::new(1));
    let now = Utc::now();
    s.on_event(&Event::Snapshot {
        sessions: vec![
            make_session("o/r#1", now),
            make_session("o/r#2", now - Duration::hours(1)),
            make_session("o/r#3", now - Duration::hours(2)),
        ],
        terminals: vec![],
    });
    // Cursor on #2.
    s.handle_key(key_code(KeyCode::Char('j')), &mut Vec::new());
    let selected_before = s.selected_session_key().cloned();
    assert_eq!(selected_before, Some(session_key_ref("github:o/r#2")));

    // An unrelated session gets a new update and would normally
    // displace #2 in the sort order — but the cursor follows the key,
    // not the index.
    s.on_event(&Event::SessionUpserted(Box::new(make_session(
        "o/r#3",
        now + Duration::hours(1),
    ))));
    assert_eq!(
        s.selected_session_key().cloned(),
        Some(session_key_ref("github:o/r#2")),
        "cursor follows the session key across re-sort"
    );
}

#[test]
fn merged_session_hidden() {
    let mut s = Sidebar::new(ComponentId::new(1));
    let now = Utc::now();
    let mut merged = make_session("o/r#1", now);
    merged.primary_task.state = TaskState::Merged;
    s.on_event(&Event::Snapshot {
        sessions: vec![merged, make_session("o/r#2", now)],
        terminals: vec![],
    });
    assert_eq!(s.visible_count(), 1);
    assert_eq!(
        s.selected_session_key(),
        Some(&session_key_ref("github:o/r#2"))
    );
}

// ── Mailbox ────────────────────────────────────────────────────────────

#[test]
fn snoozed_session_hidden_from_inbox() {
    let mut s = Sidebar::new(ComponentId::new(1));
    let now = Utc::now();
    let mut snoozed = make_session("o/r#1", now);
    snoozed.snoozed_until = Some(now + Duration::hours(4));
    s.on_event(&Event::Snapshot {
        sessions: vec![snoozed, make_session("o/r#2", now)],
        terminals: vec![],
    });
    assert_eq!(s.visible_count(), 1);
    assert_eq!(s.mailbox(), Mailbox::Inbox);
}

#[test]
fn toggle_mailbox_shows_the_other_set() {
    let mut s = Sidebar::new(ComponentId::new(1));
    let now = Utc::now();
    let mut snoozed = make_session("o/r#1", now);
    snoozed.snoozed_until = Some(now + Duration::hours(4));
    s.on_event(&Event::Snapshot {
        sessions: vec![snoozed, make_session("o/r#2", now)],
        terminals: vec![],
    });
    s.handle_key(shift_char('S'), &mut Vec::new());
    assert_eq!(s.mailbox(), Mailbox::Snoozed);
    assert_eq!(s.visible_count(), 1);
    assert_eq!(
        s.selected_session_key(),
        Some(&session_key_ref("github:o/r#1"))
    );

    s.handle_key(shift_char('S'), &mut Vec::new());
    assert_eq!(s.mailbox(), Mailbox::Inbox);
    assert_eq!(s.visible_count(), 1);
    assert_eq!(
        s.selected_session_key(),
        Some(&session_key_ref("github:o/r#2"))
    );
}

// ── Keybindings → commands ─────────────────────────────────────────────

fn populated_sidebar() -> Sidebar {
    let mut s = Sidebar::new(ComponentId::new(1));
    let now = Utc::now();
    s.on_event(&Event::Snapshot {
        sessions: vec![
            make_session("o/r#1", now),
            make_session("o/r#2", now - Duration::hours(1)),
        ],
        terminals: vec![],
    });
    s
}

#[test]
fn c_emits_spawn_claude_for_selected() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Char('c')), &mut cmds);
    assert_eq!(cmds.len(), 1);
    match &cmds[0] {
        Command::Spawn {
            session_key,
            kind: TerminalKind::Agent(agent),
            ..
        } => {
            assert_eq!(session_key.as_str(), "github:o/r#1");
            assert_eq!(agent, "claude");
        }
        other => panic!("expected Spawn Agent(claude), got {other:?}"),
    }
}

#[test]
fn shift_c_emits_spawn_codex_for_selected() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    s.handle_key(shift_char('C'), &mut cmds);
    assert_eq!(cmds.len(), 1);
    match &cmds[0] {
        Command::Spawn {
            session_key,
            kind: TerminalKind::Agent(agent),
            ..
        } => {
            assert_eq!(session_key.as_str(), "github:o/r#1");
            assert_eq!(agent, "codex", "Shift-C maps to Codex by default");
        }
        other => panic!("expected Spawn Agent(codex), got {other:?}"),
    }
}

#[test]
fn custom_agent_shortcuts_override_defaults() {
    // User maps `a` → aider via config.
    let mut s = Sidebar::new(ComponentId::new(1))
        .with_agent_shortcuts([('c', "claude".to_string()), ('a', "aider".to_string())]);
    // Seed a session so the spawn has a target.
    let now = chrono::Utc::now();
    let task = pilot_core::Task {
        id: pilot_core::TaskId {
            source: "github".into(),
            key: "o/r#1".into(),
        },
        title: "t".into(),
        body: None,
        state: pilot_core::TaskState::Open,
        role: pilot_core::TaskRole::Author,
        ci: pilot_core::CiStatus::None,
        review: pilot_core::ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: "https://github.com/o/r/pull/1".into(),
        repo: Some("o/r".into()),
        branch: Some("f".into()),
        base_branch: None,
        updated_at: now,
        labels: vec![],
        reviewers: vec![],
        assignees: vec![],
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        has_conflicts: false,
        is_behind_base: false,
        node_id: None,
        needs_reply: false,
        last_commenter: None,
        recent_activity: vec![],
        additions: 0,
        deletions: 0,
    };
    s.on_event(&Event::SessionUpserted(Box::new(pilot_core::Session::new_at(
        task, now,
    ))));

    // `a` now spawns aider.
    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Char('a')), &mut cmds);
    match cmds.as_slice() {
        [
            Command::Spawn {
                kind: TerminalKind::Agent(agent),
                ..
            },
        ] => assert_eq!(agent, "aider"),
        _ => panic!("expected Spawn Agent(aider), got {cmds:?}"),
    }

    // Shift-C is no longer mapped in the custom set — bubbles up.
    let mut cmds = Vec::new();
    let outcome = s.handle_key(shift_char('C'), &mut cmds);
    assert_eq!(
        outcome,
        pilot_v2_tui::Outcome::BubbleUp,
        "unmapped key bubbles, doesn't spawn a random default"
    );
    assert!(cmds.is_empty());
}

#[test]
fn c_on_empty_sidebar_emits_nothing() {
    let mut s = Sidebar::new(ComponentId::new(1));
    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Char('c')), &mut cmds);
    assert!(cmds.is_empty());
}

#[test]
fn b_emits_spawn_shell() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Char('b')), &mut cmds);
    assert_eq!(cmds.len(), 1);
    assert!(matches!(
        &cmds[0],
        Command::Spawn {
            kind: TerminalKind::Shell,
            ..
        }
    ));
}

#[test]
fn m_emits_mark_read() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Char('m')), &mut cmds);
    assert_eq!(cmds.len(), 1);
    match &cmds[0] {
        Command::MarkRead { session_key } => {
            assert_eq!(session_key.as_str(), "github:o/r#1");
        }
        other => panic!("expected MarkRead, got {other:?}"),
    }
}

#[test]
fn g_emits_refresh_without_selection() {
    let mut s = Sidebar::new(ComponentId::new(1));
    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Char('g')), &mut cmds);
    assert!(matches!(cmds.as_slice(), [Command::Refresh]));
}

#[test]
fn shift_m_emits_merge() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    s.handle_key(shift_char('M'), &mut cmds);
    assert!(matches!(cmds.as_slice(), [Command::Merge { .. }]));
}

#[test]
fn shift_v_emits_approve() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    s.handle_key(shift_char('V'), &mut cmds);
    assert!(matches!(cmds.as_slice(), [Command::Approve { .. }]));
}

#[test]
fn shift_u_emits_update_branch() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    s.handle_key(shift_char('U'), &mut cmds);
    assert!(matches!(cmds.as_slice(), [Command::UpdateBranch { .. }]));
}

// ── Snooze semantics ───────────────────────────────────────────────────

#[test]
fn z_snoozes_unsnoozed_session_for_4h() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    let before = Utc::now();
    s.handle_key(key_code(KeyCode::Char('z')), &mut cmds);
    let after = Utc::now();
    assert_eq!(cmds.len(), 1);
    match &cmds[0] {
        Command::Snooze { until, .. } => {
            let min = before + Duration::hours(4) - Duration::seconds(2);
            let max = after + Duration::hours(4) + Duration::seconds(2);
            assert!(
                *until >= min && *until <= max,
                "expected until ~4h from now, got {until}"
            );
        }
        other => panic!("expected Snooze, got {other:?}"),
    }
}

#[test]
fn z_unsnoozes_already_snoozed_session() {
    let mut s = Sidebar::new(ComponentId::new(1));
    let now = Utc::now();
    let mut snoozed = make_session("o/r#1", now);
    snoozed.snoozed_until = Some(now + Duration::hours(4));
    s.on_event(&Event::Snapshot {
        sessions: vec![snoozed],
        terminals: vec![],
    });
    // Toggle to Snoozed mailbox so the cursor lands on it.
    s.handle_key(shift_char('S'), &mut Vec::new());

    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Char('z')), &mut cmds);
    assert!(matches!(cmds.as_slice(), [Command::Unsnooze { .. }]));
}

#[test]
fn shift_z_archives_a_year_out() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    let before = Utc::now();
    s.handle_key(shift_char('Z'), &mut cmds);
    assert_eq!(cmds.len(), 1);
    match &cmds[0] {
        Command::Snooze { until, .. } => {
            let min = before + Duration::days(364);
            assert!(*until >= min, "archive snooze should be roughly a year");
        }
        other => panic!("expected Snooze, got {other:?}"),
    }
}

// ── Kill / double-press ────────────────────────────────────────────────

#[test]
fn shift_x_requires_two_presses() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();

    // First press — arms, no command.
    s.handle_key(shift_char('X'), &mut cmds);
    assert!(cmds.is_empty());
    assert_eq!(s.kill_armed().map(|k| k.as_str()), Some("github:o/r#1"));

    // Second press on same session — fires Kill, disarms.
    s.handle_key(shift_char('X'), &mut cmds);
    assert!(matches!(cmds.as_slice(), [Command::Kill { .. }]));
    assert!(s.kill_armed().is_none());
}

#[test]
fn shift_x_disarmed_by_unrelated_key() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    s.handle_key(shift_char('X'), &mut cmds);
    assert!(s.kill_armed().is_some());
    // `j` isn't Shift-X → disarms.
    s.handle_key(key_code(KeyCode::Char('j')), &mut cmds);
    assert!(s.kill_armed().is_none());
    // A subsequent Shift-X restarts the sequence rather than firing.
    s.handle_key(shift_char('X'), &mut cmds);
    let spawn_count = cmds
        .iter()
        .filter(|c| matches!(c, Command::Kill { .. }))
        .count();
    assert_eq!(spawn_count, 0, "Kill must not have fired");
}

#[test]
fn shift_x_disarmed_when_moved_to_different_session() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    s.handle_key(shift_char('X'), &mut cmds);
    // Cursor move disarms the pending kill.
    s.handle_key(key_code(KeyCode::Char('j')), &mut cmds);
    assert!(s.kill_armed().is_none());
    // Now Shift-X on the NEW row arms that one, not the original.
    s.handle_key(shift_char('X'), &mut cmds);
    assert_eq!(s.kill_armed().map(|k| k.as_str()), Some("github:o/r#2"));
}

// ── Navigation bounds ─────────────────────────────────────────────────

#[test]
fn j_caps_at_last_row() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    for _ in 0..10 {
        s.handle_key(key_code(KeyCode::Char('j')), &mut cmds);
    }
    assert_eq!(s.cursor(), 1, "capped at last visible index");
}

#[test]
fn k_caps_at_zero() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    for _ in 0..10 {
        s.handle_key(key_code(KeyCode::Char('k')), &mut cmds);
    }
    assert_eq!(s.cursor(), 0);
}

// ── Bubble-up ──────────────────────────────────────────────────────────

#[test]
fn unknown_key_bubbles_up() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    let outcome = s.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &mut cmds);
    assert_eq!(outcome, pilot_v2_tui::Outcome::BubbleUp);
    assert!(cmds.is_empty(), "sidebar did not emit anything");
}

// ── Render ─────────────────────────────────────────────────────────────

fn render_to_string(s: &mut Sidebar, width: u16, height: u16, focused: bool) -> String {
    let backend = TestBackend::new(width, height);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|frame| {
        let area = Rect::new(0, 0, width, height);
        s.render(area, frame, focused);
    })
    .unwrap();
    // Convert the buffer into a plain-text view for assertions.
    let buffer = term.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn render_smoke_has_mailbox_label_and_rows() {
    let mut s = populated_sidebar();
    let rendered = render_to_string(&mut s, 40, 10, true);
    assert!(
        rendered.contains("INBOX"),
        "mailbox label visible; got:\n{rendered}"
    );
    assert!(
        rendered.contains("(2)"),
        "row count reflected in title; got:\n{rendered}"
    );
    // Display names come from Session::new_at — for a task with no
    // explicit display_name, v1 uses the title.
    assert!(
        rendered.contains("task: o/r#1"),
        "first session visible; got:\n{rendered}"
    );
}

#[test]
fn render_shows_cursor_marker_on_selected_row() {
    let mut s = populated_sidebar();
    let rendered = render_to_string(&mut s, 40, 10, true);
    // Cursor marker is '▸ ' in front of the selected row.
    let cursor_line = rendered
        .lines()
        .find(|l| l.contains('▸'))
        .unwrap_or_else(|| panic!("expected cursor marker; got:\n{rendered}"));
    assert!(cursor_line.contains("o/r#1"), "cursor on top row");
}

#[test]
fn render_mailbox_toggles_title() {
    let mut s = populated_sidebar();
    s.handle_key(shift_char('S'), &mut Vec::new());
    let rendered = render_to_string(&mut s, 40, 10, true);
    assert!(
        rendered.contains("SNOOZED"),
        "title updates; got:\n{rendered}"
    );
}

#[test]
fn render_shows_kill_marker_when_armed() {
    let mut s = populated_sidebar();
    s.handle_key(shift_char('X'), &mut Vec::new());
    let rendered = render_to_string(&mut s, 40, 10, true);
    assert!(
        rendered.contains("[kill?]"),
        "kill confirmation visible; got:\n{rendered}"
    );
}
