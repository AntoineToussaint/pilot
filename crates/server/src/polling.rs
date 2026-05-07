//! Provider polling. The daemon owns ONE polling task per process; it
//! drives the configured `TaskSource`s on an interval, upserts each
//! returned task into the store, and broadcasts `SessionUpserted`
//! events through the `ServerConfig::bus` so every connected client
//! sees the change.
//!
//! ## Why a `TaskSource` trait, not direct GhClient/LinearClient calls
//!
//! The polling loop's logic — interval, upsert, broadcast, error
//! reporting — is identical for every provider. Hard-coding GitHub and
//! Linear into the loop would make it hard to test (real HTTP calls
//! against real APIs) and hard to extend (adding Jira would touch the
//! loop). With `TaskSource`, the loop is provider-agnostic and tests
//! drop in a fixture source that returns whatever vector of tasks the
//! test wants.
//!
//! ## Read-state preservation on update
//!
//! When a task we've seen before comes back from a provider, we merge
//! its fresh fields onto the existing `Session` rather than replacing
//! it — so `seen_count`, `read_indices`, `snoozed_until`, and
//! `last_viewed_at` all survive the poll. We do it inline here since
//! there's only one place sessions enter the system.

use crate::ServerConfig;
use chrono::Utc;
use pilot_auth::{CommandProvider, CredentialChain, EnvProvider};
use pilot_core::{ProviderConfig, Task, Workspace, WorkspaceKey};
use pilot_gh::GhClient;
use pilot_linear::LinearClient;
use pilot_store::WorkspaceRecord;
use pilot_ipc::Event;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// Anything that can produce a flat list of `Task`s. Implementations
/// should be cheap to construct and cheap to call repeatedly: they're
/// invoked on every poll tick.
///
/// Errors are typed (`pilot_core::ProviderError`) so polling can
/// distinguish retryable hiccups from auth failures from permanent
/// bugs and react accordingly. See `pilot_core::provider`.
pub trait TaskSource: Send + Sync + 'static {
    /// Short stable name for telemetry / `Event::ProviderError`
    /// (e.g. "github", "linear").
    fn name(&self) -> &str;

    /// Fetch the current set of tasks. Returns a classified error so
    /// the polling loop can pick the right log level + decide whether
    /// to retry.
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, pilot_core::ProviderError>> + Send + 'a>>;
}

/// `GhClient` adapter. The filter narrows the upstream result by
/// role and item type before they reach the daemon's upsert path —
/// disabled roles / types never become Workspaces. `scopes` further
/// narrows by repo / org: when non-empty, only tasks whose
/// `task.repo` matches a selected scope id pass through.
pub struct GhSource {
    pub client: GhClient,
    pub filter: ProviderConfig,
    pub scopes: std::collections::BTreeSet<String>,
    /// Bus handle so the source can emit `PollProgress` events
    /// during its fetch. The polling layer doesn't pass `&ServerConfig`
    /// to `TaskSource::fetch` (would couple them), so each source
    /// keeps a clone of just the broadcast sender.
    pub bus: tokio::sync::broadcast::Sender<Event>,
}

impl GhSource {
    fn emit_progress(&self, message: impl Into<String>) {
        let message = message.into();
        tracing::info!(source = "github", %message, "poll progress");
        let _ = self.bus.send(Event::PollProgress {
            source: "github".into(),
            message,
        });
    }
}

impl TaskSource for GhSource {
    fn name(&self) -> &str {
        "github"
    }
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, pilot_core::ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            let want_prs = self.filter.pr_enabled();
            let want_issues = self.filter.issue_enabled();

            // Surface what we're about to do so the polling modal
            // can show "Querying PRs from github…" instead of just
            // a bare spinner. Each step also lands in /tmp/pilot.log
            // for free, easing debugging.
            let plan = match (want_prs, want_issues) {
                (true, true) => "PRs + Issues",
                (true, false) => "PRs",
                (false, true) => "Issues",
                (false, false) => {
                    self.emit_progress("nothing to fetch (no PR or Issue keys enabled)");
                    return Ok(Vec::new());
                }
            };
            self.emit_progress(format!("Querying GitHub for {plan}…"));
            // Emit the EXACT rendered query strings so the user can
            // see what's being asked. This is the single most useful
            // data point when debugging "filter returned 0 results" —
            // they can paste the query into github.com/search and
            // verify what GitHub itself thinks is in scope.
            if want_prs {
                self.emit_progress(format!("PR query: {}", self.client.pr_search_query()));
            }
            if want_issues {
                self.emit_progress(format!(
                    "Issue query: {}",
                    self.client.issue_search_query()
                ));
            }

