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
pub async fn tick(config: &ServerConfig, sources: &[Box<dyn TaskSource>]) -> TickOutcome {
    let mut state = TickState::default();
    tick_with_state(config, sources, &mut state).await
}

/// Per-loop state that the long-lived `spawn` task threads through
/// `tick` so we can debounce: re-broadcasting the same provider
/// error every 60s spams the TUI with identical hint-bar churn. We
/// only re-broadcast when the error message (or success/failure
/// classification) actually changes for a given source.
#[derive(Default)]
pub struct TickState {
    last_error: std::collections::HashMap<String, String>,
    /// Workspace keys we've already broadcast `WorkspaceOutOfScope`
    /// for. Without this, every 60s tick would re-prompt the user
    /// about the same workspace (they said "no" once, that's
    /// final). Re-entered into the polled set on the next successful
    /// poll that surfaces the workspace again — i.e. once the user
    /// re-adds the filter / scope, we forget the dismissal and the
    /// workspace can produce a fresh prompt later if it falls out
    /// of scope again.
    prompted_out_of_scope: std::collections::HashSet<String>,
}

pub async fn tick_with_state(
    config: &ServerConfig,
    sources: &[Box<dyn TaskSource>],
    state: &mut TickState,
) -> TickOutcome {
    // Track every workspace key we upserted this tick. Callers use
    // it for "in scope" rescoping after the tick — anything in the
    // store NOT in this set is a candidate for removal.
    let mut polled: Vec<WorkspaceKey> = Vec::new();
    // Per-source success tracking. Rescoping needs "did anyone
    // actually report?" — a genuinely empty result set (filter
    // matches nothing) is data; "all sources errored" is not.
    let mut any_source_succeeded = false;
    for source in sources {
        match source.fetch().await {
            Ok(tasks) => {
                any_source_succeeded = true;
                let count = tasks.len();
                tracing::info!(source = source.name(), count, "poll succeeded");
                for task in tasks {
                    let key = WorkspaceKey::new(pilot_core::workspace_key_for(&task));
                    polled.push(key);
                    upsert(config, task).await;
                }
                // Clear the debounce slot — the next failure should
                // broadcast even if it carries the same message as a
                // previous run.
                state.last_error.remove(source.name());
                // Always emit `PollCompleted`, even on 0 tasks, so
                // the TUI can distinguish "polling hasn't run yet"
                // from "polling found nothing matching your filter".
                let _ = config.bus.send(Event::PollCompleted {
                    source: source.name().to_string(),
                    count,
                });
            }
            Err(e) => {
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
                // Debounce: only emit a ProviderError if the message
                // changed since the last failure for this source.
                // Same rate-limit error every minute → one event,
                // not 60/hour.
                let msg = e.user_message();
                let prev = state.last_error.get(source.name());
                if prev.map(String::as_str) != Some(msg.as_str()) {
                    state.last_error.insert(source.name().to_string(), msg.clone());
                    let _ = config.bus.send(Event::ProviderError {
                        source: e.source().to_string(),
                        message: msg,
                        detail: e.diagnostic(),
                        kind: kind.to_string(),
                    });
                }
            }
        }
    }
    TickOutcome {
        polled,
        any_source_succeeded,
    }
}

/// What `tick` / `tick_with_state` returns. The list of workspace
/// keys polled into the store, plus a "did anyone actually report?"
/// flag so callers (rescope) can distinguish "filter genuinely
/// matches nothing today" from "every source failed".
#[derive(Debug, Default)]
pub struct TickOutcome {
    pub polled: Vec<WorkspaceKey>,
    pub any_source_succeeded: bool,
}

/// Compare `polled` against the persisted workspace set; remove
/// workspaces no longer in scope (filter / scope change). Active
/// sessions are preserved — those workspaces stay until the user
/// kills them explicitly (or, in a future phase, confirms removal
/// via a prompt).
///
/// Empty `polled` is treated as "no data this cycle" and skipped —
/// otherwise a single network blip would wipe the whole sidebar.
/// Callers that genuinely want a fresh slate should delete
/// workspaces directly.
pub async fn rescope(config: &ServerConfig, outcome: &TickOutcome) {
    let mut state = TickState::default();
    rescope_with_state(config, outcome, &mut state).await;
}

