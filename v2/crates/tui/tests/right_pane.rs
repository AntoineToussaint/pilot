//! Tests for RightPane: session selection, comment navigation,
//! render. State isolation — selection changes reset scroll; unrelated
//! events don't perturb state.

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pilot_core::{
    Activity, ActivityKind, CiStatus, ReviewStatus, Session, Task, TaskId, TaskRole, TaskState,
};
use pilot_v2_ipc::Event;
use pilot_v2_tui::components::RightPane;
use pilot_v2_tui::{Component, ComponentId};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::prelude::Rect;

fn make_task(key: &str) -> Task {
    Task {
        id: TaskId {
            source: "github".into(),
            key: key.into(),
        },
        title: format!("PR {key}"),
        body: None,
        state: TaskState::Open,
        role: TaskRole::Reviewer,
        ci: CiStatus::Success,
        review: ReviewStatus::Pending,
        checks: vec![],
        unread_count: 0,
        url: format!("https://github.com/{key}"),
        repo: Some("owner/repo".into()),
        branch: Some("feature/x".into()),
        base_branch: Some("main".into()),
        updated_at: Utc::now(),
        labels: vec![],
        reviewers: vec!["alice".into(), "bob".into()],
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

fn activity(author: &str, body: &str, kind: ActivityKind) -> Activity {
    Activity {
        author: author.into(),
        body: body.into(),
        created_at: Utc::now(),
        kind,
        node_id: None,
        path: None,
        line: None,
        diff_hunk: None,
        thread_id: None,
    }
}

fn session_with_n_activities(key: &str, n: usize) -> Session {
    let mut s = Session::new_at(make_task(key), Utc::now());
    for i in 0..n {
        s.push_activity(activity(
            &format!("user{i}"),
            &format!("comment body {i}"),
            ActivityKind::Comment,
        ));
    }
    s
}

fn render_to_string(rp: &mut RightPane, width: u16, height: u16, focused: bool) -> String {
    let backend = TestBackend::new(width, height);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|frame| {
        rp.render(Rect::new(0, 0, width, height), frame, focused);
    })
    .unwrap();
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

// ── Selection ──────────────────────────────────────────────────────────

#[test]
fn set_session_stores_it() {
    let mut rp = RightPane::new(ComponentId::new(1));
    assert!(rp.selected_session().is_none());
    rp.set_session(Some(session_with_n_activities("o/r#1", 0)));
    assert!(rp.selected_session().is_some());
}

#[test]
fn set_session_to_different_resets_cursor() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_session(Some(session_with_n_activities("o/r#1", 5)));
    // Move cursor.
    for _ in 0..3 {
        rp.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut Vec::new(),
        );
    }
    assert_eq!(rp.comment_cursor(), 3);

    // Different session — cursor resets.
    rp.set_session(Some(session_with_n_activities("o/r#2", 5)));
    assert_eq!(rp.comment_cursor(), 0, "cursor resets when session changes");
}

#[test]
fn set_session_to_same_preserves_cursor() {
    let mut rp = RightPane::new(ComponentId::new(1));
    let session = session_with_n_activities("o/r#1", 5);
    rp.set_session(Some(session.clone()));
    for _ in 0..2 {
        rp.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut Vec::new(),
        );
    }
    assert_eq!(rp.comment_cursor(), 2);

    // Re-setting the same session (e.g. AppRoot refreshes on every
    // frame) must not reset the cursor.
    rp.set_session(Some(session));
    assert_eq!(rp.comment_cursor(), 2);
}

#[test]
fn set_session_none_clears() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_session(Some(session_with_n_activities("o/r#1", 3)));
    rp.set_session(None);
    assert!(rp.selected_session().is_none());
}

// ── Comment navigation ─────────────────────────────────────────────────

#[test]
fn j_moves_cursor_down_bounded() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_session(Some(session_with_n_activities("o/r#1", 3)));
    for _ in 0..10 {
        rp.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut Vec::new(),
        );
    }
    assert_eq!(rp.comment_cursor(), 2, "capped at last index");
}

#[test]
fn k_moves_cursor_up_bounded() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_session(Some(session_with_n_activities("o/r#1", 3)));
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        &mut Vec::new(),
    );
    for _ in 0..10 {
        rp.handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut Vec::new(),
        );
    }
    assert_eq!(rp.comment_cursor(), 0);
}

#[test]
fn g_jumps_to_top() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_session(Some(session_with_n_activities("o/r#1", 10)));
    for _ in 0..5 {
        rp.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut Vec::new(),
        );
    }
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert_eq!(rp.comment_cursor(), 0);
}

#[test]
fn shift_g_jumps_to_bottom() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_session(Some(session_with_n_activities("o/r#1", 7)));
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
        &mut Vec::new(),
    );
    assert_eq!(rp.comment_cursor(), 6);
}

#[test]
fn j_without_session_bubbles_up() {
    let mut rp = RightPane::new(ComponentId::new(1));
    let outcome = rp.handle_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert_eq!(outcome, pilot_v2_tui::Outcome::BubbleUp);
}

#[test]
fn unknown_key_bubbles_up_even_with_session() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_session(Some(session_with_n_activities("o/r#1", 3)));
    let outcome = rp.handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert_eq!(outcome, pilot_v2_tui::Outcome::BubbleUp);
}

