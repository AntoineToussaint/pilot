//! Phased first-run setup flow.
//!
//! Owns the entire screen until done — the main TUI does not render
//! while setup is in progress. Two phases today:
//!
//! 1. **Detecting**: probe each tool concurrently, stream results into
//!    the UI with staggered reveal + a `MIN_VISIBLE_DURATION` floor so
//!    even instant detections feel intentional. Inspired by the
//!    Bubble Tea / charm-style installer aesthetic — braille spinners,
//!    dim "Searching for…" → bold "Found X 1.0.42 ✓".
//!
//! 2. **Integrations**: every detected provider/agent shows up as a
//!    toggleable row. Defaults to all enabled. Lets the user opt out
//!    of, say, Linear even though `LINEAR_API_KEY` is set — useful if
//!    the same machine has both work and personal Linear accounts and
//!    only one matters here.
//!
//! The flow returns a `SetupOutcome` that the caller passes to
//! `polling::spawn` so disabled integrations don't get polled.

use crate::setup::{self, Category, SetupReport, ToolState, ToolStatus};
use crossterm::event::{Event as CEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::{execute, terminal};
use futures_util::StreamExt;
use pilot_core::{KV_KEY_SETUP, PersistedSetup, ProviderConfig};
use pilot_store::Store;
use ratatui::Terminal;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Re-export for callers that want the canonical name.
pub type ProviderFilter = ProviderConfig;

// ── Animation tuning ────────────────────────────────────────────────

/// Braille "MiniDot" spinner. Same set Bubble Tea ships as default.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How fast the spinner cycles.
const FRAME_INTERVAL: Duration = Duration::from_millis(80);

/// Stagger between rows starting their search animation. Even-spaced
/// reveal feels more deliberate than firing them all at once.
const ROW_STAGGER: Duration = Duration::from_millis(120);

/// Each row's spinner stays up at least this long before settling to
/// found/missing. Without this, fast detections flash by faster than
/// the eye registers, and the screen feels broken instead of brisk.
const MIN_VISIBLE_DURATION: Duration = Duration::from_millis(350);

/// After every row settles, hold for a beat before showing the prompt.
const SETTLE_HOLD: Duration = Duration::from_millis(200);

// ── Color palette (lipgloss-inspired) ───────────────────────────────

const ACCENT: Color = Color::Cyan;
const FOUND: Color = Color::Green;
const MISSING: Color = Color::Yellow;
const DIM: Color = Color::DarkGray;

// ── Public surface ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SetupOutcome {
    pub report: SetupReport,
    pub enabled_providers: BTreeSet<String>,
    pub enabled_agents: BTreeSet<String>,
    /// Per-provider filter: which item types and which user roles
    /// the daemon should pull. Keyed by provider id ("github" /
    /// "linear"). See `ProviderConfig::default_for` for the per-id
    /// default sets.
    pub provider_filters: BTreeMap<String, ProviderConfig>,
    /// Per-provider scope selection (org / repo / project ids). Empty
    /// for a provider means "everything the token can see" — the
    /// pre-picker default. The setup flow's `ScopePicker` step is
    /// where the user narrows this; the polling pipeline drops tasks
    /// that don't match (see `polling::filter_github_tasks`).
    pub selected_scopes: BTreeMap<String, BTreeSet<String>>,
}

impl SetupOutcome {
    /// Default: every detected tool is enabled and every provider
    /// gets its provider-specific default filter. The user can opt
    /// out / narrow before continuing.
    pub fn default_enabled(report: SetupReport) -> Self {
        let enabled_providers: BTreeSet<String> = report
            .tools
            .iter()
            .filter(|t| t.category == Category::Provider && t.state.is_found())
            .map(|t| t.id.to_string())
            .collect();
        let enabled_agents = report
            .tools
            .iter()
            .filter(|t| t.category == Category::Agent && t.state.is_found())
            .map(|t| t.id.to_string())
            .collect();
        let provider_filters = enabled_providers
            .iter()
            .map(|id| (id.clone(), ProviderFilter::default_for(id)))
            .collect();
        Self {
            report,
            enabled_providers,
            enabled_agents,
            provider_filters,
            // Empty by default: until the user visits the picker,
            // we subscribe to everything the token can see.
            selected_scopes: BTreeMap::new(),
        }
    }
}

/// One row on a provider-config screen.
#[derive(Debug, Clone)]
pub struct ProviderOption {
    pub key: &'static str,
    pub label: &'static str,
    pub group: &'static str,
}

/// What the user can configure for a given provider. Returned in
/// render order. An empty Vec means the provider has no per-id
/// options — its config screen still shows for symmetry but the user
/// just hits Enter to continue.
pub fn options_for_provider(provider_id: &str) -> Vec<ProviderOption> {
    match provider_id {
        "github" => vec![
            ProviderOption {
                key: "role.author",
                label: "Author — I created it",
                group: "ROLES",
            },
            ProviderOption {
                key: "role.reviewer",
                label: "Reviewer — review requested",
                group: "ROLES",
            },
            ProviderOption {
                key: "role.assignee",
                label: "Assignee — assigned to me",
                group: "ROLES",
            },
            ProviderOption {
                key: "role.mentioned",
                label: "Mentioned — @ me",
                group: "ROLES",
            },
            ProviderOption {
                key: "type.prs",
                label: "Pull Requests",
                group: "ITEM TYPES",
            },
            ProviderOption {
                key: "type.issues",
                label: "Issues",
                group: "ITEM TYPES",
            },
        ],
        "linear" => vec![
            ProviderOption {
                key: "role.author",
                label: "Author — I created it",
                group: "ROLES",
            },
            ProviderOption {
                key: "role.assignee",
                label: "Assignee — assigned to me",
                group: "ROLES",
            },
            ProviderOption {
                key: "role.subscriber",
                label: "Subscriber",
                group: "ROLES",
            },
            ProviderOption {
                key: "role.mentioned",
                label: "Mentioned — @ me",
                group: "ROLES",
            },
        ],
        _ => vec![],
    }
}

// ── Persistence ─────────────────────────────────────────────────────

/// Lift the slice of `SetupOutcome` that survives restarts. The
/// detection `report` is intentionally NOT persisted — we re-detect
/// on every launch so freshly-installed binaries surface in the UI
/// without the user needing to clear anything.
pub fn outcome_to_persisted(o: &SetupOutcome) -> PersistedSetup {
    PersistedSetup {
        enabled_providers: o.enabled_providers.clone(),
        enabled_agents: o.enabled_agents.clone(),
        provider_filters: o.provider_filters.clone(),
        selected_scopes: o.selected_scopes.clone(),
    }
}