            let raw = self
                .client
                .fetch_selected(want_prs, want_issues)
                .await
                .map_err(pilot_core::ProviderError::from)?;

            self.emit_progress(format!("Got {} raw items, applying filters…", raw.len()));
            let kept = filter_github_tasks(raw, &self.filter, &self.scopes);
            self.emit_progress(format!("{} tasks kept after filter", kept.len()));

            // Log a per-rate-budget summary too. Cheap, super useful
            // when debugging "why is polling slow / failing".
            let snap = self.client.rate_snapshot();
            if let Some(remote) = snap.remote {
                tracing::info!(
                    source = "github",
                    remote_remaining = remote.remaining,
                    remote_limit = remote.limit,
                    local_available = snap.local_available,
                    local_capacity = snap.local_capacity,
                    "rate budget snapshot"
                );
            }
            Ok(kept)
        })
    }
}

/// `LinearClient` adapter.
pub struct LinearSource {
    pub client: LinearClient,
    pub filter: ProviderConfig,
    pub bus: tokio::sync::broadcast::Sender<Event>,
}

impl LinearSource {
    fn emit_progress(&self, message: impl Into<String>) {
        let message = message.into();
        tracing::info!(source = "linear", %message, "poll progress");
        let _ = self.bus.send(Event::PollProgress {
            source: "linear".into(),
            message,
        });
    }
}

impl TaskSource for LinearSource {
    fn name(&self) -> &str {
        "linear"
    }
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, pilot_core::ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            self.emit_progress("Querying Linear for issues…");
            let raw = self
                .client
                .fetch_all()
                .await
                .map_err(pilot_core::ProviderError::from)?;
            self.emit_progress(format!("Got {} issues, applying filters…", raw.len()));
            let kept = filter_linear_tasks(raw, &self.filter);
            self.emit_progress(format!("{} issues kept after filter", kept.len()));
            Ok(kept)
        })
    }
}

/// Build the GraphQL search qualifiers a `GhClient` should use,
/// derived from the user's persisted role + scope selection. The
/// result is appended to `default_search_qualifiers` (`is:open
/// is:pr archived:false`) before being sent to GitHub.
///
/// Mapping:
///
/// - **Role** — `involves:USER` when all four roles (or none) are
///   enabled. With a strict subset, emit explicit role qualifiers
///   (`author:USER`, `review-requested:USER`, `assignee:USER`,
///   `mentions:USER`) joined with `OR` inside parens.
/// - **Scope** — each `github:owner` becomes `org:owner`; each
///   `github:owner/repo` becomes `repo:owner/repo`. Multiple scope
///   qualifiers are OR'd inside parens so the user gets the union.
///
/// Empty role + empty scope → the same `involves:USER` baseline as
/// before, so legacy setups (no picker visited) keep working.
/// Qualifiers for the **PR** search. Reads `pr.*` keys.
pub fn build_pr_search_qualifiers(
    filter: &ProviderConfig,
    scopes: &std::collections::BTreeSet<String>,
    username: &str,
) -> Vec<String> {
    let mut quals = Vec::new();
    let pr_roles = [
        ("pr.author", "author"),
        ("pr.reviewer", "review-requested"),
        ("pr.assignee", "assignee"),
        ("pr.mentioned", "mentions"),
    ];
    quals.push(role_qualifier(filter, username, &pr_roles));
    if let Some(s) = scope_qualifier(scopes) {
        quals.push(s);
    }
    quals
}

/// Qualifiers for the **Issue** search. Reads `issue.*` keys (no
/// reviewer concept — issues don't have reviewers in GitHub).
pub fn build_issue_search_qualifiers(
    filter: &ProviderConfig,
    scopes: &std::collections::BTreeSet<String>,
    username: &str,
) -> Vec<String> {
    let mut quals = Vec::new();
    let issue_roles = [
        ("issue.author", "author"),
        ("issue.assignee", "assignee"),
        ("issue.mentioned", "mentions"),
    ];
    quals.push(role_qualifier(filter, username, &issue_roles));
    if let Some(s) = scope_qualifier(scopes) {
        quals.push(s);
    }
    quals
}

