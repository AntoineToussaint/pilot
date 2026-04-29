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
//! `last_viewed_at` all survive the poll. This is the same contract
//! v1 had via its reducer; v2 just does it inline since there's only
//! one place sessions enter the system.

use crate::ServerConfig;
use chrono::Utc;
use pilot_auth::{CommandProvider, CredentialChain, EnvProvider};
use pilot_core::{ProviderConfig, Task, Workspace, WorkspaceKey};
use pilot_gh::GhClient;
use pilot_linear::LinearClient;
use pilot_store::WorkspaceRecord;
use pilot_v2_ipc::Event;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// Anything that can produce a flat list of `Task`s. Implementations
/// should be cheap to construct and cheap to call repeatedly: they're
/// invoked on every poll tick.
pub trait TaskSource: Send + Sync + 'static {
    /// Short stable name for telemetry / `Event::ProviderError`
    /// (e.g. "github", "linear").
    fn name(&self) -> &str;

    /// Fetch the current set of tasks. Errors propagate — the polling
    /// loop logs them and emits a `ProviderError` event so the TUI can
    /// surface a status indicator without itself doing any fetch.
    fn fetch<'a>(&'a self) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<Task>>> + Send + 'a>>;
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
}

impl TaskSource for GhSource {
    fn name(&self) -> &str {
        "github"
    }
    fn fetch<'a>(&'a self) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<Task>>> + Send + 'a>> {
        Box::pin(async move {
            let raw = self.client.fetch_all().await.map_err(anyhow::Error::from)?;
            Ok(filter_github_tasks(raw, &self.filter, &self.scopes))
        })
    }
}

/// `LinearClient` adapter.
pub struct LinearSource {
    pub client: LinearClient,
    pub filter: ProviderConfig,
}

impl TaskSource for LinearSource {
    fn name(&self) -> &str {
        "linear"
    }
    fn fetch<'a>(&'a self) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<Task>>> + Send + 'a>> {
        Box::pin(async move {
            let raw = self.client.fetch_all().await.map_err(anyhow::Error::from)?;
            Ok(filter_linear_tasks(raw, &self.filter))
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
pub fn build_gh_search_qualifiers(
    filter: &ProviderConfig,
    scopes: &std::collections::BTreeSet<String>,
    username: &str,
) -> Vec<String> {
    let mut quals = Vec::new();
    quals.push(role_qualifier(filter, username));
    if let Some(s) = scope_qualifier(scopes) {
        quals.push(s);
    }
    quals
}

/// One qualifier string covering the user's enabled roles. See
/// [`build_gh_search_qualifiers`] for the rationale.
fn role_qualifier(filter: &ProviderConfig, username: &str) -> String {
    let author = filter.has("role.author");
    let reviewer = filter.has("role.reviewer");
    let assignee = filter.has("role.assignee");
    let mentioned = filter.has("role.mentioned");
    let count = [author, reviewer, assignee, mentioned]
        .iter()
        .filter(|b| **b)
        .count();
    // 0 enabled → `filter_github_tasks` will drop everything later;
    // we still emit `involves:USER` so the request is valid. 4
    // enabled → `involves:USER` is the canonical shorthand for
    // "any of those four", cheaper than a 4-way OR.
    if count == 0 || count == 4 {
        return format!("involves:{username}");
    }
    let mut parts = Vec::new();
    if author {
        parts.push(format!("author:{username}"));
    }
    if reviewer {
        parts.push(format!("review-requested:{username}"));
    }
    if assignee {
        parts.push(format!("assignee:{username}"));
    }
    if mentioned {
        parts.push(format!("mentions:{username}"));
    }
    if parts.len() == 1 {
        parts.into_iter().next().unwrap()
    } else {
        format!("({})", parts.join(" OR "))
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
            // Role gate.
            if !filter.allows_role(t.role) {
                return false;
            }
            // Type gate. URLs containing `/pull/` are PRs;
            // `/issues/` are issues. Anything else (discussions
            // etc.) bypasses the type filter — we don't have a
            // toggle for them.
            let type_ok = if t.url.contains("/pull/") {
                filter.allows_prs()
            } else if t.url.contains("/issues/") {
                filter.allows_issues()
            } else {
                true
            };
            if !type_ok {
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
                return false; // unknown repo + non-empty scopes → drop
            };
            let repo_scope = format!("github:{repo}");
            if scopes.contains(&repo_scope) {
                return true;
            }
            // Org match: "github:owner" allows all of owner/*.
            if let Some((owner, _)) = repo.split_once('/') {
                return scopes.contains(&format!("github:{owner}"));
            }
            false
        })
        .collect()
}

/// Drop Linear tasks whose role isn't enabled. Linear has no
/// PRs-vs-Issues distinction.
pub fn filter_linear_tasks(tasks: Vec<Task>, filter: &ProviderConfig) -> Vec<Task> {
    tasks
        .into_iter()
        .filter(|t| filter.allows_role(t.role))
        .collect()
}

/// Best-effort: build the source set from the user's persisted
/// setup. Each constructed source carries the per-provider filter
/// (role + item-type toggles) and applies it post-fetch. Providers
/// whose id isn't in `enabled_providers` are skipped entirely.
pub async fn sources_for(setup: &pilot_core::PersistedSetup) -> Vec<Box<dyn TaskSource>> {
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
                    // Push the user's role + scope into the GraphQL
                    // search itself so we don't pull PRs we'd just
                    // post-filter away. `client.username()` is what
                    // `from_credential` resolved at construction time.
                    let qualifiers =
                        build_gh_search_qualifiers(&filter, &scopes, client.username());
                    let client = client.with_filters(qualifiers);
                    sources.push(Box::new(GhSource {
                        client,
                        filter,
                        scopes,
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
            })),
            Err(e) => tracing::info!("linear not configured: {e}"),
        }
    }

    sources
}

/// Convenience: build the default source set assuming both providers
/// are enabled with their default filters. Used by binaries that
/// bypass the setup screen (e.g. headless `pilot daemon start` in
/// CI). When a saved `PersistedSetup` exists in the v2 store, prefer
/// that instead.
pub async fn default_sources() -> Vec<Box<dyn TaskSource>> {
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
    sources_for(&setup).await
}

/// Run one poll tick: every source is called once and its tasks are
/// upserted. Errors from one source don't stop the others.
pub async fn tick(config: &ServerConfig, sources: &[Box<dyn TaskSource>]) {
    for source in sources {
        match source.fetch().await {
            Ok(tasks) => {
                tracing::info!(
                    source = source.name(),
                    count = tasks.len(),
                    "poll succeeded"
                );
                for task in tasks {
                    upsert(config, task);
                }
            }
            Err(e) => {
                tracing::warn!(source = source.name(), error = %e, "poll failed");
                let _ = config.bus.send(Event::ProviderError {
                    source: source.name().to_string(),
                    message: e.to_string(),
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
pub fn upsert(config: &ServerConfig, task: Task) {
    let key_str = pilot_core::workspace_key_for(&task);
    let key = WorkspaceKey::new(key_str.clone());

    let existing = config
        .store
        .get_workspace(&key)
        .ok()
        .flatten()
        .and_then(|r| r.workspace_json)
        .and_then(|j| serde_json::from_str::<Workspace>(&j).ok());

    let workspace = match existing {
        Some(mut w) => {
            w.attach_task(task);
            w
        }
        None => Workspace::from_task(task, Utc::now()),
    };

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
