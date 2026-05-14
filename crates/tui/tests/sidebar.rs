//! Sidebar behavior tests. Pinned model: **Repo → Workspace → Session
//! → Terminal**. The sidebar consumes WORKSPACE events from the
//! daemon and renders rows grouped by repo. Each test names the layer
//! it's exercising so a regression on one rung of the hierarchy is
//! easy to spot.
//!
//! Coverage:
//!
//! - Event handling (Snapshot / WorkspaceUpserted / WorkspaceRemoved).
//! - Repo grouping: header rows above their workspace rows; the
//!   cursor never lands on a header.
//! - Visibility filtering (Inbox vs Snoozed, merged/closed hidden).
//! - Sort order (updated_at desc within each repo group).
//! - Cursor preservation across re-sort / upsert / remove.
//! - All keybindings — each emits the expected Command.
//! - Kill two-press confirmation.
//! - Render output via ratatui's TestBackend.

use chrono::{DateTime, Duration, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pilot_core::{
    CiStatus, ReviewStatus, SessionKey, Task, TaskId, TaskRole, TaskState, Workspace, WorkspaceKey,
};
use pilot_ipc::{Command, Event, TerminalKind};
use pilot_tui::components::{Mailbox, Sidebar, sidebar::VisibleRow};
use pilot_tui::PaneId;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::prelude::Rect;

// ── Fixtures ───────────────────────────────────────────────────────────

fn make_task(repo: &str, key: &str, updated: DateTime<Utc>) -> Task {
    // The URL must contain `/pull/` for `Workspace::classify` to put
    // this task in the workspace's PR slot — issue paths land in
    // `gh_issues` instead and the assertions on `workspace.pr` fail.
    let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
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
        url: format!("https://github.com/{path}/pull/{num}"),
        repo: Some(repo.into()),
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
        closes_issues: vec![],
    }
}

fn make_workspace(repo: &str, key: &str, updated: DateTime<Utc>) -> Workspace {
    Workspace::from_task(make_task(repo, key, updated), updated)
}

/// Resolve the wire-side selection key for `task_key`. This is the
/// sanitized form `pilot_core::workspace_key_for` produces — tests
/// assert against this so they stay accurate when the sanitizer
/// changes.
fn expected_session_key(task_key: &str) -> String {
    pilot_core::workspace_key_for(&make_task("", task_key, Utc::now()))
}