/// Build a single role qualifier for the GitHub search API.
///
/// Why not OR-with-parens — GitHub's qualifier-style search parser
/// silently mishandles parens-grouped ORs combined with other
/// qualifiers (`(author:X OR review-requested:X) repo:Y`): the API
/// returns 0 even when the unrouped equivalent
/// (`author:X repo:Y`) returns rows. Confirmed against `gh search
/// prs` 2026-05-01 — same token, same query, the paren-form returns
/// `[]` while the no-paren form returns the user's PRs.
///
/// So we use the search syntax that's known to work:
///
/// - **0 roles enabled** → `involves:USER`. The user will see no rows
///   because `filter_github_tasks` drops everything; we still want
///   the request to be valid.
/// - **1 role enabled** → emit that single qualifier directly
///   (`author:USER`). No OR, no parens, just works.
/// - **2+ roles enabled** → emit `involves:USER` (covers author,
///   reviewer, assignee, mentioned) and let `filter_github_tasks`
///   drop the disabled roles post-fetch. Slightly more bytes over
///   the wire, but reliable.
///
/// Net effect: the wire query never contains a parens group, so the
/// "0 results from a valid query" footgun is gone.
fn role_qualifier(
    filter: &ProviderConfig,
    username: &str,
    keys: &[(&str, &str)],
) -> String {
    let enabled: Vec<&str> = keys
        .iter()
        .filter(|(k, _)| filter.has(k))
        .map(|(_, op)| *op)
        .collect();
    match enabled.len() {
        0 => format!("involves:{username}"),
        1 => format!("{}:{username}", enabled[0]),
        _ => format!("involves:{username}"),
    }
}

/// `(org:foo OR repo:bar/baz)` for a non-empty scope set; `None`
/// when the user hasn't narrowed (= subscribe to all).
fn scope_qualifier(scopes: &std::collections::BTreeSet<String>) -> Option<String> {
    if scopes.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = scopes
        .iter()
        .filter_map(|s| {
            let stripped = s.strip_prefix("github:")?;
            if stripped.contains('/') {
                Some(format!("repo:{stripped}"))
            } else {
                Some(format!("org:{stripped}"))
            }
        })
        .collect();
    if parts.is_empty() {
        return None;
    }
    if parts.len() == 1 {
        return Some(parts.pop().unwrap());
    }
    Some(format!("({})", parts.join(" OR ")))
}

/// Drop GitHub tasks that don't match the user's enabled roles +
/// item types + scope selection. `scopes` is the
/// `selected_scopes["github"]` set (possibly empty); tasks pass the
/// scope gate when:
///
/// - `scopes` is empty (user didn't pick anything → see all), OR
/// - the task's repo matches a selected repo scope, OR
/// - the task's repo lives under a selected org scope (parent match).
///
/// Tasks without a usable `role` field default to passing the role
/// check (we trust the upstream classification).
pub fn filter_github_tasks(
    tasks: Vec<Task>,
    filter: &ProviderConfig,
    scopes: &std::collections::BTreeSet<String>,
) -> Vec<Task> {
    tasks
        .into_iter()
        .filter(|t| {
            // Combined type+role gate. Issues use `issue.*` keys, PRs
            // use `pr.*` keys. Tasks of unknown type (discussions,
            // etc.) bypass the type/role filter — they don't have a
            // toggle.
            let type_role_ok = if t.url.contains("/pull/") {
                filter.pr_enabled() && filter.allows_pr_role(t.role)
            } else if t.url.contains("/issues/") {
                filter.issue_enabled() && filter.allows_issue_role(t.role)
            } else {
                true
            };
            if !type_role_ok {
                return false;
            }
            // Scope gate. Empty `scopes` = "all" (the no-picker
            // default). Otherwise repo:owner/name must match a
            // selected repo scope, OR its owner must match a
            // selected org scope.
            if scopes.is_empty() {
                return true;
            }
            let Some(repo) = t.repo.as_deref() else {
                return false;
            };
            let repo_scope = format!("github:{repo}");
            if scopes.contains(&repo_scope) {
                return true;
            }
            if let Some((owner, _)) = repo.split_once('/') {
                return scopes.contains(&format!("github:{owner}"));
            }
            false
        })
        .collect()
}