/// Combine persisted choices with a fresh detection report.
pub fn persisted_to_outcome(p: PersistedSetup, report: SetupReport) -> SetupOutcome {
    SetupOutcome {
        report,
        enabled_providers: p.enabled_providers,
        enabled_agents: p.enabled_agents,
        provider_filters: p.provider_filters,
        selected_scopes: p.selected_scopes,
    }
}

/// Load the persisted setup from the v2 store's kv table. Returns
/// `None` for missing key OR corrupt JSON OR backend failure — every
/// failure mode is fixed by re-running the setup screen, so we
/// degrade rather than crash startup.
pub fn load_persisted(store: &dyn Store) -> Option<PersistedSetup> {
    let raw = match store.get_kv(KV_KEY_SETUP) {
        Ok(Some(s)) => s,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!("setup config read failed: {e}");
            return None;
        }
    };
    match serde_json::from_str(&raw) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!("setup config corrupt in kv: {e}; treating as missing");
            None
        }
    }
}

/// Save the persisted setup into the v2 store. Best-effort — failures
/// are logged but don't propagate, since saving the preference is
/// never worth blocking startup over.
pub fn save_persisted(store: &dyn Store, p: &PersistedSetup) {
    let json = match serde_json::to_string(p) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("setup config serialize failed: {e}");
            return;
        }
    };
    if let Err(e) = store.set_kv(KV_KEY_SETUP, &json) {
        tracing::warn!("setup config write failed: {e}");
    }
}

/// Human-readable provider name for the title bar.
fn provider_display(id: &str) -> &str {
    match id {
        "github" => "GitHub",
        "linear" => "Linear",
        other => other,
    }
}

/// Index of the next selectable scope. Now that the picker is
/// org-only (every entry is selectable), this is a simple wrap.
/// Kept as a function so future per-org repo drill-down can
/// reintroduce header rows without restructuring.
fn next_selectable(scopes: &[pilot_core::Scope], from: usize) -> Option<usize> {
    let n = scopes.len();
    (n > 0).then(|| (from + 1) % n)
}

fn previous_selectable(scopes: &[pilot_core::Scope], from: usize) -> Option<usize> {
    let n = scopes.len();
    (n > 0).then(|| (from + n - 1) % n)
}

// ── Alignment helpers ──────────────────────────────────────────
//
// Every aligned row renders through `aligned_row` so column starts
// don't drift between Detection rows, ScopePicker rows, and any
// future flat-list phase. Adding a new row variant only needs to
// fill the slots; the widths are encoded once.

/// Fixed column widths for the detection-and-picker layout. The two
/// phases share these so the visual rhythm carries across.
const COL_INDENT: usize = 2; // outer card padding
const COL_CURSOR: usize = 2; // "▸ " or "  "
const COL_CHECKBOX: usize = 4; // "[x] " or "    "
const COL_MARK: usize = 2; // "✓ " / "✗ " / spinner
const COL_NAME: usize = 14; // "Cursor Agent" + a couple spare chars

/// Build a fully-padded row from per-column spans. Each input span
/// is rendered into a fixed-width slot; missing values pad with
/// spaces. `detail` consumes the rest of the row.
fn aligned_row(
    cursor: Option<Span<'static>>,
    checkbox: Option<Span<'static>>,
    mark: Option<Span<'static>>,
    name: Span<'static>,
    detail: Span<'static>,
) -> Line<'static> {
    Line::from(vec![
        Span::raw(" ".repeat(COL_INDENT)),
        pad_to(cursor, COL_CURSOR),
        pad_to(checkbox, COL_CHECKBOX),
        pad_to(mark, COL_MARK),
        pad_span(name, COL_NAME),
        Span::raw(" "),
        detail,
    ])
}

/// Indent matching where `detail` starts in `aligned_row`. Used by
/// secondary lines (install hints) so they line up under the detail
/// column instead of floating off in space.
fn detail_indent() -> String {
    " ".repeat(COL_INDENT + COL_CURSOR + COL_CHECKBOX + COL_MARK + COL_NAME + 1)
}