fn key_code(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn shift_char(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
}

fn ws_key(workspace: &Workspace) -> SessionKey {
    SessionKey::new(workspace.key.as_str())
}

// ── Event handling ─────────────────────────────────────────────────────

#[test]
fn snapshot_populates_workspaces() {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let w1 = make_workspace("owner/repo", "o/r#1", now);
    let w2 = make_workspace("owner/repo", "o/r#2", now - Duration::hours(1));
    s.on_event(&Event::Snapshot {
        workspaces: vec![w1.clone(), w2],
        terminals: vec![],
    });
    assert_eq!(s.workspace_count(), 2);
    assert_eq!(s.selected_session_key(), Some(&ws_key(&w1)));
}

#[test]
fn workspace_upserted_inserts_then_updates_in_place() {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let w = make_workspace("owner/repo", "o/r#1", now);
    s.on_event(&Event::WorkspaceUpserted(Box::new(w)));
    assert_eq!(s.workspace_count(), 1);

    // Same key, newer timestamp, renamed: same row, name updated.
    let mut updated = make_workspace("owner/repo", "o/r#1", now + Duration::minutes(5));
    updated.name = "renamed".into();
    s.on_event(&Event::WorkspaceUpserted(Box::new(updated.clone())));
    assert_eq!(s.workspace_count(), 1);
    assert_eq!(
        s.selected_workspace().map(|w| w.name.as_str()),
        Some("renamed")
    );
}

#[test]
fn workspace_removed_prunes_and_clamps_cursor() {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let w1 = make_workspace("owner/repo", "o/r#1", now);
    let w2 = make_workspace("owner/repo", "o/r#2", now - Duration::hours(1));
    s.on_event(&Event::Snapshot {
        workspaces: vec![w1, w2.clone()],
        terminals: vec![],
    });
    // Move cursor to second workspace row.
    s.handle_key(key_code(KeyCode::Char('j')), &mut Vec::new());
    assert_eq!(s.selected_session_key(), Some(&ws_key(&w2)));

    s.on_event(&Event::WorkspaceRemoved(w2.key.clone()));
    assert_eq!(s.workspace_count(), 1);
    // Cursor falls back to the only remaining workspace.
    assert_eq!(
        s.selected_session_key().map(|k| k.to_string()),
        Some(expected_session_key("o/r#1"))
    );
}

#[test]
fn cursor_follows_workspace_key_across_resort() {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let w1 = make_workspace("owner/repo", "o/r#1", now);
    let w2 = make_workspace("owner/repo", "o/r#2", now - Duration::hours(1));
    let w3 = make_workspace("owner/repo", "o/r#3", now - Duration::hours(2));
    s.on_event(&Event::Snapshot {
        workspaces: vec![w1, w2.clone(), w3.clone()],
        terminals: vec![],
    });
    // Cursor on #2.
    s.handle_key(key_code(KeyCode::Char('j')), &mut Vec::new());
    assert_eq!(s.selected_session_key(), Some(&ws_key(&w2)));

    // #3 jumps to top with a new updated_at — cursor stays on #2.
    let mut bumped = w3.clone();
    if let Some(t) = bumped.pr.as_mut() {
        t.updated_at = now + Duration::hours(1);
    }
    s.on_event(&Event::WorkspaceUpserted(Box::new(bumped)));
    assert_eq!(
        s.selected_session_key(),
        Some(&ws_key(&w2)),
        "cursor follows the workspace key across re-sort"
    );
}

#[test]
fn merged_workspace_hidden() {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let mut merged = make_workspace("owner/repo", "o/r#1", now);
    if let Some(t) = merged.pr.as_mut() {
        t.state = TaskState::Merged;
    }
    let live = make_workspace("owner/repo", "o/r#2", now);
    s.on_event(&Event::Snapshot {
        workspaces: vec![merged, live.clone()],
        terminals: vec![],
    });
    assert_eq!(s.workspace_count(), 1);
    assert_eq!(s.selected_session_key(), Some(&ws_key(&live)));
}

// ── Repo grouping (the hierarchy) ──────────────────────────────────────

#[test]
fn rows_are_grouped_by_repo_with_headers() {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    s.on_event(&Event::Snapshot {
        workspaces: vec![
            make_workspace("owner/alpha", "alpha#1", now),
            make_workspace("owner/beta", "beta#1", now),
            make_workspace("owner/alpha", "alpha#2", now - Duration::hours(1)),
        ],
        terminals: vec![],
    });
    let rows = s.visible_rows();
    // Hierarchy: alpha header → its 2 workspaces → beta header → its 1.
    let header_indexes: Vec<_> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| matches!(r, VisibleRow::RepoHeader(_)).then_some(i))
        .collect();
    assert_eq!(header_indexes, vec![0, 3], "headers at expected positions");
    match &rows[0] {
        VisibleRow::RepoHeader(name) => assert_eq!(name, "owner/alpha"),
        _ => panic!("expected alpha header first"),
    }
    match &rows[3] {
        VisibleRow::RepoHeader(name) => assert_eq!(name, "owner/beta"),
        _ => panic!("expected beta header second"),
    }
}