/// Drop Linear tasks whose role isn't enabled. Linear has no
/// PRs-vs-Issues distinction — flat `role.*` keys.
pub fn filter_linear_tasks(tasks: Vec<Task>, filter: &ProviderConfig) -> Vec<Task> {
    tasks
        .into_iter()
        .filter(|t| filter.allows_linear_role(t.role))
        .collect()
}

/// Best-effort: build the source set from the user's persisted
/// setup. Each constructed source carries the per-provider filter
/// (role + item-type toggles) and applies it post-fetch. Providers
/// whose id isn't in `enabled_providers` are skipped entirely.
///
/// The `bus` is the daemon's broadcast sender; sources clone it so
/// they can emit `PollProgress` events during their fetch (drives
/// the polling-modal status line).
pub async fn sources_for(
    setup: &pilot_core::PersistedSetup,
    bus: tokio::sync::broadcast::Sender<Event>,
) -> Vec<Box<dyn TaskSource>> {
    let mut sources: Vec<Box<dyn TaskSource>> = Vec::new();

    if setup.enabled_providers.contains("github") {
        let chain = CredentialChain::new()
            .with(EnvProvider::new("GH_TOKEN"))
            .with(EnvProvider::new("GITHUB_TOKEN"))
            .with(CommandProvider::new("gh", &["auth", "token"]));
        match chain.resolve("github").await {
            Ok(cred) => match GhClient::from_credential(cred).await {
                Ok(client) => {
                    let filter = setup.provider_config("github");
                    let scopes = setup
                        .selected_scopes
                        .get("github")
                        .cloned()
                        .unwrap_or_default();
                    let pr_qualifiers =
                        build_pr_search_qualifiers(&filter, &scopes, client.username());
                    let issue_qualifiers =
                        build_issue_search_qualifiers(&filter, &scopes, client.username());
                    let client = client.with_filters(pr_qualifiers, issue_qualifiers);
                    sources.push(Box::new(GhSource {
                        client,
                        filter,
                        scopes,
                        bus: bus.clone(),
                    }));
                }
                Err(e) => tracing::warn!("github client init failed: {e}"),
            },
            Err(e) => tracing::info!("github credentials not available: {e}"),
        }
    }

    if setup.enabled_providers.contains("linear") {
        match LinearClient::from_env() {
            Ok(client) => sources.push(Box::new(LinearSource {
                client,
                filter: setup.provider_config("linear"),
                bus: bus.clone(),
            })),
            Err(e) => tracing::info!("linear not configured: {e}"),
        }
    }

    sources
}

/// Convenience: build the default source set assuming both providers
/// are enabled with their default filters. Used by binaries that
/// bypass the setup screen (e.g. headless `pilot daemon start` in
/// CI). When a saved `PersistedSetup` exists in the store, prefer
/// that instead.
pub async fn default_sources(
    bus: tokio::sync::broadcast::Sender<Event>,
) -> Vec<Box<dyn TaskSource>> {
    let setup = pilot_core::PersistedSetup {
        enabled_providers: ["github".to_string(), "linear".to_string()]
            .into_iter()
            .collect(),
        enabled_agents: Default::default(),
        provider_filters: [
            ("github".into(), ProviderConfig::default_for("github")),
            ("linear".into(), ProviderConfig::default_for("linear")),
        ]
        .into_iter()
        .collect(),
        // Empty selected_scopes = "all scopes" (legacy behavior).
        selected_scopes: Default::default(),
    };
    sources_for(&setup, bus).await
}