/// Right-pad an arbitrary `Span` to `width` *visual columns* without
/// losing its style. Counts chars rather than bytes — multibyte
/// glyphs (▸, ✓, ✗) all render in 1 column, so `chars().count()`
/// is the right proxy for visual width given the labels we use.
/// CJK / emoji would need `unicode-width`, but nothing in the setup
/// flow needs that today.
fn pad_span(span: Span<'static>, width: usize) -> Span<'static> {
    let visible = span.content.chars().count();
    let pad = width.saturating_sub(visible);
    if pad == 0 {
        return span;
    }
    let padded = format!("{}{}", span.content, " ".repeat(pad));
    Span::styled(padded, span.style)
}

/// Pad an optional Span to a fixed width. None → all spaces.
fn pad_to(span: Option<Span<'static>>, width: usize) -> Span<'static> {
    match span {
        Some(s) => pad_span(s, width),
        None => Span::raw(" ".repeat(width)),
    }
}

/// Run the setup flow on the given Terminal. Returns the user's
/// final SetupOutcome (with their integration toggles applied) or
/// `None` if they hit Ctrl-C / quit.
///
/// `force_show` keeps the flow visible even when nothing's missing —
/// `pilot --fresh` uses this so devs can review the first-run UX.
pub async fn run(
    term: &mut Terminal<crate::app::AppBackend>,
    force_show: bool,
) -> anyhow::Result<Option<SetupOutcome>> {
    run_with_persistence(term, force_show, None).await
}

/// `run` plus a store to read+save the user's choices. On every
/// launch:
/// - if a persisted config exists AND `force_show` is false, the
///   flow is skipped entirely and the persisted choices are merged
///   with a fresh detection report.
/// - otherwise the flow runs normally and the confirmed outcome is
///   saved to the store so the next launch skips the screen.
pub async fn run_with_persistence(
    term: &mut Terminal<crate::app::AppBackend>,
    force_show: bool,
    store: Option<Arc<dyn Store>>,
) -> anyhow::Result<Option<SetupOutcome>> {
    run_with_persistence_and_scopes(term, force_show, store, Vec::new()).await
}

/// Full-fat entry point: takes per-provider `ScopeSource`s so the
/// runner can drive the `ScopePicker` phase. Production wires
/// `pilot_gh::GhScopes`; tests pass `MockScopeSource` so the picker
/// works without real auth.
pub async fn run_with_persistence_and_scopes(
    term: &mut Terminal<crate::app::AppBackend>,
    force_show: bool,
    store: Option<Arc<dyn Store>>,
    scope_sources: Vec<Box<dyn pilot_core::ScopeSource>>,
) -> anyhow::Result<Option<SetupOutcome>> {
    let report = setup::detect_all().await;

    // Skip-fast: a persisted config means the user already chose, and
    // unless they explicitly asked for the screen via --fresh we
    // shouldn't re-prompt every launch.
    if !force_show
        && let Some(s) = &store
        && let Some(persisted) = load_persisted(&**s)
    {
        return Ok(Some(persisted_to_outcome(persisted, report)));
    }

    // No saved config + nothing missing: also skip (first launch with
    // a fully-green environment).
    if !force_show && report.is_ready() && store.is_none() {
        return Ok(Some(SetupOutcome::default_enabled(report)));
    }

    let mut flow = SetupFlow::new(report);
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(FRAME_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Async fetch state for the picker phases. When the flow
    // enters a loading picker we spawn the matching
    // `ScopeSource::list_scopes` / `list_children` future. The
    // select loop polls it; on resolve we call the corresponding
    // setter / fail method on the flow. Two distinct queues — orgs
    // first, repos after — so the in-flight result tag carries
    // whether it was an org list or a child list.
    use std::pin::Pin;
    enum ScopeFetchTarget {
        Orgs(String),
        Repos {
            provider_id: String,
            parent_id: String,
        },
    }
    type ScopeFut = Pin<
        Box<
            dyn std::future::Future<
                    Output = (
                        ScopeFetchTarget,
                        Result<Vec<pilot_core::Scope>, pilot_core::ProviderError>,
                    ),
                > + Send,
        >,
    >;
    let mut scope_fetch: Option<ScopeFut> = None;
    let scope_sources_arc: std::sync::Arc<Vec<Box<dyn pilot_core::ScopeSource>>> =
        std::sync::Arc::new(scope_sources);

    loop {
        flow.advance(Instant::now());

        // Phase entered ScopePicker with no fetch in flight → start one.
        if let Phase::ScopePicker {
            provider_id,
            loading: true,
            ..
        } = &flow.phase
            && scope_fetch.is_none()
        {
            let target_id = provider_id.clone();
            let sources = scope_sources_arc.clone();
            scope_fetch = Some(Box::pin(async move {
                let result = match sources.iter().find(|s| s.provider_id() == target_id) {
                    Some(src) => src.list_scopes().await,
                    None => Ok(Vec::new()), // no source registered → "all repos"
                };
                (ScopeFetchTarget::Orgs(target_id), result)
            }));
        }

        // Phase entered ScopePickerRepos with no fetch in flight → start one.
        if let Phase::ScopePickerRepos {
            provider_id,
            parent_id,
            loading: true,
            ..
        } = &flow.phase
            && scope_fetch.is_none()
        {
            let target_provider = provider_id.clone();
            let target_parent = parent_id.clone();
            let sources = scope_sources_arc.clone();
            scope_fetch = Some(Box::pin(async move {
                let result = match sources.iter().find(|s| s.provider_id() == target_provider) {
                    Some(src) => src.list_children(&target_parent).await,
                    None => Ok(Vec::new()),
                };
                (
                    ScopeFetchTarget::Repos {
                        provider_id: target_provider,
                        parent_id: target_parent,
                    },
                    result,
                )
            }));
        }

        term.draw(|frame| draw(frame, &flow))?;
        if flow.is_done() {
            let outcome = flow.into_outcome();
            if let Some(s) = &store {
                save_persisted(&**s, &outcome_to_persisted(&outcome));
            }
            return Ok(Some(outcome));
        }

        tokio::select! {
            _ = ticker.tick() => {}
            ev = events.next() => {
                let Some(Ok(ev)) = ev else { break };
                if let CEvent::Key(key) = ev
                    && let Some(action) = handle_key(key, &flow)
                {
                    match action {
                        Dispatch::Quit => return Ok(None),
                        Dispatch::Apply(act) => flow.apply(act),
                    }
                }
            }
            // Pending scope fetch resolved — feed the result back
            // into the flow. `else` arm bypasses this branch when
            // `scope_fetch` is None (most of the setup lifetime).
            Some((target, result)) = async {
                match &mut scope_fetch {
                    Some(fut) => Some(fut.await),
                    None => None,
                }
            } => {
                scope_fetch = None;
                match (target, result) {
                    (ScopeFetchTarget::Orgs(provider_id), Ok(scopes)) => {
                        flow.set_scopes(&provider_id, scopes);
                    }
                    (ScopeFetchTarget::Orgs(provider_id), Err(e)) => {
                        tracing::warn!(
                            provider = %provider_id,
                            error = %e,
                            "org list fetch failed; subscribing to all by default"
                        );
                        flow.fail_scopes(&provider_id);
                    }
                    (
                        ScopeFetchTarget::Repos { provider_id, parent_id },
                        Ok(scopes),
                    ) => {
                        flow.set_repo_scopes(&provider_id, &parent_id, scopes);
                    }
                    (
                        ScopeFetchTarget::Repos { provider_id, parent_id },
                        Err(e),
                    ) => {
                        tracing::warn!(
                            provider = %provider_id,
                            parent = %parent_id,
                            error = %e,
                            "repo list fetch failed; keeping org-level scope"
                        );
                        flow.fail_repo_scopes(&provider_id, &parent_id);
                    }
                }
            }
        }
    }
    Ok(None)
}

/// Convenience entry point: handles raw mode + alt screen so callers
/// (the binary's `app::run`) don't have to think about it. After this
/// returns, the terminal is back to its prior state regardless of how
/// the flow exited.
pub async fn run_with_terminal_setup(force_show: bool) -> anyhow::Result<Option<SetupOutcome>> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;
    let result = run(&mut term, force_show).await;
    let _ = terminal::disable_raw_mode();
    let _ = execute!(term.backend_mut(), terminal::LeaveAlternateScreen);
    let _ = term.show_cursor();
    result
}

// ── State machine ───────────────────────────────────────────────────

#[derive(Debug)]
pub struct SetupFlow {
    pub phase: Phase,
    pub started_at: Instant,
    pub rows: Vec<Row>,
    /// Which rows the user has toggled OFF in the integrations phase.
    /// Inverted-set storage keeps the default ("everything detected
    /// gets enabled") expressible without iterating `rows` to seed it.
    pub disabled: BTreeSet<String>,
    pub cursor: usize,
    pub frame: u64,
    pub all_settled_at: Option<Instant>,
    /// Provider ids still waiting for their config phase. After
    /// Integrations confirms, this is seeded with every enabled
    /// provider; each `ProviderConfig` confirm pops the head onto
    /// `provider_filters` and either advances to the next id or
    /// to Done if empty.
    pub pending_providers: VecDeque<String>,
    /// Per-provider filter state, built up as the user moves through
    /// each provider's config phase. Defaults are seeded as soon as a
    /// provider enters the queue.
    pub provider_filters: BTreeMap<String, ProviderFilter>,
    /// Per-provider scope selection, populated by the picker phase.
    /// Empty = "all scopes" (legacy default; valid for providers
    /// whose runner didn't supply a `ScopeSource`).
    pub selected_scopes: BTreeMap<String, BTreeSet<String>>,
    /// Provider ids still waiting for a `ScopePicker` phase. Seeded
    /// on entry to the picker queue (after the last ProviderConfig
    /// confirms); each picker confirm pops the head and the runner
    /// kicks off the next fetch.
    pub pending_pickers: VecDeque<String>,
    /// Pending per-org repo drill-downs. Seeded when a `ScopePicker`
    /// confirms with one or more orgs selected: one entry per
    /// (provider_id, parent_id, parent_label). Each
    /// `ScopePickerRepos` confirm pops the head.
    pub pending_repo_pickers: VecDeque<(String, String, String)>,
}

/// Phases of the setup flow. Each phase that targets a specific
/// provider carries the `provider_id` so the renderer / dispatcher
/// don't need a separate "current provider" field.
///
/// Order:
///
/// ```text
///   Detecting → Integrations → ProviderConfig*  → ScopePicker* → ScopePickerRepos* → Done
///                              (one per           (one per         (one per selected
///                               provider)          provider with    org from each
///                                                  a ScopeSource)   org picker)
/// ```
///
/// Each `*` queue is skipped when there's nothing to drive it:
/// `ScopePicker` skips for providers with ≤ 1 visible scope or no
/// `ScopeSource`; `ScopePickerRepos` skips for orgs that the runner
/// can't resolve children for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Detecting,
    Integrations,
    ProviderConfig {
        provider_id: String,
    },
    /// Org-level picker. `loading=true` while the runner's
    /// `ScopeSource::list_scopes` future is in flight; once it
    /// resolves, `scopes` is populated and the user can toggle.
    ScopePicker {
        provider_id: String,
        loading: bool,
        scopes: Vec<pilot_core::Scope>,
        selected: BTreeSet<String>,
        cursor: usize,
    },
    /// Per-org repo picker. Drilling into one parent (org) and
    /// listing its repos. Empty selection on confirm keeps the
    /// org-level scope; non-empty replaces it with the chosen
    /// repo ids.
    ScopePickerRepos {
        provider_id: String,
        parent_id: String,
        parent_label: String,
        loading: bool,
        scopes: Vec<pilot_core::Scope>,
        selected: BTreeSet<String>,
        cursor: usize,
    },
    Done,
}

impl Phase {
    pub fn is_detecting(&self) -> bool {
        matches!(self, Phase::Detecting)
    }
    pub fn is_integrations(&self) -> bool {
        matches!(self, Phase::Integrations)
    }
    pub fn current_provider(&self) -> Option<&str> {
        match self {
            Phase::ProviderConfig { provider_id } => Some(provider_id.as_str()),
            Phase::ScopePicker { provider_id, .. } => Some(provider_id.as_str()),
            Phase::ScopePickerRepos { provider_id, .. } => Some(provider_id.as_str()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Row {
    pub tool: ToolStatus,
    pub revealed_at: Instant,
    pub settled_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Move cursor in the integrations list.
    CursorUp,
    CursorDown,
    /// Toggle the tool under the cursor.
    Toggle,
    /// Confirm + advance phase (Detecting→Integrations or Integrations→Done).
    Confirm,
}

#[derive(Debug, Clone, Copy)]
enum Dispatch {
    Apply(Action),
    Quit,
}

impl SetupFlow {
    pub fn new(report: SetupReport) -> Self {
        let started_at = Instant::now();
        let rows = report
            .tools
            .iter()
            .enumerate()
            .map(|(i, tool)| Row {
                tool: tool.clone(),
                revealed_at: started_at + ROW_STAGGER * i as u32,
                settled_at: None,
            })
            .collect();
        Self {
            phase: Phase::Detecting,
            started_at,
            rows,
            disabled: BTreeSet::new(),
            cursor: 0,
            frame: 0,
            all_settled_at: None,
            pending_providers: VecDeque::new(),
            provider_filters: BTreeMap::new(),
            selected_scopes: BTreeMap::new(),
            pending_pickers: VecDeque::new(),
            pending_repo_pickers: VecDeque::new(),
        }
    }

    /// Should the spinner have advanced its frame on `now`?
    fn frame_for(&self, now: Instant) -> u64 {
        (now.duration_since(self.started_at).as_millis() / FRAME_INTERVAL.as_millis()) as u64
    }

    pub fn advance(&mut self, now: Instant) {
        self.frame = self.frame_for(now);
        if self.phase != Phase::Detecting {
            return;
        }

        // Pin each row's settle time to *exactly* `revealed_at +
        // MIN_VISIBLE_DURATION` rather than `now`. That way calling
        // advance() once with a very-late `now` still produces the
        // same final state as calling it many times with monotonic
        // intermediate `now`s — the timing is content-derived, not
        // tick-derived.
        for row in self.rows.iter_mut() {
            if row.settled_at.is_some() {
                continue;
            }
            let eligible = row.revealed_at + MIN_VISIBLE_DURATION;
            if now >= eligible {
                row.settled_at = Some(eligible);
            }
        }

        let all_settled = !self.rows.is_empty() && self.rows.iter().all(|r| r.settled_at.is_some());
        if all_settled {
            // The "all-settled" instant is the latest row's settle —
            // not when we observed it, so the SETTLE_HOLD timing is
            // stable across tick frequencies.
            let last = self
                .rows
                .iter()
                .filter_map(|r| r.settled_at)
                .max()
                .unwrap_or(now);
            self.all_settled_at = Some(last);
            if now.duration_since(last) >= SETTLE_HOLD {
                self.advance_phase();
            }
        }
    }

    fn advance_phase(&mut self) {
        self.phase = match &self.phase {
            Phase::Detecting => {
                self.cursor = self.first_selectable_row().unwrap_or(0);
                Phase::Integrations
            }
            Phase::Integrations => {
                // Seed the provider-config queue from the user's
                // post-Integrations enabled set, in render order.
                self.pending_providers.clear();
                self.pending_pickers.clear();
                for row in &self.rows {
                    if row.tool.category == Category::Provider
                        && row.tool.state.is_found()
                        && !self.disabled.contains(row.tool.id)
                    {
                        let id = row.tool.id.to_string();
                        self.provider_filters
                            .entry(id.clone())
                            .or_insert_with(|| ProviderFilter::default_for(&id));
                        self.pending_providers.push_back(id.clone());
                        // Every enabled provider gets queued for a
                        // ScopePicker phase. The runner skips it for
                        // providers without a `ScopeSource` (e.g.
                        // tests) by short-circuiting in
                        // `next_phase_after_provider_config`.
                        self.pending_pickers.push_back(id);
                    }
                }
                self.next_provider_phase()
            }
            Phase::ProviderConfig { .. } => self.next_provider_phase(),
            Phase::ScopePicker { .. } => self.next_picker_phase(),
            Phase::ScopePickerRepos { .. } => self.next_repo_picker_phase(),
            Phase::Done => Phase::Done,
        };
    }

    /// Pop the next pending provider into a `ProviderConfig` phase,
    /// or move on to the picker queue if no more providers need
    /// configuration.
    fn next_provider_phase(&mut self) -> Phase {
        if let Some(provider_id) = self.pending_providers.pop_front() {
            self.cursor = 0;
            Phase::ProviderConfig { provider_id }
        } else {
            // ProviderConfigs exhausted; hand off to the picker queue.
            self.next_picker_phase()
        }
    }

    /// Pop the next picker phase. **Repo pickers drain before org
    /// pickers** so the user picks an org → immediately drills into
    /// its repos → then moves on to the next org / provider. Without
    /// this ordering all orgs would resolve before any repos, which
    /// breaks the mental flow.
    ///
    /// Queue priority on each call:
    ///   1. pending_repo_pickers (drilling into a just-picked org)
    ///   2. pending_pickers (next provider's orgs)
    ///   3. Done
    fn next_picker_phase(&mut self) -> Phase {
        if let Some((provider_id, parent_id, parent_label)) = self.pending_repo_pickers.pop_front()
        {
            self.cursor = 0;
            return Phase::ScopePickerRepos {
                provider_id,
                parent_id,
                parent_label,
                loading: true,
                scopes: Vec::new(),
                selected: BTreeSet::new(),
                cursor: 0,
            };
        }
        if let Some(provider_id) = self.pending_pickers.pop_front() {
            self.cursor = 0;
            return Phase::ScopePicker {
                provider_id,
                loading: true,
                scopes: Vec::new(),
                selected: BTreeSet::new(),
                cursor: 0,
            };
        }
        Phase::Done
    }

    fn next_repo_picker_phase(&mut self) -> Phase {
        // Repo-picker confirmations route through the unified
        // `next_picker_phase` so the priority rules apply uniformly.
        self.next_picker_phase()
    }

    /// Runner-driven: populate a `ScopePicker` with the fetched
    /// scopes.
    ///
    /// - 0 entries → auto-skip (no choice; advance with no selection).
    /// - 1 entry  → auto-select THAT entry, persist it, queue the
    ///   repo drill-down for it, advance. The user gets the same
    ///   end-state as if they'd hit Space + Enter without the click.
    /// - 2+       → render the picker, wait for user input.
    pub fn set_scopes(&mut self, provider_id: &str, scopes: Vec<pilot_core::Scope>) {
        if let Phase::ScopePicker {
            provider_id: pid,
            loading,
            scopes: dst,
            cursor,
            ..
        } = &mut self.phase
            && pid == provider_id
        {
            *loading = false;
            *dst = scopes;
            *cursor = 0;

            match dst.len() {
                0 => {
                    self.advance_phase();
                }
                1 => {
                    // Auto-select + queue the repo drill-down.
                    let only = dst[0].clone();
                    let pid_owned = provider_id.to_string();
                    self.selected_scopes
                        .entry(pid_owned.clone())
                        .or_default()
                        .insert(only.id.clone());
                    self.pending_repo_pickers.push_back((
                        pid_owned,
                        only.id.clone(),
                        only.label.clone(),
                    ));
                    self.advance_phase();
                }
                _ => {}
            }
        }
    }

    /// Mark a picker as failed (network error, missing token, etc.).
    /// The flow moves on without persisting any selection — same
    /// effect as the user choosing "all repos".
    pub fn fail_scopes(&mut self, provider_id: &str) {
        if matches!(&self.phase, Phase::ScopePicker { provider_id: pid, .. } if pid == provider_id)
        {
            self.advance_phase();
        }
    }

    /// Runner-driven: populate a `ScopePickerRepos` with the
    /// fetched repos for one parent. Same auto-skip semantics as
    /// `set_scopes` (≤ 1 child = no choice to make).
    pub fn set_repo_scopes(
        &mut self,
        provider_id: &str,
        parent_id: &str,
        scopes: Vec<pilot_core::Scope>,
    ) {
        if let Phase::ScopePickerRepos {
            provider_id: pid,
            parent_id: par,
            loading,
            scopes: dst,
            cursor,
            ..
        } = &mut self.phase
            && pid == provider_id
            && par == parent_id
        {
            *loading = false;
            *dst = scopes;
            *cursor = 0;
            if dst.len() <= 1 {
                self.advance_phase();
            }
        }
    }

    /// Mark a repo picker as failed — keep the org-level scope
    /// (the parent stays in `selected_scopes` from the previous
    /// org-picker step) and move on to the next queued picker.
    pub fn fail_repo_scopes(&mut self, provider_id: &str, parent_id: &str) {
        if matches!(
            &self.phase,
            Phase::ScopePickerRepos {
                provider_id: pid,
                parent_id: par,
                ..
            } if pid == provider_id && par == parent_id
        ) {
            self.advance_phase();
        }
    }

    pub fn apply(&mut self, action: Action) {
        match (self.phase.clone(), action) {
            (Phase::Detecting, Action::Confirm) => {
                // Confirm during detection is a "skip animation" gesture.
                let now = Instant::now();
                for row in self.rows.iter_mut() {
                    row.settled_at.get_or_insert(now);
                }
                self.all_settled_at = Some(now - SETTLE_HOLD);
                self.advance_phase();
            }
            (Phase::Integrations, Action::CursorUp) => {
                let n = self.selectable_count();
                if n > 0 {
                    let pos = self.cursor_in_selectables();
                    let next = (pos + n - 1) % n;
                    self.cursor = self.selectable_indices()[next];
                }
            }
            (Phase::Integrations, Action::CursorDown) => {
                let n = self.selectable_count();
                if n > 0 {
                    let pos = self.cursor_in_selectables();
                    let next = (pos + 1) % n;
                    self.cursor = self.selectable_indices()[next];
                }
            }
            (Phase::Integrations, Action::Toggle) => {
                if let Some(row) = self.rows.get(self.cursor)
                    && row.tool.state.is_found()
                {
                    let id = row.tool.id.to_string();
                    if !self.disabled.remove(&id) {
                        self.disabled.insert(id);
                    }
                }
            }
            (Phase::Integrations, Action::Confirm) => self.advance_phase(),
            (Phase::ProviderConfig { provider_id }, Action::CursorUp) => {
                let n = options_for_provider(&provider_id).len();
                if n > 0 {
                    self.cursor = (self.cursor + n - 1) % n;
                }
            }
            (Phase::ProviderConfig { provider_id }, Action::CursorDown) => {
                let n = options_for_provider(&provider_id).len();
                if n > 0 {
                    self.cursor = (self.cursor + 1) % n;
                }
            }
            (Phase::ProviderConfig { provider_id }, Action::Toggle) => {
                let opts = options_for_provider(&provider_id);
                if let Some(opt) = opts.get(self.cursor) {
                    let filter = self
                        .provider_filters
                        .entry(provider_id.clone())
                        .or_insert_with(|| ProviderFilter::default_for(&provider_id));
                    filter.toggle(opt.key);
                }
            }
            (Phase::ProviderConfig { .. }, Action::Confirm) => self.advance_phase(),

            // ── ScopePicker ─────────────────────────────────────
            (Phase::ScopePicker { loading: true, .. }, _) => {
                // Block all input until the runner finishes the
                // fetch and calls set_scopes. Avoids accidental
                // "confirm with empty selection" when the user
                // hammers Enter during a slow API call.
            }
            (
                Phase::ScopePicker {
                    scopes,
                    cursor: pcur,
                    ..
                },
                Action::CursorUp,
            ) => {
                if let Some(prev) = previous_selectable(&scopes, pcur)
                    && let Phase::ScopePicker { cursor, .. } = &mut self.phase
                {
                    *cursor = prev;
                }
            }
            (
                Phase::ScopePicker {
                    scopes,
                    cursor: pcur,
                    ..
                },
                Action::CursorDown,
            ) => {
                if let Some(next) = next_selectable(&scopes, pcur)
                    && let Phase::ScopePicker { cursor, .. } = &mut self.phase
                {
                    *cursor = next;
                }
            }
            (
                Phase::ScopePicker {
                    scopes,
                    cursor: pcur,
                    ..
                },
                Action::Toggle,
            ) => {
                if let Some(scope) = scopes.get(pcur)
                    && let Phase::ScopePicker { selected, .. } = &mut self.phase
                    && !selected.remove(&scope.id)
                {
                    selected.insert(scope.id.clone());
                }
            }
            (
                Phase::ScopePicker {
                    provider_id,
                    scopes,
                    selected,
                    ..
                },
                Action::Confirm,
            ) => {
                if !selected.is_empty() {
                    self.selected_scopes
                        .insert(provider_id.clone(), selected.clone());
                    // Queue a per-org repo drill-down for each
                    // selected org so the user can narrow further
                    // (or skip with Enter to keep org-level scope).
                    for scope in scopes.iter().filter(|s| selected.contains(&s.id)) {
                        self.pending_repo_pickers.push_back((
                            provider_id.clone(),
                            scope.id.clone(),
                            scope.label.clone(),
                        ));
                    }
                }
                self.advance_phase();
            }

            // ── ScopePickerRepos ────────────────────────────────
            (Phase::ScopePickerRepos { loading: true, .. }, _) => {
                // Block input until the runner finishes the fetch.
            }
            (
                Phase::ScopePickerRepos {
                    scopes,
                    cursor: pcur,
                    ..
                },
                Action::CursorUp,
            ) => {
                if let Some(prev) = previous_selectable(&scopes, pcur)
                    && let Phase::ScopePickerRepos { cursor, .. } = &mut self.phase
                {
                    *cursor = prev;
                }
            }
            (
                Phase::ScopePickerRepos {
                    scopes,
                    cursor: pcur,
                    ..
                },
                Action::CursorDown,
            ) => {
                if let Some(next) = next_selectable(&scopes, pcur)
                    && let Phase::ScopePickerRepos { cursor, .. } = &mut self.phase
                {
                    *cursor = next;
                }
            }
            (
                Phase::ScopePickerRepos {
                    scopes,
                    cursor: pcur,
                    ..
                },
                Action::Toggle,
            ) => {
                if let Some(scope) = scopes.get(pcur)
                    && let Phase::ScopePickerRepos { selected, .. } = &mut self.phase
                    && !selected.remove(&scope.id)
                {
                    selected.insert(scope.id.clone());
                }
            }
            (
                Phase::ScopePickerRepos {
                    provider_id,
                    parent_id,
                    selected,
                    ..
                },
                Action::Confirm,
            ) => {
                // Empty selection → keep the org-level scope (already
                // in selected_scopes from the previous picker step).
                // Non-empty selection → REPLACE the org id with the
                // chosen repo ids; the user is opting for finer
                // granularity within this org.
                if !selected.is_empty()
                    && let Some(set) = self.selected_scopes.get_mut(&provider_id)
                {
                    set.remove(&parent_id);
                    for repo_id in selected {
                        set.insert(repo_id);
                    }
                }
                self.advance_phase();
            }
            _ => {}
        }
    }

    fn selectable_indices(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.tool.state.is_found())
            .map(|(i, _)| i)
            .collect()
    }

    fn selectable_count(&self) -> usize {
        self.rows.iter().filter(|r| r.tool.state.is_found()).count()
    }

    fn cursor_in_selectables(&self) -> usize {
        self.selectable_indices()
            .iter()
            .position(|i| *i == self.cursor)
            .unwrap_or(0)
    }

    fn first_selectable_row(&self) -> Option<usize> {
        self.selectable_indices().first().copied()
    }

    pub fn is_done(&self) -> bool {
        matches!(self.phase, Phase::Done)
    }

    /// True if `tool_id` is found AND the user hasn't disabled it.
    pub fn is_enabled(&self, tool_id: &str) -> bool {
        self.rows
            .iter()
            .any(|r| r.tool.id == tool_id && r.tool.state.is_found())
            && !self.disabled.contains(tool_id)
    }

    pub fn into_outcome(self) -> SetupOutcome {
        let report = SetupReport {
            tools: self.rows.iter().map(|r| r.tool.clone()).collect(),
        };
        let enabled_providers: BTreeSet<String> = report
            .tools
            .iter()
            .filter(|t| {
                t.category == Category::Provider
                    && t.state.is_found()
                    && !self.disabled.contains(t.id)
            })
            .map(|t| t.id.to_string())
            .collect();
        let enabled_agents = report
            .tools
            .iter()
            .filter(|t| {
                t.category == Category::Agent && t.state.is_found() && !self.disabled.contains(t.id)
            })
            .map(|t| t.id.to_string())
            .collect();
        // Keep a filter row per enabled provider, defaulting any
        // provider the user didn't actually walk through (which can
        // only happen if they disabled it or skipped via Ctrl-C, but
        // we're robust anyway).
        let provider_filters = enabled_providers
            .iter()
            .map(|id| {
                let f = self
                    .provider_filters
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| ProviderFilter::default_for(id));
                (id.clone(), f)
            })
            .collect();
        SetupOutcome {
            report,
            enabled_providers,
            enabled_agents,
            provider_filters,
            selected_scopes: self.selected_scopes.clone(),
        }
    }
}

// ── Key dispatch ────────────────────────────────────────────────────

pub fn handle_key_for_test(key: KeyEvent, flow: &SetupFlow) -> Option<Action> {
    match handle_key(key, flow) {
        Some(Dispatch::Apply(a)) => Some(a),
        _ => None,
    }
}

fn handle_key(key: KeyEvent, flow: &SetupFlow) -> Option<Dispatch> {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Some(Dispatch::Quit);
    }
    // Integrations + ProviderConfig phases share the same key bindings:
    // up/down/space/enter/q. Detecting only listens for skip + quit.
    let in_select_phase = flow.phase.is_integrations() || flow.phase.current_provider().is_some();
    if flow.phase.is_detecting() {
        return match (key.code, key.modifiers) {
            (KeyCode::Enter | KeyCode::Char(' '), _) => Some(Dispatch::Apply(Action::Confirm)),
            (KeyCode::Char('q'), KeyModifiers::NONE) => Some(Dispatch::Quit),
            _ => None,
        };
    }
    if in_select_phase {
        return match (key.code, key.modifiers) {
            (KeyCode::Up | KeyCode::Char('k'), _) => Some(Dispatch::Apply(Action::CursorUp)),
            (KeyCode::Down | KeyCode::Char('j'), _) => Some(Dispatch::Apply(Action::CursorDown)),
            (KeyCode::Char(' '), _) => Some(Dispatch::Apply(Action::Toggle)),
            (KeyCode::Enter, _) => Some(Dispatch::Apply(Action::Confirm)),
            (KeyCode::Char('q') | KeyCode::Esc, _) => Some(Dispatch::Quit),
            _ => None,
        };
    }
    None
}

// ── Rendering ───────────────────────────────────────────────────────

fn draw(frame: &mut Frame, flow: &SetupFlow) {
    let area = frame.area();
    let card = centered(72, 24, area);
    frame.render_widget(Clear, card);

    let title = match &flow.phase {
        Phase::ProviderConfig { provider_id } => {
            format!(" pilot setup · {} ", provider_display(provider_id))
        }
        Phase::ScopePicker { provider_id, .. } => {
            format!(" pilot setup · {} orgs ", provider_display(provider_id))
        }
        Phase::ScopePickerRepos {
            provider_id,
            parent_label,
            ..
        } => format!(
            " pilot setup · {} · {} repos ",
            provider_display(provider_id),
            parent_label
        ),
        _ => " pilot setup ".into(),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            title,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(card);
    frame.render_widget(block, card);

    let lines = match &flow.phase {
        Phase::ProviderConfig { provider_id } => render_provider_phase(flow, provider_id),
        Phase::ScopePicker { .. } => render_scope_picker(flow),
        Phase::ScopePickerRepos { .. } => render_scope_picker_repos(flow),
        _ => render_detection_phase(flow),
    };

    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
}

fn render_detection_phase(flow: &SetupFlow) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::raw(""));
    lines.push(section_header("PROVIDERS"));
    for (i, row) in flow.rows.iter().enumerate() {
        if row.tool.category == Category::Provider {
            lines.extend(render_row(flow, row, i));
        }
    }
    lines.push(Line::raw(""));
    lines.push(section_header("AGENTS"));
    for (i, row) in flow.rows.iter().enumerate() {
        if row.tool.category == Category::Agent {
            lines.extend(render_row(flow, row, i));
        }
    }
    lines.push(Line::raw(""));
    lines.push(footer(flow));
    lines
}

fn render_provider_phase(flow: &SetupFlow, provider_id: &str) -> Vec<Line<'static>> {
    let opts = options_for_provider(provider_id);
    let filter = flow
        .provider_filters
        .get(provider_id)
        .cloned()
        .unwrap_or_else(|| ProviderFilter::default_for(provider_id));

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("Configure ", Style::default().fg(Color::Gray)),
        Span::styled(
            provider_display(provider_id).to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " — pull items where I am:",
            Style::default().fg(Color::Gray),
        ),
    ]));
    lines.push(Line::raw(""));

    let mut last_group: &str = "";
    for (i, opt) in opts.iter().enumerate() {
        if opt.group != last_group {
            if !last_group.is_empty() {
                lines.push(Line::raw(""));
            }
            lines.push(section_header(opt.group));
            last_group = opt.group;
        }
        let on = filter.has(opt.key);
        let glyph = if on { "[x]" } else { "[ ]" };
        let cursor_marker = if flow.cursor == i { "▸" } else { " " };
        let marker_color = if flow.cursor == i { ACCENT } else { DIM };
        let label_style = if on {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(
                format!("{cursor_marker} {glyph} "),
                Style::default()
                    .fg(marker_color)
                    .add_modifier(if flow.cursor == i {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(opt.label.to_string(), label_style),
        ]));
    }

    // Tail summary: how many providers remain after this one.
    let remaining = flow.pending_providers.len();
    if remaining > 0 {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!(
                "  {remaining} more provider{} after this",
                if remaining == 1 { "" } else { "s" }
            ),
            Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
        )));
    }
    lines.push(Line::raw(""));
    lines.push(footer(flow));
    lines
}