#[test]
fn cursor_walks_through_repo_headers() {
    // j/k now stop on repo headers too — needed so users can land
    // on a collapsed header and Space-to-expand. Header rows have
    // no session key (selected_session_key is None on them).
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    s.on_event(&Event::Snapshot {
        workspaces: vec![
            make_workspace("owner/alpha", "alpha#1", now),
            make_workspace("owner/beta", "beta#1", now),
        ],
        terminals: vec![],
    });
    // Layout: [alpha header, alpha#1, beta header, beta#1]. Cursor
    // starts on alpha#1. j → beta header → beta#1.
    assert_eq!(
        s.selected_session_key().map(|k| k.to_string()),
        Some(expected_session_key("alpha#1"))
    );
    s.handle_key(key_code(KeyCode::Char('j')), &mut Vec::new());
    assert!(s.selected_session_key().is_none(), "cursor on beta header");
    s.handle_key(key_code(KeyCode::Char('j')), &mut Vec::new());
    assert_eq!(
        s.selected_session_key().map(|k| k.to_string()),
        Some(expected_session_key("beta#1"))
    );
}

// ── Mailbox ────────────────────────────────────────────────────────────

#[test]
fn snoozed_workspace_hidden_from_inbox() {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let mut snoozed = make_workspace("owner/repo", "o/r#1", now);
    snoozed.snoozed_until = Some(now + Duration::hours(4));
    s.on_event(&Event::Snapshot {
        workspaces: vec![snoozed, make_workspace("owner/repo", "o/r#2", now)],
        terminals: vec![],
    });
    assert_eq!(s.workspace_count(), 1);
    assert_eq!(s.mailbox(), Mailbox::Inbox);
}

#[test]
fn toggle_mailbox_cycles_inbox_inactive_snoozed() {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let mut snoozed = make_workspace("owner/repo", "o/r#1", now);
    snoozed.snoozed_until = Some(now + Duration::hours(4));
    s.on_event(&Event::Snapshot {
        workspaces: vec![snoozed, make_workspace("owner/repo", "o/r#2", now)],
        terminals: vec![],
    });
    // Cycle: Inbox → Inactive → Snoozed → Inbox.
    assert_eq!(s.mailbox(), Mailbox::Inbox);
    s.handle_key(shift_char('S'), &mut Vec::new());
    assert_eq!(s.mailbox(), Mailbox::Inactive);
    s.handle_key(shift_char('S'), &mut Vec::new());
    assert_eq!(s.mailbox(), Mailbox::Snoozed);
    assert_eq!(
        s.selected_session_key().map(|k| k.to_string()),
        Some(expected_session_key("o/r#1"))
    );
    s.handle_key(shift_char('S'), &mut Vec::new());
    assert_eq!(s.mailbox(), Mailbox::Inbox);
    assert_eq!(
        s.selected_session_key().map(|k| k.to_string()),
        Some(expected_session_key("o/r#2"))
    );
}

#[test]
fn inactive_mailbox_shows_merged_and_closed_workspaces() {
    // The whole point of Inactive: surface workspaces whose primary
    // task is merged or closed. Without this view those rows just
    // disappeared from the inbox after a merge.
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let mut merged = make_workspace("owner/repo", "merged#1", now);
    if let Some(t) = merged.pr.as_mut() {
        t.state = TaskState::Merged;
    }
    let mut closed = make_workspace("owner/repo", "closed#1", now);
    if let Some(t) = closed.pr.as_mut() {
        t.state = TaskState::Closed;
    }
    let live = make_workspace("owner/repo", "live#1", now);
    s.on_event(&Event::Snapshot {
        workspaces: vec![merged, closed, live],
        terminals: vec![],
    });
    // Inbox has only the live workspace.
    assert_eq!(s.workspace_count(), 1);

    // Inactive surfaces both the merged and the closed.
    s.handle_key(shift_char('S'), &mut Vec::new());
    assert_eq!(s.mailbox(), Mailbox::Inactive);
    assert_eq!(s.workspace_count(), 2);
}

// ── Keybindings → commands ─────────────────────────────────────────────