/// Run one poll tick: every source is called once and its tasks are
/// upserted. Errors from one source don't stop the others.
pub async fn tick(config: &ServerConfig, sources: &[Box<dyn TaskSource>]) {
    for source in sources {
        match source.fetch().await {
            Ok(tasks) => {
                let count = tasks.len();
                tracing::info!(source = source.name(), count, "poll succeeded");
                for task in tasks {
                    upsert(config, task).await;
                }
                // Always emit `PollCompleted`, even on 0 tasks, so
                // the TUI can distinguish "polling hasn't run yet"
                // from "polling found nothing matching your filter".
                let _ = config.bus.send(Event::PollCompleted {
                    source: source.name().to_string(),
                    count,
                });
            }
            Err(e) => {
                // Log the full diagnostic always (file is private,
                // dev tooling can tail it). The TUI status bar gets
                // the terse `user_message` so it stays one row.
                if e.is_retryable() {
                    tracing::warn!(diagnostic = %e.diagnostic(), "poll failed (retryable)");
                } else if e.is_auth() {
                    tracing::error!(diagnostic = %e.diagnostic(), "poll failed (auth)");
                } else {
                    tracing::error!(diagnostic = %e.diagnostic(), "poll failed (permanent)");
                }
                let kind = if e.is_retryable() {
                    "retryable"
                } else if e.is_auth() {
                    "auth"
                } else {
                    "permanent"
                };
                let _ = config.bus.send(Event::ProviderError {
                    source: e.source().to_string(),
                    message: e.user_message(),
                    detail: e.diagnostic(),
                    kind: kind.to_string(),
                });
            }
        }
    }
}

/// Spawn the long-lived polling loop. Returns the join handle so the
/// caller can `abort()` on shutdown if it wants — `pilot daemon stop`
/// drops the whole process so we don't bother in main.
pub fn spawn(
    config: ServerConfig,
    sources: Vec<Box<dyn TaskSource>>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if sources.is_empty() {
            tracing::warn!("no provider sources configured — polling task is idle");
            return;
        }
        let mut ticker = tokio::time::interval(interval);
        // First tick fires immediately; subsequent ticks honor `interval`.
        ticker.tick().await;
        tick(&config, &sources).await;
        loop {
            ticker.tick().await;
            tick(&config, &sources).await;
        }
    })
}

/// Merge `task` into the existing workspace for its workspace key
/// (PR + matching issues collapse to one row), persist it, and
/// broadcast `WorkspaceUpserted`. The store backs this with
/// `workspace:<key>` rows in the kv table — no schema migration
/// needed.
///
/// Read state (`seen_count`, `read_indices`, `snoozed_until`,
/// `last_viewed_at`) is preserved across updates — providers only
/// own upstream-derived fields.
pub async fn upsert(config: &ServerConfig, task: Task) {
    let key_str = pilot_core::workspace_key_for(&task);
    let key = WorkspaceKey::new(key_str.clone());

    let existing = config
        .store
        .get_workspace(&key)
        .ok()
        .flatten()
        .and_then(|r| r.workspace_json)
        .and_then(|j| serde_json::from_str::<Workspace>(&j).ok());

    let mut workspace = match existing {
        Some(mut w) => {
            w.attach_task(task);
            w
        }
        None => Workspace::from_task(task, Utc::now()),
    };

    // PR-attach migration runs **before** persistence + broadcast so
    // observers never see a stale `worktree_path`. Most polls are
    // no-ops here (current slug already matches the persisted path).
    // The migration is a tokio-async git operation so we await it.
    crate::spawn_handler::migrate_session_paths_if_needed(config, &mut workspace).await;

    let json = serde_json::to_string(&workspace).ok();
    let record = WorkspaceRecord {
        key: key.as_str().to_string(),
        created_at: workspace.created_at,
        workspace_json: json,
    };
    if let Err(e) = config.store.save_workspace(&record) {
        tracing::warn!("save_workspace failed: {e}");
    }
    let _ = config
        .bus
        .send(Event::WorkspaceUpserted(Box::new(workspace)));
}