fn render_scope_picker(flow: &SetupFlow) -> Vec<Line<'static>> {
    let Phase::ScopePicker {
        provider_id,
        loading,
        scopes,
        selected,
        cursor,
    } = &flow.phase
    else {
        return Vec::new();
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("Choose ", Style::default().fg(Color::Gray)),
        Span::styled(
            provider_display(provider_id).to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" orgs to subscribe to:", Style::default().fg(Color::Gray)),
    ]));
    lines.push(Line::raw(""));

    if *loading {
        lines.push(Line::from(vec![
            Span::raw(detail_indent()),
            Span::styled(
                "fetching orgs…",
                Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
            ),
        ]));
    } else if scopes.is_empty() {
        lines.push(Line::from(vec![
            Span::raw(detail_indent()),
            Span::styled(
                "no orgs visible to this token",
                Style::default().fg(MISSING).add_modifier(Modifier::ITALIC),
            ),
        ]));
    } else {
        // Org-only flat list. Selecting an org subscribes to every
        // PR / issue under owner/* — repo-level narrowing is a
        // separate drill-down to keep the initial fetch cheap.
        for (i, scope) in scopes.iter().enumerate() {
            let is_cursor = i == *cursor;
            let on = selected.contains(&scope.id);
            let cursor_span = is_cursor.then(|| {
                Span::styled(
                    "▸",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )
            });
            let checkbox_span = Some(Span::styled(
                if on { "[x]" } else { "[ ]" }.to_string(),
                Style::default().fg(if is_cursor { ACCENT } else { DIM }),
            ));
            let label_style = if on {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            lines.push(aligned_row(
                cursor_span,
                checkbox_span,
                None,
                Span::styled(scope.label.clone(), label_style),
                Span::raw(""),
            ));
        }
    }

    // Counter line: "3 selected" so the user knows what they're
    // committing to before confirming. An empty selection means
    // "all repos" (legacy default), which we surface explicitly.
    lines.push(Line::raw(""));
    let n = selected.len();
    let counter = if n == 0 {
        "  empty selection · enter = subscribe to ALL orgs".to_string()
    } else {
        format!("  {n} org{} selected", if n == 1 { "" } else { "s" })
    };
    lines.push(Line::from(Span::styled(
        counter,
        Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
    )));

    lines.push(Line::raw(""));
    lines.push(footer(flow));
    lines
}

