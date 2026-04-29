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
use pilot_v2_ipc::{Command, Event, channel};
use pilot_v2_server::polling::{self, TaskSource};
use pilot_v2_server::{Server, ServerConfig};
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

// ── Test fixtures ──────────────────────────────────────────────────

struct FakeSource {
    name: String,
    tasks: Vec<Task>,
}

impl TaskSource for FakeSource {
    fn name(&self) -> &str {
        &self.name
    }
    fn fetch<'a>(&'a self) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<Task>>> + Send + 'a>> {
        let tasks = self.tasks.clone();
        Box::pin(async move { Ok(tasks) })
    }
}

struct FailingSource(String);

impl TaskSource for FailingSource {
    fn name(&self) -> &str {
        &self.0
    }
    fn fetch<'a>(&'a self) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<Task>>> + Send + 'a>> {
        Box::pin(async move { Err(anyhow::anyhow!("rate limited")) })
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
    fn fetch<'a>(&'a self) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<Task>>> + Send + 'a>> {
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
            // the originating task key. This is the v2 wire contract:
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
    polling::upsert(&config, make_task("o/r#42"));

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
    // The user marked the workspace read; v2's poller mustn't wipe
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
    polling::upsert(&config, task);

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
    polling::upsert(&config, mk());
    polling::upsert(&config, mk());
    polling::upsert(&config, mk());

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
        Event::ProviderError { source, message } => {
            assert_eq!(source, "github");
            assert!(message.contains("rate limited"));
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
    polling::upsert(&config, make_task("o/r#777"));

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
    // User wants only their own PRs (Author).
    let mut filter = ProviderConfig::default();
    filter.enabled_keys.insert("role.author".into());
    filter.enabled_keys.insert("type.prs".into());
    filter.enabled_keys.insert("type.issues".into());

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
    // Author of everything but only wants PRs surfaced.
    let mut filter = ProviderConfig::default();
    filter.enabled_keys.insert("role.author".into());
    filter.enabled_keys.insert("type.prs".into());
    // type.issues intentionally absent.

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

fn fully_open_filter() -> ProviderConfig {
    let mut f = ProviderConfig::default();
    f.enabled_keys.insert("role.author".into());
    f.enabled_keys.insert("role.reviewer".into());
    f.enabled_keys.insert("role.assignee".into());
    f.enabled_keys.insert("role.mentioned".into());
    f.enabled_keys.insert("type.prs".into());
    f.enabled_keys.insert("type.issues".into());
    f
}

#[test]
fn empty_scope_set_lets_every_task_through() {
    // No picker run → empty selected_scopes → "all scopes". This is
    // the upgrade-from-v1 default, so existing setups keep working.
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
fn qualifiers_default_to_involves_when_all_roles_enabled() {
    // The historical default: all 4 roles + no scope picker run.
    // Fastest, broadest query.
    let quals = polling::build_gh_search_qualifiers(
        &fully_open_filter(),
        &std::collections::BTreeSet::new(),
        "alice",
    );
    assert_eq!(quals, vec!["involves:alice"]);
}

#[test]
fn qualifiers_emit_specific_role_when_subset_enabled() {
    // Just "author" enabled — search the role directly so GH
    // doesn't return reviewer / mention rows we'd just drop.
    let mut filter = ProviderConfig::default();
    filter.enabled_keys.insert("role.author".into());
    filter.enabled_keys.insert("type.prs".into());
    let quals =
        polling::build_gh_search_qualifiers(&filter, &std::collections::BTreeSet::new(), "alice");
    assert_eq!(quals, vec!["author:alice"]);
}

#[test]
fn qualifiers_or_two_roles_inside_parens() {
    let mut filter = ProviderConfig::default();
    filter.enabled_keys.insert("role.author".into());
    filter.enabled_keys.insert("role.reviewer".into());
    filter.enabled_keys.insert("type.prs".into());
    let quals =
        polling::build_gh_search_qualifiers(&filter, &std::collections::BTreeSet::new(), "alice");
    assert_eq!(quals, vec!["(author:alice OR review-requested:alice)"]);
}

#[test]
fn qualifiers_append_org_scope() {
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("github:acme".to_string());
    let quals = polling::build_gh_search_qualifiers(&fully_open_filter(), &scopes, "alice");
    assert_eq!(quals, vec!["involves:alice", "org:acme"]);
}

#[test]
fn qualifiers_append_repo_scope() {
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("github:acme/web".to_string());
    let quals = polling::build_gh_search_qualifiers(&fully_open_filter(), &scopes, "alice");
    assert_eq!(quals, vec!["involves:alice", "repo:acme/web"]);
}

#[test]
fn qualifiers_or_multiple_scopes_inside_parens() {
    // Mixed selection (1 org + 1 repo); both end up in one OR group
    // so GitHub treats them as union.
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("github:acme".to_string());
    scopes.insert("github:widgets/core".to_string());
    let quals = polling::build_gh_search_qualifiers(&fully_open_filter(), &scopes, "alice");
    // BTreeSet is sorted: "github:acme" < "github:widgets/core".
    assert_eq!(
        quals,
        vec!["involves:alice", "(org:acme OR repo:widgets/core)"]
    );
}

#[test]
fn qualifiers_drop_unknown_provider_prefix() {
    // Defensive: a stale `linear:project` row in selected_scopes
    // shouldn't poison the GH search.
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("linear:bogus".to_string());
    let quals = polling::build_gh_search_qualifiers(&fully_open_filter(), &scopes, "alice");
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