/// Create an empty workspace (no PR, no issues) named by the user.
/// Generates a `WorkspaceKey` from the name's slug, disambiguating
/// with a numeric suffix if a workspace with that key already
/// exists. Persists + broadcasts `WorkspaceUpserted`.
///
/// Returns the new key so the caller (sidebar, tests) can land the
/// cursor on the freshly-created row.
pub fn create_empty_workspace(config: &ServerConfig, name: &str) -> WorkspaceKey {
    let base = pilot_core::slug::slugify(name);
    let base = if base.is_empty() {
        "workspace".to_string()
    } else {
        base
    };
    // Collision: try `<base>`, `<base>-2`, `<base>-3`, ... until the
    // store reports no existing record.
    let key = (1..)
        .map(|i| {
            if i == 1 {
                WorkspaceKey::new(base.clone())
            } else {
                WorkspaceKey::new(format!("{base}-{i}"))
            }
        })
        .find(|k| {
            config
                .store
                .get_workspace(k)
                .ok()
                .flatten()
                .and_then(|r| r.workspace_json)
                .is_none()
        })
        .expect("infinite range yields a free key");

    let mut workspace = Workspace::empty(key.clone(), "main", Utc::now());
    if !name.trim().is_empty() {
        workspace.name = name.trim().to_string();
    }

    let record = WorkspaceRecord {
        key: key.as_str().to_string(),
        created_at: workspace.created_at,
        workspace_json: serde_json::to_string(&workspace).ok(),
    };
    if let Err(e) = config.store.save_workspace(&record) {
        tracing::warn!("save_workspace failed: {e}");
    }
    let _ = config
        .bus
        .send(Event::WorkspaceUpserted(Box::new(workspace)));
    key
}

/// Set or clear the workspace's `snoozed_until` timestamp. `None`
/// un-snoozes. Persists + broadcasts so the sidebar's mailbox-aware
/// rendering re-categorises the row.
pub fn set_snooze(
    config: &ServerConfig,
    key: &WorkspaceKey,
    until: Option<chrono::DateTime<Utc>>,
) {
    let Some(json) = config
        .store
        .get_workspace(key)
        .ok()
        .flatten()
        .and_then(|r| r.workspace_json)
    else {
        return;
    };
    let mut workspace = match serde_json::from_str::<Workspace>(&json) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("set_snooze: bad JSON for {key}: {e}");
            return;
        }
    };
    workspace.snoozed_until = until;
    let record = WorkspaceRecord {
        key: key.as_str().to_string(),
        created_at: workspace.created_at,
        workspace_json: serde_json::to_string(&workspace).ok(),
    };
    if let Err(e) = config.store.save_workspace(&record) {
        tracing::warn!("save_workspace failed: {e}");
    }
    let _ = config
        .bus
        .send(Event::WorkspaceUpserted(Box::new(workspace)));
}

/// Delete a workspace + all its sessions from the store. Broadcasts
/// `WorkspaceRemoved` so every connected TUI prunes its sidebar row.
/// Used by the sidebar's `Shift-X` two-press kill flow.
///
/// Does NOT delete the worktree directories on disk — that's a
/// future enhancement (needs to also kill any live PTY runners
/// rooted in those paths). For now we just drop the metadata; the
/// worktree dirs survive as ordinary git checkouts the user can
/// reuse or remove manually.
///
/// Also kills every backing terminal (PTY / tmux session) that
/// belonged to the workspace — without this the user's `Shift-X X`
/// hides the tabs in pilot but leaves ghost tmux sessions visible
/// in `tmux ls`, which then re-surface on the next pilot launch
/// via `recover_sessions`.
pub async fn delete_workspace(config: &ServerConfig, key: &WorkspaceKey) {
    let key_str = key.as_str();

    // Snapshot (terminal_id, backend_key) pairs whose backend key
    // belongs to this workspace, then drop the lock before any
    // async backend.kill() calls. The backend key shape is
    // "<session_key>/<session_id>/<kind>" — workspace key is the
    // first slash-separated segment.
    let to_kill: Vec<(pilot_ipc::TerminalId, String)> = {
        let terminals = config.terminals.lock().await;
        terminals
            .iter()
            .filter(|(_, backend_key)| {
                backend_key
                    .split('/')
                    .next()
                    .map(|s| s == key_str)
                    .unwrap_or(false)
            })
            .map(|(tid, k)| (*tid, k.clone()))
            .collect()
    };

    if !to_kill.is_empty() {
        tracing::info!(
            "delete_workspace {key}: killing {} backing terminal(s)",
            to_kill.len()
        );
        for (tid, backend_key) in to_kill {
            if let Err(e) = config.backend.kill(&backend_key).await {
                tracing::warn!("kill {backend_key}: {e}");
            }
            config.terminals.lock().await.remove(&tid);
            // Mirror the daemon-pump's exit broadcast so any
            // still-connected clients see the tab disappear.
            let _ = config.bus.send(Event::TerminalExited {
                terminal_id: tid,
                exit_code: None,
            });
        }
    }

    if let Err(e) = config.store.delete_workspace(key) {
        tracing::warn!("delete_workspace failed: {e}");
    }
    let _ = config.bus.send(Event::WorkspaceRemoved(key.clone()));
}