fn render_scope_picker_repos(flow: &SetupFlow) -> Vec<Line<'static>> {
    let Phase::ScopePickerRepos {
        provider_id,
        parent_label,
        loading,
        scopes,
        selected,
        cursor,
        ..
    } = &flow.phase
    else {
        return Vec::new();
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("Repos in ", Style::default().fg(Color::Gray)),
        Span::styled(
            parent_label.clone(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  · enter with none selected = subscribe to whole org",
            Style::default().fg(DIM),
        ),
    ]));
    lines.push(Line::raw(""));

    if *loading {
        lines.push(Line::from(vec![
            Span::raw(detail_indent()),
            Span::styled(
                "fetching repos…",
                Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
            ),
        ]));
    } else if scopes.is_empty() {
        lines.push(Line::from(vec![
            Span::raw(detail_indent()),
            Span::styled(
                "no repos visible to this token",
                Style::default().fg(MISSING).add_modifier(Modifier::ITALIC),
            ),
        ]));
    } else {
        for (i, scope) in scopes.iter().enumerate() {
            let is_cursor = i == *cursor;
            let on = selected.contains(&scope.id);
            let cursor_span = is_cursor.then(|| {
                Span::styled(
                    "▸",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )
            });
            let checkbox_span = Some(Span::styled(
                if on { "[x]" } else { "[ ]" }.to_string(),
                Style::default().fg(if is_cursor { ACCENT } else { DIM }),
            ));
            let label_style = if on {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            // Strip the org prefix — `acme/web` → `web` since we're
            // already scoped to one parent.
            let short = scope
                .label
                .split_once('/')
                .map(|(_, r)| r.to_string())
                .unwrap_or_else(|| scope.label.clone());
            lines.push(aligned_row(
                cursor_span,
                checkbox_span,
                None,
                Span::styled(short, label_style),
                Span::raw(""),
            ));
        }
    }

    lines.push(Line::raw(""));
    let n = selected.len();
    let counter = if n == 0 {
        format!("  no repo picked · enter = subscribe to all of {parent_label}/*")
    } else {
        format!("  {n} repo{} selected", if n == 1 { "" } else { "s" })
    };
    lines.push(Line::from(Span::styled(
        counter,
        Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
    )));

    // How many drill-downs are left so the user knows how many
    // confirms remain.
    let remaining = flow.pending_repo_pickers.len();
    if remaining > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "  {remaining} more org{} after this",
                if remaining == 1 { "" } else { "s" }
            ),
            Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
        )));
    }

    lines.push(Line::raw(""));
    lines.push(footer(flow));
    let _ = provider_id;
    lines
}