fn populated_sidebar() -> Sidebar {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    s.on_event(&Event::Snapshot {
        workspaces: vec![
            make_workspace("owner/repo", "o/r#1", now),
            make_workspace("owner/repo", "o/r#2", now - Duration::hours(1)),
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
            assert_eq!(session_key.to_string(), expected_session_key("o/r#1"));
            assert_eq!(agent, "claude");
        }
        other => panic!("expected Spawn Agent(claude), got {other:?}"),
    }
}

#[test]
fn x_emits_spawn_codex_for_selected() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Char('x')), &mut cmds);
    assert_eq!(cmds.len(), 1);
    match &cmds[0] {
        Command::Spawn {
            session_key,
            kind: TerminalKind::Agent(agent),
            ..
        } => {
            assert_eq!(session_key.to_string(), expected_session_key("o/r#1"));
            assert_eq!(agent, "codex", "x maps to Codex by default");
        }
        other => panic!("expected Spawn Agent(codex), got {other:?}"),
    }
}

#[test]
fn custom_agent_shortcuts_override_defaults() {
    let mut s = Sidebar::new(PaneId::new(1))
        .with_agent_shortcuts([('c', "claude".into()), ('a', "aider".into())]);
    let now = Utc::now();
    s.on_event(&Event::WorkspaceUpserted(Box::new(make_workspace(
        "owner/repo",
        "o/r#1",
        now,
    ))));

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

    // `x` is no longer mapped in the custom set — bubbles up.
    let mut cmds = Vec::new();
    let outcome = s.handle_key(key_code(KeyCode::Char('x')), &mut cmds);
    assert_eq!(
        outcome,
        pilot_tui::PaneOutcome::Pass,
        "unmapped key bubbles, doesn't spawn a random default"
    );
    assert!(cmds.is_empty());
}

#[test]
fn c_on_empty_sidebar_emits_nothing() {
    let mut s = Sidebar::new(PaneId::new(1));
    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Char('c')), &mut cmds);
    assert!(cmds.is_empty());
}

#[test]
fn s_emits_spawn_shell() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Char('s')), &mut cmds);
    assert!(matches!(
        cmds.as_slice(),
        [Command::Spawn {
            kind: TerminalKind::Shell,
            ..
        }]
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
            assert_eq!(session_key.to_string(), expected_session_key("o/r#1"));
        }
        other => panic!("expected MarkRead, got {other:?}"),
    }
}

#[test]
fn g_emits_refresh_without_selection() {
    let mut s = Sidebar::new(PaneId::new(1));
    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Char('g')), &mut cmds);
    assert!(matches!(cmds.as_slice(), [Command::Refresh]));
}

// ── Snooze semantics ───────────────────────────────────────────────────

#[test]
fn z_snoozes_unsnoozed_for_4h() {
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
            assert!(*until >= min && *until <= max);
        }
        other => panic!("expected Snooze, got {other:?}"),
    }
}

#[test]
fn z_unsnoozes_already_snoozed() {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let mut snoozed = make_workspace("owner/repo", "o/r#1", now);
    snoozed.snoozed_until = Some(now + Duration::hours(4));
    s.on_event(&Event::Snapshot {
        workspaces: vec![snoozed],
        terminals: vec![],
    });
    // Inbox → Inactive → Snoozed (3-state cycle).
    s.handle_key(shift_char('S'), &mut Vec::new());
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

    s.handle_key(shift_char('X'), &mut cmds);
    assert!(cmds.is_empty());
    assert_eq!(
        s.kill_armed().map(|k| k.to_string()),
        Some(expected_session_key("o/r#1"))
    );

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
    s.handle_key(key_code(KeyCode::Char('j')), &mut cmds);
    assert!(s.kill_armed().is_none());
    s.handle_key(shift_char('X'), &mut cmds);
    assert_eq!(
        cmds.iter()
            .filter(|c| matches!(c, Command::Kill { .. }))
            .count(),
        0,
        "Kill must not have fired"
    );
}

#[test]
fn shift_x_disarmed_when_moved_to_different_workspace() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    s.handle_key(shift_char('X'), &mut cmds);
    s.handle_key(key_code(KeyCode::Char('j')), &mut cmds);
    assert!(s.kill_armed().is_none());
    s.handle_key(shift_char('X'), &mut cmds);
    assert_eq!(
        s.kill_armed().map(|k| k.to_string()),
        Some(expected_session_key("o/r#2"))
    );
}