pub async fn rescope_with_state(
    config: &ServerConfig,
    outcome: &TickOutcome,
    state: &mut TickState,
) {
    // No source produced a successful response — every provider
    // errored out (rate limit, network, auth). Treat as a transient
    // hiccup and skip the rescope; otherwise a single bad minute
    // would wipe the whole sidebar.
    if !outcome.any_source_succeeded {
        return;
    }
    let polled_set: std::collections::HashSet<&str> =
        outcome.polled.iter().map(|k| k.as_str()).collect();
    // Anything we polled is back in scope — drop any "already
    // prompted" memory for it so a future fall-out triggers a fresh
    // prompt.
    state.prompted_out_of_scope.retain(|k| !polled_set.contains(k.as_str()));

    let records = match config.store.list_workspaces() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("rescope: list_workspaces failed: {e}");
            return;
        }
    };

    // Per session_key → count of live terminals. Lets us both
    // detect "has active session" and report the count to the user
    // when prompting.
    let terminal_meta = config.terminal_meta.lock().await;
    let mut active_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (sk, _) in terminal_meta.values() {
        *active_counts.entry(sk.as_str().to_string()).or_default() += 1;
    }
    drop(terminal_meta);

    for r in records {
        if polled_set.contains(r.key.as_str()) {
            continue;
        }
        let key = WorkspaceKey::new(r.key.clone());
        match active_counts.get(r.key.as_str()).copied() {
            None | Some(0) => {
                // Safe to remove silently: nothing's running.
                tracing::info!(
                    workspace_key = %r.key,
                    "rescope: removing out-of-scope workspace"
                );
                delete_workspace(config, &key).await;
                state.prompted_out_of_scope.remove(r.key.as_str());
            }
            Some(count) => {
                // Has active sessions — ask the user, once. Without
                // the dedupe, every 60s tick would re-fire the same
                // prompt for a workspace the user already dismissed.
                if state.prompted_out_of_scope.contains(r.key.as_str()) {
                    continue;
                }
                state.prompted_out_of_scope.insert(r.key.clone());
                // Build a short label + title from the stored workspace
                // JSON if available; fall back to the raw key.
                //
                // `task.id.key` is already `owner/repo#N` (e.g.
                // `tensorzero/tensorzero#7307`) — concatenating `repo`
                // in front of it previously produced
                // `tensorzero/tensorzero#tensorzero/tensorzero#7307`.
                // Trust `id.key` and only fall back to `repo` when the
                // key is missing.
                let task_ref = r
                    .workspace_json
                    .as_deref()
                    .and_then(|json| serde_json::from_str::<pilot_core::Workspace>(json).ok())
                    .and_then(|w| w.primary_task().cloned());
                let (label, title) = match task_ref {
                    Some(t) => {
                        let label = if !t.id.key.is_empty() {
                            t.id.key.clone()
                        } else if let Some(repo) = &t.repo {
                            repo.clone()
                        } else {
                            r.key.clone()
                        };
                        let title = if !t.title.is_empty() {
                            Some(t.title.clone())
                        } else {
                            None
                        };
                        (label, title)
                    }
                    None => (r.key.clone(), None),
                };
                tracing::info!(
                    workspace_key = %r.key,
                    active = count,
                    "rescope: out of scope with active sessions — prompting"
                );
                let _ = config.bus.send(Event::WorkspaceOutOfScope {
                    workspace_key: key,
                    label,
                    title,
                    active_terminal_count: count,
                });
            }
        }
    }
}

/// Spawn the long-lived polling loop. Returns the join handle so the
/// caller can `abort()` on shutdown if it wants — `pilot daemon stop`
/// drops the whole process so we don't bother in main.
///
/// Each tick reads `~/.pilot/config.yaml` fresh and rebuilds the
/// source list. This means a filter / scope change made via the
/// Settings palette takes effect on the NEXT tick at the latest —
/// no separate "respawn polling" plumbing needed, and the previous
/// per-Finish-respawn pattern (which leaked one tokio task per
/// edit) is gone.
pub fn spawn(
    config: ServerConfig,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // First tick fires immediately; subsequent ticks honor `interval`.
        ticker.tick().await;
        run_one_tick(&config).await;
        loop {
            ticker.tick().await;
            run_one_tick(&config).await;
        }
    })
}

