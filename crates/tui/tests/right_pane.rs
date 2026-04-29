//! Tests for RightPane. The hierarchy step under test is **Workspace
//! → primary task** — the right pane projects the selected workspace
//! down to its headline task and its merged activity feed.
//!
//! Coverage:
//! - Selection (set_workspace stores, replaces, clears).
//! - Cursor preservation when re-setting the same workspace.
//! - Comment navigation (j/k/g/G, bounds).
//! - Event handling (WorkspaceUpserted refreshes the current row).
//! - Render: header (state + branch only) and activity list.

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pilot_core::{
    Activity, ActivityKind, CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace,
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

fn workspace_with_n_activities(key: &str, n: usize) -> Workspace {
    let mut w = Workspace::from_task(make_task(key), Utc::now());
    for i in 0..n {
        w.activity.push(activity(
            &format!("user{i}"),
            &format!("comment body {i}"),
            ActivityKind::Comment,
        ));
    }
    w.sort_activity();
    w
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

// ── Selection ─────────────────────────────────────────────────────────

#[test]
fn set_workspace_stores_it() {
    let mut rp = RightPane::new(ComponentId::new(1));
    assert!(rp.selected_workspace().is_none());
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 0)));
    assert!(rp.selected_workspace().is_some());
}

#[test]
fn set_workspace_to_different_resets_cursor() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 5)));
    for _ in 0..3 {
        rp.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut Vec::new(),
        );
    }
    assert_eq!(rp.comment_cursor(), 3);

    rp.set_workspace(Some(workspace_with_n_activities("o/r#2", 5)));
    assert_eq!(
        rp.comment_cursor(),
        0,
        "cursor resets when workspace changes"
    );
}

#[test]
fn set_workspace_to_same_preserves_cursor() {
    let mut rp = RightPane::new(ComponentId::new(1));
    let ws = workspace_with_n_activities("o/r#1", 5);
    rp.set_workspace(Some(ws.clone()));
    for _ in 0..2 {
        rp.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut Vec::new(),
        );
    }
    assert_eq!(rp.comment_cursor(), 2);

    rp.set_workspace(Some(ws));
    assert_eq!(rp.comment_cursor(), 2);
}

#[test]
fn set_workspace_none_clears() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    rp.set_workspace(None);
    assert!(rp.selected_workspace().is_none());
}

// ── Comment navigation ────────────────────────────────────────────────

#[test]
fn j_moves_cursor_down_bounded() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    for _ in 0..10 {
        rp.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut Vec::new(),
        );
    }
    assert_eq!(rp.comment_cursor(), 2);
}

#[test]
fn k_moves_cursor_up_bounded() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
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
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 10)));
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
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 7)));
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
        &mut Vec::new(),
    );
    assert_eq!(rp.comment_cursor(), 6);
}

#[test]
fn j_without_workspace_bubbles_up() {
    let mut rp = RightPane::new(ComponentId::new(1));
    let outcome = rp.handle_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert_eq!(outcome, pilot_v2_tui::Outcome::BubbleUp);
}

#[test]
fn unknown_key_bubbles_up_even_with_workspace() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    let outcome = rp.handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert_eq!(outcome, pilot_v2_tui::Outcome::BubbleUp);
}

#[test]
fn j_on_empty_activity_list_consumes() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 0)));
    let outcome = rp.handle_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert_eq!(outcome, pilot_v2_tui::Outcome::Consumed);
    assert_eq!(rp.comment_cursor(), 0);
}

// ── Events ────────────────────────────────────────────────────────────

#[test]
fn workspace_upserted_for_current_updates_state() {
    let mut rp = RightPane::new(ComponentId::new(1));
    let mut ws = workspace_with_n_activities("o/r#1", 3);
    rp.set_workspace(Some(ws.clone()));
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
        &mut Vec::new(),
    );
    assert_eq!(rp.comment_cursor(), 2);

    ws.activity
        .push(activity("bob", "new one", ActivityKind::Comment));
    ws.sort_activity();
    rp.on_event(&Event::WorkspaceUpserted(Box::new(ws)));
    assert_eq!(rp.comment_cursor(), 2);
    assert_eq!(rp.selected_workspace().unwrap().activity.len(), 4);
}

#[test]
fn workspace_upserted_shrinks_clamps_cursor() {
    let mut rp = RightPane::new(ComponentId::new(1));
    let ws = workspace_with_n_activities("o/r#1", 5);
    rp.set_workspace(Some(ws));
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
        &mut Vec::new(),
    );
    assert_eq!(rp.comment_cursor(), 4);

    let smaller = workspace_with_n_activities("o/r#1", 2);
    rp.on_event(&Event::WorkspaceUpserted(Box::new(smaller)));
    assert_eq!(rp.comment_cursor(), 1);
}

#[test]
fn workspace_upserted_for_different_workspace_is_ignored() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    rp.on_event(&Event::WorkspaceUpserted(Box::new(
        workspace_with_n_activities("o/r#99", 10),
    )));
    assert_eq!(
        rp.selected_workspace()
            .and_then(|w| w.primary_task())
            .map(|t| t.id.key.as_str()),
        Some("o/r#1"),
        "unrelated upsert ignored"
    );
}

#[test]
fn unrelated_events_do_not_perturb_state() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    rp.on_event(&Event::Notification {
        title: "hi".into(),
        body: "there".into(),
    });
    rp.on_event(&Event::ProviderError {
        source: "x".into(),
        message: "y".into(),
    });
    assert_eq!(rp.comment_cursor(), 0);
    assert!(rp.selected_workspace().is_some());
}

// ── handle_key never emits commands ───────────────────────────────────

#[test]
fn key_handling_never_emits_commands() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    let mut cmds = Vec::new();
    for c in ['j', 'k', 'g', 'G', 'x'] {
        rp.handle_key(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            &mut cmds,
        );
    }
    assert!(cmds.is_empty());
}

// ── Rendering ─────────────────────────────────────────────────────────

#[test]
fn render_empty_shows_placeholder() {
    let mut rp = RightPane::new(ComponentId::new(1));
    let rendered = render_to_string(&mut rp, 60, 20, true);
    assert!(rendered.contains("no session selected"));
}

#[test]
fn render_shows_header_fields() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 2)));
    let rendered = render_to_string(&mut rp, 60, 20, true);
    assert!(rendered.contains("OPEN"), "state tag");
    assert!(rendered.contains("PR o/r#1"), "title");
    assert!(rendered.contains("feature/x"), "branch");
    // Role/Review/Reviewers were intentionally dropped — see the
    // header simplification commit.
    assert!(!rendered.contains("Reviewer"), "role hidden");
    assert!(!rendered.contains("alice"), "reviewers hidden");
}

#[test]
fn render_shows_activity_count_in_title() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 7)));
    let rendered = render_to_string(&mut rp, 60, 20, true);
    assert!(rendered.contains("Activity (7)"));
}

#[test]
fn render_lists_comments() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    let rendered = render_to_string(&mut rp, 60, 20, true);
    assert!(rendered.contains("user0"));
    assert!(rendered.contains("comment body 0"));
}

#[test]
fn render_empty_activity_shows_placeholder() {
    let mut rp = RightPane::new(ComponentId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 0)));
    let rendered = render_to_string(&mut rp, 60, 20, true);
    assert!(rendered.contains("no activity"));
}