/// Persist a new `SessionLayout` for one session inside a workspace.
/// The user's tile arrangement (Tabs vs Splits with a tree) is local
/// to the workspace; this writes it through the store and broadcasts
/// `WorkspaceUpserted` so other clients see the new layout.
///
/// No-op when the workspace or session can't be found.
pub fn set_session_layout(
    config: &ServerConfig,
    key: &WorkspaceKey,
    session_id: pilot_core::SessionId,
    layout: pilot_core::SessionLayout,
) {
    let Some(json) = config
        .store
        .get_workspace(key)
        .ok()
        .flatten()
        .and_then(|r| r.workspace_json)
    else {
        return;
    };
    let mut workspace = match serde_json::from_str::<Workspace>(&json) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("set_session_layout: bad JSON for {key}: {e}");
            return;
        }
    };
    let Some(session) = workspace.sessions.iter_mut().find(|s| s.id == session_id) else {
        tracing::debug!("set_session_layout: no session {session_id} in {key}");
        return;
    };
    session.layout = layout;

    let record = WorkspaceRecord {
        key: key.as_str().to_string(),
        created_at: workspace.created_at,
        workspace_json: serde_json::to_string(&workspace).ok(),
    };
    if let Err(e) = config.store.save_workspace(&record) {
        tracing::warn!("save_workspace failed: {e}");
    }
    let _ = config
        .bus
        .send(Event::WorkspaceUpserted(Box::new(workspace)));
}

/// Apply a partial-mark to one activity row. Used by the TUI's
/// auto-mark-on-hover feature so the user can scroll past comments
/// and have them flip read individually, instead of `MarkRead`'s
/// "flip the whole workspace" behavior. Persists + broadcasts.
///
/// No-op when the workspace isn't in the store or `index` is out of
/// range — both are user-driven inputs and we don't want a TUI race
/// (poll deletes a workspace while the user hovers) to crash the
/// daemon.
pub fn mark_activity_read(config: &ServerConfig, key: &WorkspaceKey, index: usize) {
    apply_activity_mark(config, key, index, /*read=*/ true);
}

/// Reverse of `mark_activity_read`. `z` undo binds here.
pub fn unmark_activity_read(config: &ServerConfig, key: &WorkspaceKey, index: usize) {
    apply_activity_mark(config, key, index, /*read=*/ false);
}

fn apply_activity_mark(
    config: &ServerConfig,
    key: &WorkspaceKey,
    index: usize,
    read: bool,
) {
    let Some(json) = config
        .store
        .get_workspace(key)
        .ok()
        .flatten()
        .and_then(|r| r.workspace_json)
    else {
        tracing::debug!("apply_activity_mark: no record for {key}");
        return;
    };
    let mut workspace = match serde_json::from_str::<Workspace>(&json) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("apply_activity_mark: bad JSON for {key}: {e}");
            return;
        }
    };
    if read {
        workspace.mark_activity_read(index);
    } else {
        workspace.unmark_activity_read(index);
    }
    let record = WorkspaceRecord {
        key: key.as_str().to_string(),
        created_at: workspace.created_at,
        workspace_json: serde_json::to_string(&workspace).ok(),
    };
    if let Err(e) = config.store.save_workspace(&record) {
        tracing::warn!("save_workspace failed: {e}");
    }
    let _ = config
        .bus
        .send(Event::WorkspaceUpserted(Box::new(workspace)));
}