fn section_header(label: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            label.to_string(),
            Style::default().fg(DIM).add_modifier(Modifier::BOLD),
        ),
    ])
}

fn render_row(flow: &SetupFlow, row: &Row, idx: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let now = Instant::now();
    let now_revealed = now >= row.revealed_at;
    let settled = row.settled_at.is_some();

    let (mark, mark_color) = match (settled, &row.tool.state) {
        (false, _) => {
            let f = (flow.frame as usize) % SPINNER_FRAMES.len();
            (SPINNER_FRAMES[f], ACCENT)
        }
        (true, ToolState::Found { .. }) => ("✓", FOUND),
        (true, ToolState::Missing) => ("✗", MISSING),
    };

    let label_style = if settled {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray).add_modifier(Modifier::DIM)
    };

    let detail = if !settled {
        format!("Searching for {}…", row.tool.display_name)
    } else {
        match &row.tool.state {
            ToolState::Found { detail } => detail.clone(),
            ToolState::Missing => "not found".into(),
        }
    };
    let detail_style = match (settled, &row.tool.state) {
        (false, _) => Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
        (true, ToolState::Found { .. }) => Style::default().fg(Color::Gray),
        (true, ToolState::Missing) => Style::default().fg(DIM),
    };

    // Hide rows whose stagger hasn't arrived yet (don't pollute layout).
    if !now_revealed {
        out.push(Line::raw(""));
        return out;
    }

    let cursor = (flow.phase == Phase::Integrations && flow.cursor == idx).then(|| {
        Span::styled(
            "▸",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    });

    let checkbox = (flow.phase == Phase::Integrations && row.tool.state.is_found()).then(|| {
        let on = !flow.disabled.contains(row.tool.id);
        let glyph = if on { "[x]" } else { "[ ]" };
        Span::styled(
            glyph.to_string(),
            Style::default().fg(if flow.cursor == idx { ACCENT } else { DIM }),
        )
    });

    let mark_span = Span::styled(
        mark.to_string(),
        Style::default().fg(mark_color).add_modifier(Modifier::BOLD),
    );

    out.push(aligned_row(
        cursor,
        checkbox,
        Some(mark_span),
        Span::styled(row.tool.display_name.to_string(), label_style),
        Span::styled(detail, detail_style),
    ));

    if settled && matches!(row.tool.state, ToolState::Missing) && !row.tool.install_hint.is_empty()
    {
        out.push(Line::from(vec![
            Span::raw(detail_indent()),
            Span::styled(
                format!("install: {}", row.tool.install_hint),
                Style::default().fg(MISSING),
            ),
        ]));
    }
    out
}

fn footer(flow: &SetupFlow) -> Line<'static> {
    let text = match &flow.phase {
        Phase::Detecting => "  ".to_string() + "press enter to skip animation · ctrl-c to quit",
        Phase::Integrations
        | Phase::ProviderConfig { .. }
        | Phase::ScopePicker { loading: false, .. }
        | Phase::ScopePickerRepos { loading: false, .. } => {
            "  ".to_string() + "j/k move · space toggle · enter continue · q quit"
        }
        Phase::ScopePicker { loading: true, .. } => "  ".to_string() + "fetching orgs…",
        Phase::ScopePickerRepos { loading: true, .. } => "  ".to_string() + "fetching repos…",
        Phase::Done => "  ".to_string() + "starting…",
    };
    Line::from(Span::styled(text, Style::default().fg(DIM)))
}

fn centered(width: u16, height: u16, r: Rect) -> Rect {
    let w = width.min(r.width);
    let h = height.min(r.height);
    let x = r.x + r.width.saturating_sub(w) / 2;
    let y = r.y + r.height.saturating_sub(h) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}