/// Test-only entry point: spawn a polling loop with an explicit
/// source list (skips the YAML reload). Production code should use
/// `spawn`; this exists so tests can inject mock `TaskSource`s
/// without writing a config file.
#[doc(hidden)]
pub fn spawn_with_sources(
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
        let mut state = TickState::default();
        ticker.tick().await;
        let outcome = tick_with_state(&config, &sources, &mut state).await;
        rescope_with_state(&config, &outcome, &mut state).await;
        loop {
            ticker.tick().await;
            let outcome = tick_with_state(&config, &sources, &mut state).await;
            rescope_with_state(&config, &outcome, &mut state).await;
        }
    })
}

/// Single iteration of the poll loop. Loads the latest persisted
/// setup, builds sources, ticks, rescopes. Shared between the
/// long-lived spawn and the `Command::Refresh` immediate-tick path.
/// Uses `config.poll_state` so prompt-dismissal memory crosses both
/// paths.
pub async fn run_one_tick(config: &ServerConfig) {
    let setup = match pilot_config::Config::load() {
        Ok(c) => crate::persisted_from_config(&c),
        Err(e) => {
            tracing::warn!("polling: config.yaml load failed: {e}");
            return;
        }
    };
    let sources = sources_for(&setup, config.bus.clone()).await;
    let mut state = config.poll_state.lock().await;
    if sources.is_empty() {
        // User disabled every provider (or credentials all
        // failed to resolve). Treat as "deliberately empty
        // result" — rescope so existing workspaces actually
        // disappear from the sidebar. Without this, unchecking
        // every provider leaves the inbox frozen with stale
        // rows that no current poll source could produce.
        let outcome = TickOutcome {
            polled: vec![],
            any_source_succeeded: true,
        };
        rescope_with_state(config, &outcome, &mut state).await;
        return;
    }
    let outcome = tick_with_state(config, &sources, &mut state).await;
    rescope_with_state(config, &outcome, &mut state).await;
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
///
/// ## Issue → PR collapsing
///
/// GitHub PRs can link to issues via `closingIssuesReferences` (the
/// canonical "Closes #N" / "Fixes #N" mapping). When we observe a PR
/// whose closes_issues lists an issue that already lives in its own
/// standalone workspace, we **merge** the issue workspace into the
/// PR workspace — moving its sessions over (terminals keep running)
/// and dropping the standalone row. Conversely, when an issue is
/// polled and a PR already claims it, we route the issue update into
/// the PR workspace instead of building a duplicate.
pub async fn upsert(config: &ServerConfig, task: Task) {
    // For issues: if a PR somewhere already claims this issue as
    // closed-by, route the upsert into that PR workspace. This is
    // the "issue polled AFTER its PR" path. We only kick in when
    // the issue has no standalone workspace yet — once one exists,
    // either the PR poll will collapse them or the issue's own row
    // remains until the PR shows up. Polling is cheap to scan: the
    // workspace list is bounded by the user's filter scope.
    if !is_pr_task(&task) {
        let issue_key_str = pilot_core::workspace_key_for(&task);
        let issue_key = WorkspaceKey::new(issue_key_str);
        let already_standalone = config
            .store
            .get_workspace(&issue_key)
            .ok()
            .flatten()
            .is_some();
        if !already_standalone
            && let Some(pr_key) = pr_workspace_claiming_issue(config, &task.id)
        {
            tracing::info!(
                issue = %task.id,
                pr_workspace = %pr_key,
                "routing issue upsert into PR workspace (closingIssuesReferences)"
            );
            upsert_into_workspace_key(config, &pr_key, task).await;
            return;
        }
    }

    let key_str = pilot_core::workspace_key_for(&task);
    let key = WorkspaceKey::new(key_str.clone());
    upsert_into_workspace_key(config, &key, task).await;
}

/// Inner upsert: load workspace at `key`, attach the task, migrate
/// linked-issue workspaces if the task is a PR with closing refs,
/// then persist + broadcast. Split out from `upsert` so the
/// "route to PR workspace" path can reuse the same write/broadcast
/// behaviour without duplicating it.
async fn upsert_into_workspace_key(
    config: &ServerConfig,
    key: &WorkspaceKey,
    task: Task,
) {
    let existing = config
        .store
        .get_workspace(key)
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

    // If this workspace's primary task is a PR that closes issues,
    // pull any standalone issue workspaces into it. Runs **before**
    // session-path migration so the migration sees the final session
    // set (issue sessions included) and renames their worktrees to
    // the PR slug in one pass.
    merge_closing_issue_workspaces(config, &mut workspace).await;

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
        // Bumped to error: a store write failure means the workspace
        // we just broadcast won't survive a restart. Caller side
        // can't currently see this, but at least the log is loud.
        tracing::error!(
            workspace_key = %record.key,
            "save_workspace failed: {e}"
        );
    }
    let _ = config
        .bus
        .send(Event::WorkspaceUpserted(Box::new(workspace)));
}

