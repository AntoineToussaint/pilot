//! Migration shim: on first v2 launch, sessions in v1's
//! `~/.pilot/state.db` are copied **once** into v2's separate store
//! (`~/.pilot/v2/state.db`). v1's file is left untouched so the user
//! can run both versions side-by-side. These tests drive the import
//! function directly + the post-import Subscribe contract via a
//! pre-populated v2 store.

use chrono::Utc;
use pilot_core::{
    Activity, ActivityKind, CiStatus, ReviewStatus, Session, Task, TaskId, TaskRole, TaskState,
};
use pilot_store::{SessionRecord, SqliteStore, Store};
use pilot_v2_ipc::{Command, Event, channel};
use pilot_v2_server::{Server, ServerConfig};
use std::sync::Arc;

fn make_task(key: &str) -> Task {
    // Strip the trailing `#NUM` so the URL still parses as
    // `owner/repo/pull/NUM` (which is what `Workspace::classify`
    // checks via `url.contains("/pull/")` to put this in the PR slot).
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
        repo: Some("owner/repo".into()),
        branch: Some("feature/x".into()),
        base_branch: Some("main".into()),
        updated_at: Utc::now(),
        labels: vec![],
        reviewers: vec!["alice".into()],
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

fn seed_session(store: &dyn Store, key: &str, seen: usize) -> Session {
    let task = make_task(key);
    let mut session = Session::new_at(task, Utc::now());
    session.push_activity(Activity {
        author: "alice".into(),
        body: "looks good".into(),
        created_at: Utc::now(),
        kind: ActivityKind::Comment,
        node_id: None,
        path: None,
        line: None,
        diff_hunk: None,
        thread_id: None,
    });
    session.seen_count = seen;
    let record = SessionRecord {
        task_id: session.task_id.to_string(),
        seen_count: seen as i64,
        last_viewed_at: session.last_viewed_at,
        created_at: session.created_at,
        session_json: Some(serde_json::to_string(&session).unwrap()),
        metadata: None,
    };
    store.save_session(&record).unwrap();
    session
}

async fn subscribe_once(config: ServerConfig) -> Event {
    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        Server::new(config).serve(server).await.unwrap();
    });
    client.send(Command::Subscribe).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
        .await
        .expect("daemon replies")
        .expect("event")
}

/// Run the v1 → v2 migration explicitly: take a v1-shaped store
/// (Session rows), copy into a fresh v2 store as Workspaces, and
/// hand the v2 store to ServerConfig so the daemon's snapshot path
/// surfaces them. Mirrors what `ServerConfig::from_user_config` does
/// in production on first launch.
fn migrate(v1: SqliteStore) -> Arc<SqliteStore> {
    use pilot_v2_server::import_v1_into;
    let v2 = SqliteStore::in_memory().unwrap();
    import_v1_into(&v2, &v1);
    Arc::new(v2)
}