// ── Navigation bounds ─────────────────────────────────────────────────

#[test]
fn j_stops_at_last_workspace() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    for _ in 0..10 {
        s.handle_key(key_code(KeyCode::Char('j')), &mut cmds);
    }
    assert_eq!(
        s.selected_session_key().map(|k| k.to_string()),
        Some(expected_session_key("o/r#2"))
    );
}

#[test]
fn k_stops_at_top_row() {
    // After repeatedly pressing k from any row, the cursor lands
    // on the top of the visible list. With the collapse-aware nav
    // that's the repo header — assert via `cursor_on_repo_header`
    // because `selected_session_key` is None on a header.
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Char('j')), &mut cmds);
    for _ in 0..10 {
        s.handle_key(key_code(KeyCode::Char('k')), &mut cmds);
    }
    assert!(
        s.cursor_on_repo_header(),
        "k repeatedly should leave the cursor on the top repo header, not a workspace"
    );
}

// ── Bubble-up ──────────────────────────────────────────────────────────

#[test]
fn unknown_key_bubbles_up() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    let outcome = s.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &mut cmds);
    assert_eq!(outcome, pilot_tui::PaneOutcome::Pass);
    assert!(cmds.is_empty());
}

// ── Render ─────────────────────────────────────────────────────────────

fn render_to_string(s: &mut Sidebar, width: u16, height: u16, focused: bool) -> String {
    let backend = TestBackend::new(width, height);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|frame| {
        s.render(Rect::new(0, 0, width, height), frame, focused);
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

#[test]
fn render_smoke_has_mailbox_label_and_grouped_rows() {
    let mut s = populated_sidebar();
    let rendered = render_to_string(&mut s, 40, 12, true);
    // V1-style brand label: `PILOT` for the Inbox mailbox.
    assert!(rendered.contains("PILOT"));
    assert!(rendered.contains('2'), "row count in title");
    assert!(rendered.contains("owner/repo"), "repo header rendered");
    assert!(rendered.contains("task: o/r#1"), "first workspace visible");
}

#[test]
fn render_shows_cursor_marker_on_selected_workspace() {
    let mut s = populated_sidebar();
    let rendered = render_to_string(&mut s, 40, 10, true);
    let cursor_line = rendered
        .lines()
        .find(|l| l.contains('▸'))
        .unwrap_or_else(|| panic!("expected cursor marker; got:\n{rendered}"));
    assert!(cursor_line.contains("o/r#1"));
}

#[test]
fn render_mailbox_toggles_title() {
    let mut s = populated_sidebar();
    // PILOT → INACTIVE → SNOOZED; uppercase brand label per V1.
    s.handle_key(shift_char('S'), &mut Vec::new());
    let rendered = render_to_string(&mut s, 40, 12, true);
    assert!(rendered.contains("INACTIVE"));
    s.handle_key(shift_char('S'), &mut Vec::new());
    let rendered = render_to_string(&mut s, 40, 12, true);
    assert!(rendered.contains("SNOOZED"));
}

#[test]
fn render_shows_kill_marker_when_armed() {
    let mut s = populated_sidebar();
    s.handle_key(shift_char('X'), &mut Vec::new());
    let rendered = render_to_string(&mut s, 40, 10, true);
    assert!(rendered.contains("[kill?]"));
}

// ── Hierarchy invariant: WorkspaceKey ↔ SessionKey conversions ────────

#[test]
fn workspace_key_round_trips_through_session_key() {
    // The wire-side selection key is `SessionKey`, but the values
    // flowing through it are workspace keys. Round-trip both ways
    // because every Sidebar lookup hits this conversion.
    let wk = WorkspaceKey::new("owner/repo:42");
    let sk: SessionKey = (&wk).into();
    assert_eq!(sk.as_str(), wk.as_str());
}

// ── Workspace ↔ Session expansion (the user-facing rule) ─────────────

use pilot_core::{SessionKind, WorkspaceSession};
use std::path::PathBuf;

fn add_session(workspace: &mut Workspace, name: &str) -> pilot_core::SessionId {
    let mut s = WorkspaceSession::new(
        workspace.key.clone(),
        SessionKind::Shell,
        PathBuf::from(format!("/tmp/{name}")),
        Utc::now(),
    );
    s.name = name.into();
    workspace.add_session(s)
}

#[test]
fn workspace_with_one_session_does_not_show_a_subrow() {
    // 99% of workspaces have a single session — duplicating it as
    // its own row is visual noise. The runner badge on the workspace
    // row already conveys "this workspace has a live session".
    let mut s = Sidebar::new(PaneId::new(1));
    let mut w = make_workspace("owner/repo", "o/r#1", Utc::now());
    add_session(&mut w, "claude");
    s.on_event(&Event::Snapshot {
        workspaces: vec![w],
        terminals: vec![],
    });
    let session_rows = s
        .visible_rows()
        .iter()
        .filter(|r| matches!(r, VisibleRow::Session { .. }))
        .count();
    assert_eq!(session_rows, 0, "one session → no separate sub-row");
}

#[test]
fn workspace_with_two_sessions_expands_into_subrows() {
    // Crossing the threshold from 1 → 2 sessions makes the workspace
    // visually expand: the workspace row stays, plus one Session
    // sub-row per session.
    let mut s = Sidebar::new(PaneId::new(1));
    let mut w = make_workspace("owner/repo", "o/r#1", Utc::now());
    add_session(&mut w, "claude");
    add_session(&mut w, "shell");
    s.on_event(&Event::Snapshot {
        workspaces: vec![w],
        terminals: vec![],
    });
    let session_rows: Vec<_> = s
        .visible_rows()
        .iter()
        .filter(|r| matches!(r, VisibleRow::Session { .. }))
        .collect();
    assert_eq!(session_rows.len(), 2, "two Session sub-rows for 2 sessions");
}

#[test]
fn cursor_can_land_on_a_session_subrow() {
    // With 2+ sessions, j moves the cursor through the session
    // sub-rows. selected_session_id surfaces which one.
    let mut s = Sidebar::new(PaneId::new(1));
    let mut w = make_workspace("owner/repo", "o/r#1", Utc::now());
    let s0 = add_session(&mut w, "claude");
    let s1 = add_session(&mut w, "shell");
    s.on_event(&Event::Snapshot {
        workspaces: vec![w],
        terminals: vec![],
    });
    // Cursor starts on the workspace row. Down once → session 0.
    s.handle_key(key_code(KeyCode::Char('j')), &mut Vec::new());
    assert_eq!(s.selected_session_id(), Some(s0));
    s.handle_key(key_code(KeyCode::Char('j')), &mut Vec::new());
    assert_eq!(s.selected_session_id(), Some(s1));
    // Workspace row's selected_session_id is None — the daemon
    // resolves which session to use.
    s.handle_key(key_code(KeyCode::Char('k')), &mut Vec::new());
    s.handle_key(key_code(KeyCode::Char('k')), &mut Vec::new());
    assert_eq!(s.selected_session_id(), None);
}

#[test]
fn session_created_event_expands_into_subrows_at_two() {
    // The user has a workspace with 1 session, hits `c` to spawn
    // Claude into a second session. The daemon emits SessionCreated;
    // the sidebar crosses the 1→2 threshold and now shows one Session
    // sub-row per session so the user can pick between them.
    let mut s = Sidebar::new(PaneId::new(1));
    let mut w = make_workspace("owner/repo", "o/r#1", Utc::now());
    add_session(&mut w, "shell");
    s.on_event(&Event::Snapshot {
        workspaces: vec![w.clone()],
        terminals: vec![],
    });
    assert_eq!(
        s.visible_rows()
            .iter()
            .filter(|r| matches!(r, VisibleRow::Session { .. }))
            .count(),
        0,
        "single-session workspaces collapse — runner badge handles them"
    );

    let new_session = WorkspaceSession::new(
        w.key.clone(),
        SessionKind::Agent {
            agent_id: "claude".into(),
        },
        PathBuf::from("/tmp/claude"),
        Utc::now(),
    );
    s.on_event(&Event::SessionCreated(Box::new(new_session)));
    assert_eq!(
        s.visible_rows()
            .iter()
            .filter(|r| matches!(r, VisibleRow::Session { .. }))
            .count(),
        2,
        "expanded to two sub-rows once the workspace had two sessions"
    );
}

#[test]
fn session_ended_event_collapses_back_below_two() {
    // 2 → 1 sessions: the workspace drops back to a single workspace
    // row with no Session sub-rows. The remaining session is implicit.
    let mut s = Sidebar::new(PaneId::new(1));
    let mut w = make_workspace("owner/repo", "o/r#1", Utc::now());
    add_session(&mut w, "shell");
    let claude_id = add_session(&mut w, "claude");
    s.on_event(&Event::Snapshot {
        workspaces: vec![w.clone()],
        terminals: vec![],
    });
    assert_eq!(
        s.visible_rows()
            .iter()
            .filter(|r| matches!(r, VisibleRow::Session { .. }))
            .count(),
        2
    );

    s.on_event(&Event::SessionEnded {
        workspace_key: w.key.clone(),
        session_id: claude_id,
    });
    assert_eq!(
        s.visible_rows()
            .iter()
            .filter(|r| matches!(r, VisibleRow::Session { .. }))
            .count(),
        0,
        "single survivor → workspace row alone, no sub-rows"
    );
}

#[test]
fn subscribed_repo_with_no_workspace_still_renders_a_header() {
    // The "I added a repo but the sidebar is empty" UX bug: until
    // polling finds open PRs/issues, no workspace exists for the
    // new repo, so the old render code emitted no row at all.
    // After apply_subscribed_scopes, an empty header should appear.
    let mut s = Sidebar::new(PaneId::new(1));

    // Empty snapshot — no workspaces at all.
    s.on_event(&Event::Snapshot {
        workspaces: vec![],
        terminals: vec![],
    });
    let scopes: std::collections::BTreeSet<String> =
        ["github:fresh-org/new-repo".to_string()].into_iter().collect();
    s.apply_subscribed_scopes(&scopes);

    // The visible list should contain a RepoHeader for the new
    // repo even though there's no workspace under it.
    let names: Vec<&str> = s
        .visible_rows()
        .iter()
        .filter_map(|r| match r {
            pilot_tui::components::sidebar::VisibleRow::RepoHeader(name) => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        names.contains(&"fresh-org/new-repo"),
        "expected an empty header for the subscribed repo, got: {names:?}"
    );
}

#[test]
fn subscribed_org_level_scope_does_not_render_a_header() {
    // Subscribing to a whole org (no slash in the scope id after
    // the provider prefix) means "all repos under this org" — we
    // can't render a repo header for it because we don't know the
    // repo names. Headers materialize as polling discovers them.
    let mut s = Sidebar::new(PaneId::new(1));
    s.on_event(&Event::Snapshot {
        workspaces: vec![],
        terminals: vec![],
    });
    let scopes: std::collections::BTreeSet<String> =
        ["github:some-org".to_string()].into_iter().collect();
    s.apply_subscribed_scopes(&scopes);

    let headers = s
        .visible_rows()
        .iter()
        .filter(|r| matches!(r, pilot_tui::components::sidebar::VisibleRow::RepoHeader(_)))
        .count();
    assert_eq!(headers, 0, "org-level scope should NOT produce a header");
}

// ── `f` / `w` agent-spawn targeting ──────────────────────────────────

fn issue_task(repo: &str, key: &str, body: Option<&str>) -> Task {
    let mut t = make_task(repo, key, Utc::now());
    let num = key.rsplit_once('#').map(|(_, n)| n).unwrap_or("1");
    t.url = format!("https://github.com/{repo}/issues/{num}");
    t.body = body.map(str::to_string);
    t
}

fn pr_task_with_ci(repo: &str, key: &str, ci: CiStatus) -> Task {
    let mut t = make_task(repo, key, Utc::now());
    t.ci = ci;
    t
}

#[test]
fn fix_target_fires_only_when_ci_is_failing() {
    // `f` is the narrow CI-fix mnemonic. PRs with green / running
    // CI must NOT advertise the binding — otherwise the hint bar
    // would lie and pressing `f` would no-op.
    let mut s = Sidebar::new(PaneId::new(1));
    let pr = pr_task_with_ci("o/r", "o/r#1", CiStatus::Success);
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(pr, Utc::now())],
        terminals: vec![],
    });
    assert!(s.fix_target_for_cursor().is_none());

    let mut s = Sidebar::new(PaneId::new(1));
    let pr = pr_task_with_ci("o/r", "o/r#2", CiStatus::Failure);
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(pr, Utc::now())],
        terminals: vec![],
    });
    let (_, prompt) = s.fix_target_for_cursor().expect("Failure CI must fire");
    assert!(prompt.contains("CI is failing"), "prompt: {prompt}");
}