/// Heuristic for "is this Task the PR side of a PR/issue pair?". We
/// classify on URL (the same rule [`pilot_core::workspace::classify`]
/// uses), so a single source of truth governs both "which slot does
/// this task fill?" and "should I look up closing-issue references?".
fn is_pr_task(task: &Task) -> bool {
    task.url.contains("/pull/")
}

/// Scan stored workspaces for one whose PR claims `issue_id` via
/// `closes_issues`. Returns the PR's workspace key when a match is
/// found. Linear in the workspace count — fine in practice (10s to
/// low 100s of workspaces).
fn pr_workspace_claiming_issue(
    config: &ServerConfig,
    issue_id: &pilot_core::TaskId,
) -> Option<WorkspaceKey> {
    let records = config.store.list_workspaces().ok()?;
    for record in records {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(ws) = serde_json::from_str::<Workspace>(&json) else {
            continue;
        };
        let Some(pr) = &ws.pr else {
            continue;
        };
        if pr.closes_issues.iter().any(|id| id == issue_id) {
            return Some(ws.key);
        }
    }
    None
}

/// If `workspace`'s PR closes issues that pilot tracks as their own
/// workspaces, fold each issue's workspace into `workspace` and
/// remove the standalone row. Sessions move over (terminals keep
/// running); `terminal_meta` is rewritten so wire-side events for
/// the old session_key flow to the new one.
///
/// No-op when there's no PR, no `closes_issues`, or no matching
/// issue workspace exists yet.
async fn merge_closing_issue_workspaces(
    config: &ServerConfig,
    workspace: &mut Workspace,
) {
    let Some(pr) = workspace.pr.as_ref() else {
        return;
    };
    if pr.closes_issues.is_empty() {
        return;
    }

    let pr_session_key: pilot_core::SessionKey = (&workspace.key).into();
    let mut closed_ids: Vec<pilot_core::TaskId> = pr.closes_issues.clone();
    closed_ids.dedup();

    for issue_id in closed_ids {
        // The issue's standalone workspace key is whatever
        // workspace_key_for() would have produced when the issue was
        // first upserted. We synthesize that with a Task fragment
        // since we only have the TaskId here — the slug logic uses
        // source + key.
        let stub = Task {
            id: issue_id.clone(),
            title: String::new(),
            body: None,
            state: pilot_core::TaskState::Open,
            role: pilot_core::TaskRole::Author,
            ci: pilot_core::CiStatus::None,
            review: pilot_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: String::new(),
            repo: None,
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
            closes_issues: vec![],
        };
        let issue_key = WorkspaceKey::new(pilot_core::workspace_key_for(&stub));
        if issue_key == workspace.key {
            // Self-link — nothing to merge.
            continue;
        }
        let Some(record) = config.store.get_workspace(&issue_key).ok().flatten() else {
            continue;
        };
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(issue_ws) = serde_json::from_str::<Workspace>(&json) else {
            continue;
        };

        // 1. Move sessions. Each session carries its own
        // workspace_key; rewrite it so future `worktree_slug` /
        // path-migration sees the PR as the owner.
        let issue_session_key: pilot_core::SessionKey = (&issue_key).into();
        for mut session in issue_ws.sessions {
            session.workspace_key = workspace.key.clone();
            workspace.add_session(session);
        }

        // 2. Carry the issue Task data forward so the PR row shows
        // a "linked issue: …" entry. If the issue workspace had its
        // own gh_issues / linear_issues lists, splice those in too.
        for issue_task in &issue_ws.gh_issues {
            workspace.attach_task(issue_task.clone());
        }
        for issue_task in &issue_ws.linear_issues {
            workspace.attach_task(issue_task.clone());
        }

        // 3. Rewrite terminal_meta so any terminals previously
        // bound to the issue's session_key now route to the PR's
        // session_key. Without this, reconnecting TUI clients would
        // see orphan terminals (workspace gone, terminal still wired
        // to its old key).
        let mut meta = config.terminal_meta.lock().await;
        for (_tid, entry) in meta.iter_mut() {
            if entry.0 == issue_session_key {
                entry.0 = pr_session_key.clone();
            }
        }
        drop(meta);

        // 4. Drop the issue workspace from the store + broadcast
        // its removal. We call `store.delete_workspace` directly
        // (not `delete_workspace(config, ..)`) so we DON'T trigger
        // the kill-loop in that helper — the terminals are alive
        // and well, just rebadged. A bus broadcast keeps every
        // connected TUI in sync without going through the kill path.
        if let Err(e) = config.store.delete_workspace(&issue_key) {
            tracing::warn!(
                issue_workspace = %issue_key,
                "delete_workspace during PR merge failed: {e}"
            );
        }
        let _ = config
            .bus
            .send(Event::WorkspaceRemoved(issue_key.clone()));

        tracing::info!(
            issue_workspace = %issue_key,
            pr_workspace = %workspace.key,
            "merged issue workspace into PR (closingIssuesReferences)"
        );
    }
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
        // Bumped to error: a store write failure means the workspace
        // we just broadcast won't survive a restart. Caller side
        // can't currently see this, but at least the log is loud.
        tracing::error!(
            workspace_key = %record.key,
            "save_workspace failed: {e}"
        );
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
        // Bumped to error: a store write failure means the workspace
        // we just broadcast won't survive a restart. Caller side
        // can't currently see this, but at least the log is loud.
        tracing::error!(
            workspace_key = %record.key,
            "save_workspace failed: {e}"
        );
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

    // Find every terminal whose session_key matches via
    // terminal_meta — the authoritative wire-side mapping. Earlier
    // we parsed the backend_key prefix, but the backend's session
    // name format isn't part of any contract (tmux now uses
    // `pilot-{repo}-{kind}-{pid}-{n}`); the meta map is. Locks are
    // taken + dropped before async backend.kill() calls.
    let to_kill_ids: Vec<pilot_ipc::TerminalId> = {
        let meta = config.terminal_meta.lock().await;
        meta.iter()
            .filter(|(_, (sk, _))| sk.as_str() == key_str)
            .map(|(tid, _)| *tid)
            .collect()
    };
    let to_kill: Vec<(pilot_ipc::TerminalId, String)> = {
        let terminals = config.terminals.lock().await;
        to_kill_ids
            .into_iter()
            .filter_map(|tid| terminals.get(&tid).map(|k| (tid, k.clone())))
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
            // Clean every auxiliary map too. The pump task will
            // ALSO clean these when wait_exit returns, but that
            // happens on a tokio task with no upper bound on
            // latency. Doing it here closes the window where
            // rescope (or another subsystem) would see an entry
            // for a workspace we just deleted.
            config.terminals.lock().await.remove(&tid);
            config.terminal_meta.lock().await.remove(&tid);
            config.terminal_sessions.lock().await.remove(&tid);
            config.agent_states.lock().await.remove(&tid);
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
        // Bumped to error: a store write failure means the workspace
        // we just broadcast won't survive a restart. Caller side
        // can't currently see this, but at least the log is loud.
        tracing::error!(
            workspace_key = %record.key,
            "save_workspace failed: {e}"
        );
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
        // Bumped to error: a store write failure means the workspace
        // we just broadcast won't survive a restart. Caller side
        // can't currently see this, but at least the log is loud.
        tracing::error!(
            workspace_key = %record.key,
            "save_workspace failed: {e}"
        );
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
        // Bumped to error: a store write failure means the workspace
        // we just broadcast won't survive a restart. Caller side
        // can't currently see this, but at least the log is loud.
        tracing::error!(
            workspace_key = %record.key,
            "save_workspace failed: {e}"
        );
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