#[tokio::test]
async fn snapshot_replays_v1_sessions() {
    let v1 = SqliteStore::in_memory().unwrap();
    let seeded = seed_session(&v1, "owner/repo#42", 3);

    let v2 = migrate(v1);
    let evt = subscribe_once(ServerConfig::with_store(v2)).await;
    match evt {
        Event::Snapshot { workspaces, .. } => {
            // v1 → v2 import projects each Session into a Workspace.
            // The seeded session's primary task is a PR, so it lands
            // in the workspace's `pr` slot.
            assert_eq!(workspaces.len(), 1);
            let w = &workspaces[0];
            assert_eq!(w.pr.as_ref().unwrap().id, seeded.task_id);
            assert_eq!(w.seen_count, 3);
            assert_eq!(w.activity.len(), 1);
            assert_eq!(w.activity[0].author, "alice");
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}

#[tokio::test]
async fn snapshot_preserves_read_state_across_migration() {
    // The critical user-visible contract: after upgrading from v1 to
    // v2, workspaces marked read in v1 remain read in v2.
    let v1 = SqliteStore::in_memory().unwrap();
    let _read = seed_session(&v1, "owner/repo#1", 5);
    let _unread = seed_session(&v1, "owner/repo#2", 0);

    let v2 = migrate(v1);
    let evt = subscribe_once(ServerConfig::with_store(v2)).await;
    let workspaces = match evt {
        Event::Snapshot { workspaces, .. } => workspaces,
        other => panic!("expected Snapshot, got {other:?}"),
    };
    let mut by_key: std::collections::HashMap<_, _> = workspaces
        .into_iter()
        .map(|w| (w.pr.as_ref().unwrap().id.key.clone(), w))
        .collect();
    assert_eq!(by_key.remove("owner/repo#1").unwrap().seen_count, 5);
    assert_eq!(by_key.remove("owner/repo#2").unwrap().seen_count, 0);
}

#[tokio::test]
async fn snapshot_skips_malformed_session_rows() {
    // An older v1 row with no JSON blob, and a row with corrupt JSON,
    // must both be dropped silently rather than crashing the daemon.
    // One good session alongside them still surfaces.
    let v1 = SqliteStore::in_memory().unwrap();
    let good = seed_session(&v1, "owner/repo#good", 1);

    v1.save_session(&SessionRecord {
        task_id: "github:owner/repo#empty".into(),
        seen_count: 0,
        last_viewed_at: None,
        created_at: Utc::now(),
        session_json: None,
        metadata: None,
    })
    .unwrap();
    v1.save_session(&SessionRecord {
        task_id: "github:owner/repo#bad".into(),
        seen_count: 0,
        last_viewed_at: None,
        created_at: Utc::now(),
        session_json: Some("{not valid json".into()),
        metadata: None,
    })
    .unwrap();

    let v2 = migrate(v1);
    let evt = subscribe_once(ServerConfig::with_store(v2)).await;
    match evt {
        Event::Snapshot { workspaces, .. } => {
            assert_eq!(workspaces.len(), 1, "only the good workspace survives");
            assert_eq!(workspaces[0].pr.as_ref().unwrap().id, good.task_id);
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}

#[tokio::test]
async fn snapshot_is_empty_when_store_is_empty() {
    // New installs with a fresh DB just return an empty snapshot — no
    // errors, no ghost sessions. This is the "nothing to migrate" path.
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let evt = subscribe_once(ServerConfig::with_store(store)).await;
    match evt {
        Event::Snapshot {
            workspaces,
            terminals,
        } => {
            assert!(workspaces.is_empty());
            assert!(terminals.is_empty());
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}

// ── import_v1_into ────────────────────────────────────────────────────

#[test]
fn import_copies_every_v1_session_into_v2() {
    use pilot_v2_server::import_v1_into;
    let v1 = SqliteStore::in_memory().unwrap();
    seed_session(&v1, "owner/repo#1", 3);
    seed_session(&v1, "owner/repo#2", 0);

    let v2 = SqliteStore::in_memory().unwrap();
    let n = import_v1_into(&v2, &v1);
    assert_eq!(n, 2);
    // Each v1 Session projects into one v2 Workspace via
    // `Workspace::from_v1_session`.
    assert_eq!(v2.list_workspaces().unwrap().len(), 2);
}

#[test]
fn import_does_not_mutate_v1_store() {
    use pilot_v2_server::import_v1_into;
    let v1 = SqliteStore::in_memory().unwrap();
    seed_session(&v1, "owner/repo#1", 7);
    let before = v1.list_sessions().unwrap();

    let v2 = SqliteStore::in_memory().unwrap();
    import_v1_into(&v2, &v1);

    let after = v1.list_sessions().unwrap();
    assert_eq!(before.len(), after.len(), "v1 row count unchanged");
    assert_eq!(
        after[0].seen_count, 7,
        "v1 read state preserved after import"
    );
}

#[test]
fn import_returns_zero_for_empty_v1() {
    use pilot_v2_server::import_v1_into;
    let v1 = SqliteStore::in_memory().unwrap();
    let v2 = SqliteStore::in_memory().unwrap();
    let n = import_v1_into(&v2, &v1);
    assert_eq!(n, 0);
    assert!(v2.list_sessions().unwrap().is_empty());
}

#[test]
fn state_db_path_is_scoped_under_v2_subdir() {
    use pilot_v2_server::{legacy_v1_state_db_path, state_db_path};
    let v2 = state_db_path();
    let v1 = legacy_v1_state_db_path();
    assert_ne!(v2, v1, "v2 must use a different file from v1");
    assert!(
        v2.to_string_lossy().contains("v2"),
        "v2 path should be under a v2 subdir, got {}",
        v2.display()
    );
    // v1's path is the parent's `state.db`, not under v2.
    assert!(
        v1.parent() != v2.parent(),
        "v1 and v2 live in different directories"
    );
}