/// Apply the user's "mark every activity item read" gesture to a
/// stored workspace and broadcast the change. Activity-seen state is
/// **independent** of the upstream provider state: providers only ever
/// rewrite the activity feed itself; `seen_count` + `read_indices`
/// belong to the local user. Preserving them across polls happens in
/// `upsert`; this function flips them all-read on demand.
///
/// No-op if the workspace isn't in the store.
pub fn mark_workspace_read(config: &ServerConfig, key: &WorkspaceKey) {
    let Some(json) = config
        .store
        .get_workspace(key)
        .ok()
        .flatten()
        .and_then(|r| r.workspace_json)
    else {
        tracing::debug!("mark_workspace_read: no record for {key}");
        return;
    };
    let mut workspace = match serde_json::from_str::<Workspace>(&json) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("mark_workspace_read: bad JSON for {key}: {e}");
            return;
        }
    };
    workspace.mark_read_all();
    workspace.last_viewed_at = Some(Utc::now());

    let record = WorkspaceRecord {
        key: key.as_str().to_string(),
        created_at: workspace.created_at,
        workspace_json: serde_json::to_string(&workspace).ok(),
    };
    if let Err(e) = config.store.save_workspace(&record) {
        tracing::warn!("save_workspace failed: {e}");
    }
    let _ = config
        .bus
        .send(Event::WorkspaceUpserted(Box::new(workspace)));
}

/// Post a top-level reply to the workspace's primary task. Today this
/// targets only GitHub PRs/issues; Linear and other providers can grow
/// into the same shape. On success we don't update the local activity
/// feed inline — the next poll picks up the new comment, which keeps
/// the "what the upstream provider says" invariant intact.
pub async fn post_reply(
    config: &ServerConfig,
    session_key: pilot_core::SessionKey,
    body: String,
) {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return;
    }
    let workspace_key = WorkspaceKey::new(session_key.as_str().to_string());
    let workspace = match config
        .store
        .get_workspace(&workspace_key)
        .ok()
        .flatten()
        .and_then(|r| r.workspace_json)
    {
        Some(json) => match serde_json::from_str::<Workspace>(&json) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("post_reply: bad JSON for {workspace_key}: {e}");
                emit_reply_error(config, &format!("workspace decode failed: {e}"));
                return;
            }
        },
        None => {
            emit_reply_error(config, "workspace not found");
            return;
        }
    };
    let Some(task) = workspace.primary_task() else {
        emit_reply_error(config, "workspace has no task to reply to");
        return;
    };
    let Some(repo) = task.repo.as_deref() else {
        emit_reply_error(config, "task has no repo");
        return;
    };
    // GitHub's PR-comment API takes the issue number; the task id's
    // `key` field is `repo#NNN` for github tasks.
    let pr_number = match extract_pr_number(&task.id.key) {
        Some(n) => n,
        None => {
            emit_reply_error(
                config,
                &format!("can't parse PR number from {}", task.id.key),
            );
            return;
        }
    };

    let chain = CredentialChain::new()
        .with(EnvProvider::new("GH_TOKEN"))
        .with(EnvProvider::new("GITHUB_TOKEN"))
        .with(CommandProvider::new("gh", &["auth", "token"]));
    let cred = match chain.resolve("github").await {
        Ok(c) => c,
        Err(e) => {
            emit_reply_error(config, &format!("github credentials: {e}"));
            return;
        }
    };
    let client = match GhClient::from_credential(cred).await {
        Ok(c) => c,
        Err(e) => {
            emit_reply_error(config, &format!("github client init: {e}"));
            return;
        }
    };
    if let Err(e) = client.post_issue_comment(repo, pr_number, trimmed).await {
        tracing::warn!("post_issue_comment {repo}#{pr_number}: {e}");
        emit_reply_error(config, &format!("post failed: {e}"));
        return;
    }
    tracing::info!("posted reply to {repo}#{pr_number} ({} chars)", trimmed.len());
    // The poller picks up the comment on its next tick and broadcasts
    // a workspace upsert; nothing else to do here.
}

/// Recover the PR/issue number from a `Task.id` string. Pilot ids
/// follow `github:owner/name#1234` for the wire form; we accept both
/// `#NNN` and trailing-`NNN` so legacy encodings keep working.
fn extract_pr_number(task_id: &str) -> Option<u64> {
    if let Some((_, n)) = task_id.rsplit_once('#') {
        return n.parse().ok();
    }
    task_id.rsplit_once('-').and_then(|(_, n)| n.parse().ok())
}

fn emit_reply_error(config: &ServerConfig, msg: &str) {
    let _ = config.bus.send(Event::ProviderError {
        source: "reply".into(),
        message: msg.to_string(),
        detail: String::new(),
        kind: "retryable".into(),
    });
}
