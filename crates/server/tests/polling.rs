//! Tests for `polling::tick` / `polling::upsert` / bus integration.
//!
//! These exercise the contract:
//! 1. tick(source) → upsert(each task) → SessionUpserted broadcast.
//! 2. Read state (seen_count, read_indices, last_viewed_at,
//!    snoozed_until) is preserved across updates from the same task_id.
//! 3. Source errors surface as `Event::ProviderError` events; one bad
//!    source doesn't poison the others.
//! 4. The bus reaches a client connected through `Server::serve`.

use chrono::Utc;
use pilot_core::{
    Activity, ActivityKind, CiStatus, ProviderConfig, ReviewStatus, Task, TaskId, TaskRole,
    TaskState,
};
use pilot_store::WorkspaceRecord;
use pilot_ipc::{Command, Event, channel};
use pilot_server::polling::{self, TaskSource};
use pilot_server::{Server, ServerConfig};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn make_task(key: &str) -> Task {
    // The URL must contain `/pull/` for `Workspace::classify` to put
    // this task in the workspace's PR slot — otherwise it lands as
    // a GhIssue and the assertions on `workspace.pr` fail.
    let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
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
        url: format!("https://github.com/{path}/pull/{num}"),
        repo: Some("o/r".into()),
        branch: None,
        base_branch: None,
        updated_at: Utc::now(),
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

fn make_activity(author: &str, body: &str) -> Activity {
    Activity {
        author: author.into(),
        body: body.into(),
        created_at: Utc::now(),
        kind: ActivityKind::Comment,
        node_id: None,
        path: None,
        line: None,
        diff_hunk: None,
        thread_id: None,
    }
}

// ── Test fixtures ──────────────────────────────────────────────────

struct FakeSource {
    name: String,
    tasks: Vec<Task>,
}

impl TaskSource for FakeSource {
    fn name(&self) -> &str {
        &self.name
    }
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, pilot_core::ProviderError>> + Send + 'a>> {
        let tasks = self.tasks.clone();
        Box::pin(async move { Ok(tasks) })
    }
}

struct FailingSource(String);

impl TaskSource for FailingSource {
    fn name(&self) -> &str {
        &self.0
    }
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, pilot_core::ProviderError>> + Send + 'a>> {
        Box::pin(async move { Err(pilot_core::ProviderError::retryable(self.0.clone(), "rate limited")) })
    }
}

struct CountingSource {
    name: String,
    counter: Arc<AtomicUsize>,
}

impl TaskSource for CountingSource {
    fn name(&self) -> &str {
        &self.name
    }
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, pilot_core::ProviderError>> + Send + 'a>> {
        let counter = self.counter.clone();
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(vec![])
        })
    }
}

// ── tick() / upsert() ───────────────────────────────────────────────

#[tokio::test]
async fn tick_broadcasts_session_upserted_for_each_task() {
    let config = ServerConfig::in_memory();
    let mut bus_rx = config.bus.subscribe();

    let source: Box<dyn TaskSource> = Box::new(FakeSource {
        name: "github".into(),
        tasks: vec![make_task("o/r#1"), make_task("o/r#2")],
    });
    polling::tick(&config, &[source]).await;

    let mut keys = Vec::new();
    while let Ok(evt) = bus_rx.try_recv() {
        if let Event::WorkspaceUpserted(w) = evt {
            // Each PR projects to one workspace whose pr.id.key matches
            // the originating task key. The wire contract is that the
            // poller emits Workspace events, never Session events.
            keys.push(w.pr.as_ref().unwrap().id.key.clone());
        }
    }
    keys.sort();
    assert_eq!(keys, vec!["o/r#1", "o/r#2"]);
}