#[test]
fn work_target_fires_for_ci_failure_same_as_fix() {
    // `w` is the polymorphic "work on this" key — it should subsume
    // the CI-failure case so users can use one key everywhere.
    let mut s = Sidebar::new(PaneId::new(1));
    let pr = pr_task_with_ci("o/r", "o/r#3", CiStatus::Failure);
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(pr, Utc::now())],
        terminals: vec![],
    });
    let fix = s.fix_target_for_cursor();
    let work = s.work_target_for_cursor();
    assert!(work.is_some());
    assert_eq!(
        work.map(|(_, p)| p),
        fix.map(|(_, p)| p),
        "w on a CI-failing PR must produce the same prompt as f",
    );
}

#[test]
fn work_target_fires_for_issue_with_implement_prompt() {
    let mut s = Sidebar::new(PaneId::new(1));
    let issue = issue_task("o/r", "o/r#42", Some("Stack overflow when …"));
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(issue, Utc::now())],
        terminals: vec![],
    });
    let (_, prompt) = s
        .work_target_for_cursor()
        .expect("issue must produce a work target");
    assert!(
        prompt.contains("Implement GitHub issue #42"),
        "prompt: {prompt}"
    );
    assert!(
        prompt.contains("Closes #42"),
        "prompt must instruct the agent to close the issue: {prompt}"
    );
    assert!(
        prompt.contains("Stack overflow when"),
        "prompt must include the issue body: {prompt}"
    );
}