#[test]
fn j_on_empty_activity_list_consumes_but_does_not_advance() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_session(Some(session_with_n_activities("o/r#1", 0)));
    let outcome = rp.handle_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert_eq!(
        outcome,
        pilot_v2_tui::Outcome::Consumed,
        "j still claims the key (we're 'navigating', just in an empty list)"
    );
    assert_eq!(rp.comment_cursor(), 0);
}

// ── Events ─────────────────────────────────────────────────────────────

#[test]
fn session_upserted_for_current_updates_state() {
    let mut rp = RightPane::new(ComponentId::new(1));
    let mut session = session_with_n_activities("o/r#1", 3);
    rp.set_session(Some(session.clone()));
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
        &mut Vec::new(),
    );
    assert_eq!(rp.comment_cursor(), 2);

    // Upsert with one MORE activity. Cursor stays put.
    session.push_activity(activity("bob", "new one", ActivityKind::Comment));
    rp.on_event(&Event::SessionUpserted(Box::new(session)));
    assert_eq!(rp.comment_cursor(), 2);
    assert_eq!(rp.selected_session().unwrap().activity.len(), 4);
}

#[test]
fn session_upserted_shrinks_clamps_cursor() {
    let mut rp = RightPane::new(ComponentId::new(1));
    let session = session_with_n_activities("o/r#1", 5);
    rp.set_session(Some(session));
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
        &mut Vec::new(),
    );
    assert_eq!(rp.comment_cursor(), 4);

    // New snapshot has fewer activities (e.g. server de-duped).
    let smaller = session_with_n_activities("o/r#1", 2);
    rp.on_event(&Event::SessionUpserted(Box::new(smaller)));
    assert_eq!(rp.comment_cursor(), 1, "clamped to last available");
}

#[test]
fn session_upserted_for_different_session_is_ignored() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_session(Some(session_with_n_activities("o/r#1", 3)));
    // Upsert arrives for a DIFFERENT session — must not replace ours.
    rp.on_event(&Event::SessionUpserted(Box::new(session_with_n_activities(
        "o/r#99", 10,
    ))));
    assert_eq!(
        rp.selected_session().unwrap().task_id.key,
        "o/r#1",
        "unrelated upsert ignored"
    );
}

#[test]
fn unrelated_events_do_not_perturb_state() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_session(Some(session_with_n_activities("o/r#1", 3)));
    rp.on_event(&Event::Notification {
        title: "hi".into(),
        body: "there".into(),
    });
    rp.on_event(&Event::refresh_stub());
    assert_eq!(rp.comment_cursor(), 0);
    assert!(rp.selected_session().is_some());
}

trait EventStub {
    fn refresh_stub() -> Event;
}
impl EventStub for Event {
    fn refresh_stub() -> Event {
        Event::ProviderError {
            source: "x".into(),
            message: "y".into(),
        }
    }
}

// ── handle_key never emits commands ────────────────────────────────────

#[test]
fn key_handling_never_emits_commands() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_session(Some(session_with_n_activities("o/r#1", 3)));
    let mut cmds = Vec::new();
    for c in ['j', 'k', 'g', 'G', 'x'] {
        rp.handle_key(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            &mut cmds,
        );
    }
    // RightPane is read-only today; any command emission would be
    // surprising and likely a bug. If a future change adds Space→
    // select comments etc., this test updates alongside it.
    assert!(cmds.is_empty());
}

// ── Rendering ──────────────────────────────────────────────────────────

#[test]
fn render_empty_shows_placeholder() {
    let mut rp = RightPane::new(ComponentId::new(1));
    let rendered = render_to_string(&mut rp, 60, 20, true);
    assert!(
        rendered.contains("no session selected"),
        "placeholder visible when no session; got:\n{rendered}"
    );
}

#[test]
fn render_shows_header_fields() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_session(Some(session_with_n_activities("o/r#1", 2)));
    let rendered = render_to_string(&mut rp, 60, 20, true);
    assert!(rendered.contains("OPEN"), "state tag; got:\n{rendered}");
    assert!(rendered.contains("PR o/r#1"), "title; got:\n{rendered}");
    assert!(rendered.contains("feature/x"), "branch; got:\n{rendered}");
    assert!(rendered.contains("Reviewer"), "role; got:\n{rendered}");
    assert!(
        rendered.contains("alice"),
        "reviewers list; got:\n{rendered}"
    );
}

#[test]
fn render_shows_activity_count_in_title() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_session(Some(session_with_n_activities("o/r#1", 7)));
    let rendered = render_to_string(&mut rp, 60, 20, true);
    assert!(
        rendered.contains("Activity (7)"),
        "count in title; got:\n{rendered}"
    );
}

#[test]
fn render_lists_comments() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_session(Some(session_with_n_activities("o/r#1", 3)));
    let rendered = render_to_string(&mut rp, 60, 20, true);
    // Each comment shows "user0: comment body 0" style, with unread
    // marker and kind tag.
    assert!(rendered.contains("user0"), "first author; got:\n{rendered}");
    assert!(
        rendered.contains("comment body 0"),
        "first body; got:\n{rendered}"
    );
}

#[test]
fn render_empty_activity_shows_placeholder() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_session(Some(session_with_n_activities("o/r#1", 0)));
    let rendered = render_to_string(&mut rp, 60, 20, true);
    assert!(
        rendered.contains("no activity"),
        "empty-state placeholder; got:\n{rendered}"
    );
}