#[tokio::test]
async fn upsert_persists_to_store_so_subscribe_can_replay_it() {
    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_task("o/r#42")).await;

    // Now connect a client via channel::pair and Subscribe — the
    // Snapshot should include the just-upserted session.
    let (mut client, server) = channel::pair();
    let serve_config = config.clone();
    tokio::spawn(async move {
        Server::new(serve_config).serve(server).await.unwrap();
    });
    client.send(Command::Subscribe).unwrap();
    let evt = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("daemon replies")
        .expect("event");
    match evt {
        Event::Snapshot { workspaces, .. } => {
            assert_eq!(workspaces.len(), 1);
            assert_eq!(workspaces[0].pr.as_ref().unwrap().id.key, "o/r#42");
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}

#[tokio::test]
async fn upsert_preserves_seen_count_across_updates() {
    // The user marked the workspace read; the poller mustn't wipe
    // that out when GitHub returns the same PR again.
    let config = ServerConfig::in_memory();

    // Seed a workspace with seen_count=5 in the store directly.
    let task = make_task("o/r#7");
    let mut workspace = pilot_core::Workspace::from_task(task.clone(), Utc::now());
    workspace.seen_count = 5;
    let json = serde_json::to_string(&workspace).unwrap();
    config
        .store
        .save_workspace(&WorkspaceRecord {
            key: workspace.key.as_str().to_string(),
            created_at: workspace.created_at,
            workspace_json: Some(json),
        })
        .unwrap();

    // Poll re-discovers the same task. Read state must survive.
    polling::upsert(&config, task).await;

    let stored = config.store.get_workspace(&workspace.key).unwrap().unwrap();
    let parsed: pilot_core::Workspace =
        serde_json::from_str(&stored.workspace_json.unwrap()).unwrap();
    assert_eq!(parsed.seen_count, 5, "seen_count preserved");
}

#[tokio::test]
async fn upsert_de_duplicates_recent_activity() {
    // Provider returns the same activity entry on every poll. Without
    // de-dup, every tick would push another copy onto session.activity
    // and the unread-count would explode.
    let config = ServerConfig::in_memory();

    let activity_at = Utc::now();
    let mk = || {
        let mut t = make_task("o/r#1");
        t.recent_activity = vec![Activity {
            author: "alice".into(),
            body: "lgtm".into(),
            created_at: activity_at,
            kind: ActivityKind::Comment,
            node_id: None,
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        }];
        t
    };
    polling::upsert(&config, mk()).await;
    polling::upsert(&config, mk()).await;
    polling::upsert(&config, mk()).await;

    // Compute the workspace key the same way the poller does, then
    // round-trip through the store and verify the activity feed
    // didn't grow on every poll.
    let key = pilot_core::WorkspaceKey::new(pilot_core::workspace_key_for(&mk()));
    let stored = config.store.get_workspace(&key).unwrap().unwrap();
    let workspace: pilot_core::Workspace =
        serde_json::from_str(&stored.workspace_json.unwrap()).unwrap();
    assert_eq!(workspace.activity.len(), 1, "activity de-duplicated");
}

#[tokio::test]
async fn tick_emits_provider_error_on_failure() {
    let config = ServerConfig::in_memory();
    let mut bus_rx = config.bus.subscribe();

    let bad: Box<dyn TaskSource> = Box::new(FailingSource("github".into()));
    polling::tick(&config, &[bad]).await;

    let evt = bus_rx.try_recv().expect("error broadcasted");
    match evt {
        Event::ProviderError { source, message, .. } => {
            assert_eq!(source, "github");
            // Message on the bus is the user-facing one (terse) —
            // full diagnostic lives in /tmp/pilot.log. For a
            // Retryable error the user_message format is
            // "<source> hiccup, retrying next cycle".
            assert!(
                message.contains("hiccup") || message.contains("retrying"),
                "expected terse retryable user_message, got {message}"
            );
        }
        other => panic!("expected ProviderError, got {other:?}"),
    }
}

#[tokio::test]
async fn tick_continues_after_one_source_fails() {
    let config = ServerConfig::in_memory();
    let mut bus_rx = config.bus.subscribe();

    let bad: Box<dyn TaskSource> = Box::new(FailingSource("github".into()));
    let good: Box<dyn TaskSource> = Box::new(FakeSource {
        name: "linear".into(),
        tasks: vec![make_task("ENG-1")],
    });
    polling::tick(&config, &[bad, good]).await;

    let mut had_upsert = false;
    let mut had_error = false;
    while let Ok(evt) = bus_rx.try_recv() {
        match evt {
            Event::WorkspaceUpserted(_) => had_upsert = true,
            Event::ProviderError { .. } => had_error = true,
            _ => {}
        }
    }
    assert!(had_error, "failure broadcast");
    assert!(had_upsert, "successful source still ran");
}

// ── mark_workspace_read ──────────────────────────────────────────────
//
// Activity-seen state is local to the user — independent of provider
// state. mark_workspace_read flips every known activity item to read,
// persists, and broadcasts so the right pane re-renders without a
// pending unread badge.

#[tokio::test]
async fn mark_workspace_read_persists_seen_count() {
    let config = ServerConfig::in_memory();

    // Seed a workspace with three activity items, none read.
    let mut task = make_task("o/r#11");
    task.recent_activity = vec![
        make_activity("alice", "first"),
        make_activity("bob", "second"),
        make_activity("carol", "third"),
    ];
    polling::upsert(&config, task).await;

    let key = pilot_core::WorkspaceKey::new(pilot_core::workspace_key_for(&make_task(
        "o/r#11",
    )));
    let before: pilot_core::Workspace = serde_json::from_str(
        &config
            .store
            .get_workspace(&key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap();
    assert_eq!(before.activity.len(), 3);
    assert_eq!(before.unread_count(), 3, "everything unread initially");

    polling::mark_workspace_read(&config, &key);

    let after: pilot_core::Workspace = serde_json::from_str(
        &config
            .store
            .get_workspace(&key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap();
    assert_eq!(after.unread_count(), 0, "everything read after mark");
    assert_eq!(after.seen_count, 3, "seen_count bumped to activity len");
    assert!(after.last_viewed_at.is_some(), "last_viewed stamped");
}

#[tokio::test]
async fn mark_workspace_read_broadcasts_upsert() {
    let config = ServerConfig::in_memory();
    let (mut client, server) = channel::pair();
    let serve_config = config.clone();
    tokio::spawn(async move {
        Server::new(serve_config).serve(server).await.unwrap();
    });
    client.send(Command::Subscribe).unwrap();
    let _snap = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .unwrap();

    let mut task = make_task("o/r#22");
    task.recent_activity = vec![make_activity("alice", "hi-broadcast")];
    polling::upsert(&config, task).await;
    // Drain the upsert event from the initial seed.
    let _seed = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .unwrap();

    let key = pilot_core::WorkspaceKey::new(pilot_core::workspace_key_for(&make_task(
        "o/r#22",
    )));
    polling::mark_workspace_read(&config, &key);

    let evt = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("client receives mark-read upsert")
        .expect("event");
    match evt {
        Event::WorkspaceUpserted(w) => {
            assert_eq!(w.unread_count(), 0, "broadcast workspace is read");
        }
        other => panic!("expected WorkspaceUpserted, got {other:?}"),
    }
}

#[tokio::test]
async fn mark_workspace_read_is_independent_of_provider_state() {
    // Marking read is purely a local user gesture — no provider
    // metadata changes. After re-polling the same task, seen state
    // must survive (the upsert path preserves seen_count).
    let config = ServerConfig::in_memory();
    let mut task = make_task("o/r#33");
    task.recent_activity = vec![make_activity("alice", "ping")];
    polling::upsert(&config, task.clone()).await;

    let key = pilot_core::WorkspaceKey::new(pilot_core::workspace_key_for(&make_task(
        "o/r#33",
    )));
    polling::mark_workspace_read(&config, &key);

    // Re-poll the same task — seen state survives.
    polling::upsert(&config, task).await;

    let stored: pilot_core::Workspace = serde_json::from_str(
        &config
            .store
            .get_workspace(&key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap();
    assert_eq!(stored.unread_count(), 0, "still read after re-poll");
}

#[tokio::test]
async fn mark_workspace_read_no_op_when_workspace_missing() {
    // Pressing `m` on a workspace that the daemon doesn't actually have
    // (race: TUI saw a stale snapshot) must not panic.
    let config = ServerConfig::in_memory();
    let key = pilot_core::WorkspaceKey::new("github:o/r#nope");
    polling::mark_workspace_read(&config, &key);
    assert!(config.store.get_workspace(&key).unwrap().is_none());
}

// ── PR-attach migration ──────────────────────────────────────────────
//
// `migrate_session_paths_if_needed` walks the workspace's sessions
// and moves any whose persisted `worktree_path` no longer matches
// what the current slug would produce. The git-side `worktree move`
// needs a real bare clone to test honestly; these cover the
// orthogonal "path doesn't exist on disk" branch where the migration
// rewrites the record without doing I/O.

#[tokio::test]
async fn migrate_path_only_when_dir_missing() {
    use pilot_core::WorkspaceSession;
    let config = ServerConfig::in_memory();
    let task = make_task("o/r#11");
    let mut ws = pilot_core::Workspace::from_task(task, Utc::now());
    let session = WorkspaceSession::new(
        ws.key.clone(),
        pilot_core::SessionKind::Shell,
        std::path::PathBuf::from("/tmp/pilot-nonexistent-old-path"),
        Utc::now(),
    );
    ws.add_session(session);

    let moved = pilot_server::spawn_handler::migrate_session_paths_if_needed(
        &config, &mut ws,
    )
    .await;
    assert!(moved, "stale path detected → migrated record");

    let expected =
        pilot_server::spawn_handler::worktree_root().join(ws.worktree_slug());
    assert_eq!(
        ws.sessions[0].worktree_path, expected,
        "session path now matches the slug-derived path"
    );
}

#[tokio::test]
async fn migrate_no_op_when_path_already_matches() {
    use pilot_core::WorkspaceSession;
    let config = ServerConfig::in_memory();
    let task = make_task("o/r#22");
    let mut ws = pilot_core::Workspace::from_task(task, Utc::now());
    let expected = pilot_server::spawn_handler::worktree_root().join(ws.worktree_slug());
    let session = WorkspaceSession::new(
        ws.key.clone(),
        pilot_core::SessionKind::Shell,
        expected.clone(),
        Utc::now(),
    );
    ws.add_session(session);

    let moved = pilot_server::spawn_handler::migrate_session_paths_if_needed(
        &config, &mut ws,
    )
    .await;
    assert!(!moved, "path already matches → migration is a no-op");
    assert_eq!(ws.sessions[0].worktree_path, expected);
}

#[tokio::test]
async fn migrate_handles_zero_sessions() {
    let config = ServerConfig::in_memory();
    let task = make_task("o/r#33");
    let mut ws = pilot_core::Workspace::from_task(task, Utc::now());
    let moved = pilot_server::spawn_handler::migrate_session_paths_if_needed(
        &config, &mut ws,
    )
    .await;
    assert!(!moved, "no sessions → nothing to migrate");
}

// ── Create empty workspace (n key flow) ──────────────────────────────

#[tokio::test]
async fn create_empty_workspace_persists_with_user_name() {
    let config = ServerConfig::in_memory();
    let key = polling::create_empty_workspace(&config, "fix login flow");
    assert_eq!(
        key.as_str(),
        "fix-login-flow",
        "workspace key is the slugified name"
    );
    let stored: pilot_core::Workspace = serde_json::from_str(
        &config
            .store
            .get_workspace(&key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap();
    assert_eq!(stored.name, "fix login flow", "human-readable name kept");
    assert!(stored.pr.is_none(), "pre-PR workspace has no PR");
}

#[tokio::test]
async fn create_empty_workspace_disambiguates_collisions() {
    let config = ServerConfig::in_memory();
    let k1 = polling::create_empty_workspace(&config, "Refactor auth");
    let k2 = polling::create_empty_workspace(&config, "Refactor auth");
    let k3 = polling::create_empty_workspace(&config, "Refactor auth");
    assert_eq!(k1.as_str(), "refactor-auth");
    assert_eq!(k2.as_str(), "refactor-auth-2");
    assert_eq!(k3.as_str(), "refactor-auth-3");
}

#[tokio::test]
async fn create_empty_workspace_falls_back_when_name_is_unsluggable() {
    let config = ServerConfig::in_memory();
    let k = polling::create_empty_workspace(&config, "🚀✨");
    assert_eq!(
        k.as_str(),
        "workspace",
        "fallback slug is 'workspace' when name has no alnum chars"
    );
}

#[tokio::test]
async fn create_empty_workspace_broadcasts_upserted() {
    let config = ServerConfig::in_memory();
    let (mut client, server) = channel::pair();
    let serve_config = config.clone();
    tokio::spawn(async move {
        Server::new(serve_config).serve(server).await.unwrap();
    });
    client.send(Command::Subscribe).unwrap();
    let _snap = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .unwrap();

    polling::create_empty_workspace(&config, "side experiment");
    let evt = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("upsert event")
        .expect("event");
    match evt {
        Event::WorkspaceUpserted(w) => {
            assert_eq!(w.name, "side experiment");
        }
        other => panic!("expected WorkspaceUpserted, got {other:?}"),
    }
}

// ── Session layout persistence ───────────────────────────────────────

#[tokio::test]
async fn set_session_layout_persists_and_broadcasts() {
    use pilot_core::{SessionLayout, TileTree, WorkspaceSession, SessionKind};
    let config = ServerConfig::in_memory();

    // Seed a workspace with one session.
    let task = make_task("o/r#1");
    let ws_key = pilot_core::WorkspaceKey::new(pilot_core::workspace_key_for(&task));
    let mut ws = pilot_core::Workspace::from_task(task, Utc::now());
    let session = WorkspaceSession::new(
        ws_key.clone(),
        SessionKind::Shell,
        std::path::PathBuf::from("/tmp/pilot-test"),
        Utc::now(),
    );
    let session_id = session.id;
    ws.add_session(session);
    config
        .store
        .save_workspace(&pilot_store::WorkspaceRecord {
            key: ws_key.as_str().to_string(),
            created_at: ws.created_at,
            workspace_json: serde_json::to_string(&ws).ok(),
        })
        .unwrap();

    // New layout: HSplit with two leaves.
    let layout = SessionLayout::Splits {
        tree: TileTree::HSplit {
            left: Box::new(TileTree::Leaf { terminal_id: 1 }),
            right: Box::new(TileTree::Leaf { terminal_id: 2 }),
            ratio: 50,
        },
        focused: vec![0],
    };
    polling::set_session_layout(&config, &ws_key, session_id, layout.clone());

    // Reload + verify.
    let stored: pilot_core::Workspace = serde_json::from_str(
        &config
            .store
            .get_workspace(&ws_key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap();
    let stored_layout = &stored.sessions[0].layout;
    assert_eq!(stored_layout, &layout, "layout round-trips through the store");
}

#[tokio::test]
async fn set_session_layout_no_op_for_missing_session() {
    use pilot_core::SessionLayout;
    let config = ServerConfig::in_memory();
    let key = pilot_core::WorkspaceKey::new("github:none");
    // Should not panic when neither workspace nor session exist.
    polling::set_session_layout(
        &config,
        &key,
        pilot_core::SessionId::new(),
        SessionLayout::default(),
    );
}

// ── Bus → Server::serve integration ──────────────────────────────────

#[tokio::test]
async fn upserts_reach_subscribed_client_through_bus() {
    let config = ServerConfig::in_memory();
    let (mut client, server) = channel::pair();
    let serve_config = config.clone();
    tokio::spawn(async move {
        Server::new(serve_config).serve(server).await.unwrap();
    });
    client.send(Command::Subscribe).unwrap();
    // Drain the initial Snapshot.
    let _snap = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .unwrap();

    // Now produce an upsert. The bus should fan it out to the client.
    polling::upsert(&config, make_task("o/r#777")).await;

    let evt = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("client receives upsert")
        .expect("event");
    match evt {
        Event::WorkspaceUpserted(w) => {
            assert_eq!(w.pr.as_ref().unwrap().id.key, "o/r#777");
        }
        other => panic!("expected WorkspaceUpserted, got {other:?}"),
    }
}

// ── spawn() loop ─────────────────────────────────────────────────────

#[tokio::test]
async fn spawn_with_no_sources_exits_quickly_and_silently() {
    // Edge case: user has no GH token + no LINEAR_API_KEY. The daemon
    // should still boot, just with an idle polling task that doesn't
    // burn CPU spinning forever.
    let config = ServerConfig::in_memory();
    let handle = polling::spawn(config, vec![], Duration::from_millis(10));
    tokio::time::timeout(Duration::from_millis(500), handle)
        .await
        .expect("polling task exits when sources is empty")
        .expect("no panic");
}

// ── Per-provider filter ────────────────────────────────────────────

fn make_typed_task(key: &str, role: TaskRole, is_pr: bool) -> Task {
    let mut t = make_task(key);
    t.role = role;
    t.url = if is_pr {
        format!("https://github.com/o/r/pull/{key}")
    } else {
        format!("https://github.com/o/r/issues/{key}")
    };
    t
}

#[test]
fn github_filter_drops_disallowed_roles() {
    // User wants only PRs they authored. Per-type schema: pr.author
    // on, pr.reviewer/etc off → reviewer-role PRs dropped.
    let mut filter = ProviderConfig::default();
    filter.enabled_keys.insert("pr.author".into());
    filter.enabled_keys.insert("issue.author".into());

    let mine = make_typed_task("1", TaskRole::Author, true);
    let theirs = make_typed_task("2", TaskRole::Reviewer, true);

    let kept = polling::filter_github_tasks(
        vec![mine.clone(), theirs.clone()],
        &filter,
        &std::collections::BTreeSet::new(),
    );
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].id.key, mine.id.key);
}

#[test]
fn github_filter_drops_disallowed_types() {
    // Author of everything but only wants PRs — no issue.* keys at
    // all → issues filtered out entirely.
    let mut filter = ProviderConfig::default();
    filter.enabled_keys.insert("pr.author".into());
    // No issue.* keys — issues should be dropped.

    let pr = make_typed_task("1", TaskRole::Author, true);
    let issue = make_typed_task("2", TaskRole::Author, false);

    let kept = polling::filter_github_tasks(
        vec![pr.clone(), issue.clone()],
        &filter,
        &std::collections::BTreeSet::new(),
    );
    assert_eq!(kept.len(), 1, "issue dropped, PR kept");
    assert!(kept[0].url.contains("/pull/"));
}

#[test]
fn linear_filter_drops_disallowed_roles() {
    let mut filter = ProviderConfig::default();
    filter.enabled_keys.insert("role.assignee".into());

    let mut assignee = make_task("LIN-1");
    assignee.id.source = "linear".into();
    assignee.role = TaskRole::Assignee;
    let mut subscriber = make_task("LIN-2");
    subscriber.id.source = "linear".into();
    subscriber.role = TaskRole::Mentioned;

    let kept = polling::filter_linear_tasks(vec![assignee.clone(), subscriber.clone()], &filter);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].id.key, "LIN-1");
}

#[test]
fn empty_filter_drops_everything() {
    // Defensive: if the user somehow ends up with an empty filter,
    // the daemon shouldn't spam them with every task.
    let filter = ProviderConfig::default();
    let kept = polling::filter_github_tasks(
        vec![make_typed_task("1", TaskRole::Author, true)],
        &filter,
        &std::collections::BTreeSet::new(),
    );
    assert!(kept.is_empty());
}

// ── Scope filter ───────────────────────────────────────────────────

fn make_repo_task(repo: &str) -> Task {
    let mut t = make_task("1");
    t.role = TaskRole::Author;
    t.repo = Some(repo.into());
    t.url = format!("https://github.com/{repo}/pull/1");
    t
}

/// All PR + Issue role keys on. Equivalent to "subscribe to
/// everything the user is involved with."
fn fully_open_filter() -> ProviderConfig {
    let mut f = ProviderConfig::default();
    f.enabled_keys.insert("pr.author".into());
    f.enabled_keys.insert("pr.reviewer".into());
    f.enabled_keys.insert("pr.assignee".into());
    f.enabled_keys.insert("pr.mentioned".into());
    f.enabled_keys.insert("issue.author".into());
    f.enabled_keys.insert("issue.assignee".into());
    f.enabled_keys.insert("issue.mentioned".into());
    f
}

#[test]
fn empty_scope_set_lets_every_task_through() {
    // No picker run → empty selected_scopes → "all scopes". Default
    // for setups that haven't run the scope picker yet.
    let kept = polling::filter_github_tasks(
        vec![make_repo_task("acme/web"), make_repo_task("widgets/core")],
        &fully_open_filter(),
        &std::collections::BTreeSet::new(),
    );
    assert_eq!(kept.len(), 2);
}

#[test]
fn repo_scope_keeps_only_matching_repos() {
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("github:acme/web".to_string());
    let kept = polling::filter_github_tasks(
        vec![
            make_repo_task("acme/web"),
            make_repo_task("acme/api"),
            make_repo_task("widgets/core"),
        ],
        &fully_open_filter(),
        &scopes,
    );
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].repo.as_deref(), Some("acme/web"));
}

#[test]
fn org_scope_keeps_every_repo_under_that_org() {
    // Selecting an org scope is shorthand for "all of its repos".
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("github:acme".to_string());
    let kept = polling::filter_github_tasks(
        vec![
            make_repo_task("acme/web"),
            make_repo_task("acme/api"),
            make_repo_task("widgets/core"),
        ],
        &fully_open_filter(),
        &scopes,
    );
    let kept_repos: Vec<&str> = kept.iter().filter_map(|t| t.repo.as_deref()).collect();
    assert_eq!(kept_repos, vec!["acme/web", "acme/api"]);
}

#[test]
fn task_with_no_repo_drops_when_scopes_set() {
    // Defensive: scope-narrowing should not leak unknown-origin tasks.
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("github:acme/web".to_string());
    let mut t = make_repo_task("acme/web");
    t.repo = None;
    let kept = polling::filter_github_tasks(vec![t], &fully_open_filter(), &scopes);
    assert!(kept.is_empty());
}

// ── Search-qualifier builder ────────────────────────────────────────

#[test]
fn pr_qualifiers_default_to_involves_when_all_pr_roles_enabled() {
    // All four PR roles set → use the broadest involves: shortcut.
    let quals = polling::build_pr_search_qualifiers(
        &fully_open_filter(),
        &std::collections::BTreeSet::new(),
        "alice",
    );
    assert_eq!(quals, vec!["involves:alice"]);
}

#[test]
fn pr_qualifiers_emit_specific_role_when_subset_enabled() {
    // Just `pr.author` — narrow upstream so GitHub doesn't return PRs
    // matching other roles we'd drop post-fetch.
    let mut filter = ProviderConfig::default();
    filter.enabled_keys.insert("pr.author".into());
    let quals = polling::build_pr_search_qualifiers(
        &filter,
        &std::collections::BTreeSet::new(),
        "alice",
    );
    assert_eq!(quals, vec!["author:alice"]);
}

#[test]
fn pr_qualifiers_two_roles_emit_involves_not_paren_or() {
    // Regression: GitHub's qualifier search silently mishandles
    // `(author:X OR review-requested:X) repo:Y`, returning 0 even
    // when the rows exist. Confirmed against `gh search prs`. We
    // route through `involves:USER` instead and post-filter in
    // `filter_github_tasks`. See `polling::role_qualifier`.
    let mut filter = ProviderConfig::default();
    filter.enabled_keys.insert("pr.author".into());
    filter.enabled_keys.insert("pr.reviewer".into());
    let quals = polling::build_pr_search_qualifiers(
        &filter,
        &std::collections::BTreeSet::new(),
        "alice",
    );
    assert_eq!(
        quals,
        vec!["involves:alice"],
        "must NOT emit a paren-OR group — GitHub's parser drops rows"
    );
}

#[test]
fn issue_qualifiers_have_no_reviewer() {
    // Issues never have a reviewer — `pr.reviewer` is irrelevant for
    // the issue search.
    let mut filter = ProviderConfig::default();
    filter.enabled_keys.insert("issue.author".into());
    filter.enabled_keys.insert("pr.reviewer".into());
    let quals = polling::build_issue_search_qualifiers(
        &filter,
        &std::collections::BTreeSet::new(),
        "alice",
    );
    assert_eq!(quals, vec!["author:alice"]);
}

#[test]
fn issue_qualifiers_default_to_involves_when_all_issue_roles_enabled() {
    let quals = polling::build_issue_search_qualifiers(
        &fully_open_filter(),
        &std::collections::BTreeSet::new(),
        "alice",
    );
    assert_eq!(quals, vec!["involves:alice"]);
}

#[test]
fn pr_qualifiers_append_org_scope() {
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("github:acme".to_string());
    let quals = polling::build_pr_search_qualifiers(&fully_open_filter(), &scopes, "alice");
    assert_eq!(quals, vec!["involves:alice", "org:acme"]);
}

#[test]
fn pr_qualifiers_append_repo_scope() {
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("github:acme/web".to_string());
    let quals = polling::build_pr_search_qualifiers(&fully_open_filter(), &scopes, "alice");
    assert_eq!(quals, vec!["involves:alice", "repo:acme/web"]);
}

#[test]
fn pr_qualifiers_or_multiple_scopes_inside_parens() {
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("github:acme".to_string());
    scopes.insert("github:widgets/core".to_string());
    let quals = polling::build_pr_search_qualifiers(&fully_open_filter(), &scopes, "alice");
    assert_eq!(
        quals,
        vec!["involves:alice", "(org:acme OR repo:widgets/core)"]
    );
}

#[test]
fn pr_qualifiers_drop_unknown_provider_prefix() {
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("linear:bogus".to_string());
    let quals = polling::build_pr_search_qualifiers(&fully_open_filter(), &scopes, "alice");
    assert_eq!(quals, vec!["involves:alice"]);
}

#[tokio::test]
async fn spawn_drives_sources_on_interval() {
    let config = ServerConfig::in_memory();
    let counter = Arc::new(AtomicUsize::new(0));
    let source: Box<dyn TaskSource> = Box::new(CountingSource {
        name: "test".into(),
        counter: counter.clone(),
    });
    let handle = polling::spawn(config, vec![source], Duration::from_millis(40));

    // Wait long enough for several ticks; the first tick fires
    // immediately, subsequent ticks every 40ms.
    tokio::time::sleep(Duration::from_millis(150)).await;
    handle.abort();
    let n = counter.load(Ordering::SeqCst);
    assert!(n >= 2, "polled at least twice (got {n})");
}