#[test]
fn work_target_skips_passing_pr_with_no_action() {
    // PR exists, CI green, no review issues — nothing to "work
    // on". `w` must hide itself so the hint bar stays honest.
    let mut s = Sidebar::new(PaneId::new(1));
    let pr = pr_task_with_ci("o/r", "o/r#5", CiStatus::Success);
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(pr, Utc::now())],
        terminals: vec![],
    });
    assert!(s.work_target_for_cursor().is_none());
}

#[test]
fn work_key_emits_spawn_command_on_issue() {
    // End-to-end: pressing `w` on an issue row emits a Spawn(Agent)
    // command with the implement-issue prompt baked in.
    let mut s = Sidebar::new(PaneId::new(1));
    let issue = issue_task("o/r", "o/r#7", Some("Migrate to Postgres 16"));
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(issue, Utc::now())],
        terminals: vec![],
    });

    let mut cmds: Vec<Command> = Vec::new();
    let _ = s.handle_key(key_code(KeyCode::Char('w')), &mut cmds);

    assert_eq!(cmds.len(), 1, "exactly one Spawn must fire");
    match &cmds[0] {
        Command::Spawn {
            kind,
            initial_prompt,
            ..
        } => {
            assert!(
                matches!(kind, TerminalKind::Agent(_)),
                "must spawn an agent (not shell), got {kind:?}",
            );
            let prompt = initial_prompt.as_deref().unwrap_or("");
            assert!(prompt.contains("Implement GitHub issue #7"), "{prompt}");
        }
        other => panic!("expected Spawn, got {other:?}"),
    }
}
