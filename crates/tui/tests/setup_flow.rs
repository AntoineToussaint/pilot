//! Tests for the animated setup flow's pure state-machine logic.
//! The `run` loop's terminal/event-stream plumbing isn't covered here
//! — those would need a synthetic Backend + EventStream which isn't
//! worth the test rig for what is essentially glue. The state
//! machine is where every interesting behavior lives.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pilot_core::ProviderConfig as ProviderFilter;
use pilot_v2_tui::setup::{Category, SetupReport, ToolState, ToolStatus};
use pilot_v2_tui::setup_flow::{
    Action, Phase, SetupFlow, handle_key_for_test, options_for_provider,
};
use std::time::{Duration, Instant};

fn tool(id: &'static str, cat: Category, found: bool) -> ToolStatus {
    ToolStatus {
        id,
        display_name: id,
        category: cat,
        state: if found {
            ToolState::Found {
                detail: format!("{id} v1.0"),
            }
        } else {
            ToolState::Missing
        },
        install_hint: "brew install whatever",
    }
}

fn report(tools: Vec<ToolStatus>) -> SetupReport {
    SetupReport { tools }
}

fn realistic_report() -> SetupReport {
    report(vec![
        tool("github", Category::Provider, true),
        tool("linear", Category::Provider, true),
        tool("claude", Category::Agent, true),
        tool("codex", Category::Agent, false),
        tool("cursor", Category::Agent, false),
    ])
}

fn well_past(d: Duration) -> Duration {
    d * 10
}

// ── Phase progression ──────────────────────────────────────────────

#[test]
fn flow_starts_in_detecting_phase() {
    let flow = SetupFlow::new(realistic_report());
    assert_eq!(flow.phase, Phase::Detecting);
    assert!(!flow.is_done());
}

#[test]
fn advance_eventually_promotes_detecting_to_integrations() {
    let mut flow = SetupFlow::new(realistic_report());
    // Five rows × 120ms stagger + 350ms min visible + 200ms hold —
    // a healthy 5s into the future is way past the settle deadline.
    let now = flow.started_at + well_past(Duration::from_secs(5));
    flow.advance(now);
    assert_eq!(flow.phase, Phase::Integrations);
}

#[test]
fn confirm_during_detection_skips_animation_and_advances() {
    let mut flow = SetupFlow::new(realistic_report());
    flow.apply(Action::Confirm);
    flow.advance(Instant::now());
    assert_eq!(flow.phase, Phase::Integrations);
}

#[test]
fn confirm_walks_through_provider_phases_then_done() {
    // Realistic report has 2 enabled providers (github + linear).
    // After Integrations confirm we should be on github's config
    // screen, then linear's. ScopePicker phases land between each
    // provider's config and Done; the runner is responsible for
    // either populating them or calling `fail_scopes` to skip when
    // no source is registered. This test simulates the no-source
    // path so we land cleanly on Done.
    let mut flow = SetupFlow::new(realistic_report());
    flow.apply(Action::Confirm); // Detecting → Integrations
    flow.advance(Instant::now());
    flow.apply(Action::Confirm); // Integrations → ProviderConfig(github)
    assert_eq!(flow.phase.current_provider(), Some("github"));
    flow.apply(Action::Confirm); // → ProviderConfig(linear)
    assert_eq!(flow.phase.current_provider(), Some("linear"));
    flow.apply(Action::Confirm); // → ScopePicker(github), loading
    flow.fail_scopes("github"); // runner: no source → skip
    flow.fail_scopes("linear"); // runner: no source → skip
    assert!(flow.is_done());
}

#[test]
fn no_enabled_providers_skips_provider_config_phases() {
    // If the user disables every provider during Integrations, there
    // are no per-provider screens to walk through — straight to Done.
    let mut flow = SetupFlow::new(realistic_report());
    flow.apply(Action::Confirm);
    flow.advance(Instant::now());

    // Disable both providers (cursor lands on github first, then walk
    // to linear, toggling each off).
    while flow.rows[flow.cursor].tool.id != "github" {
        flow.apply(Action::CursorDown);
    }
    flow.apply(Action::Toggle); // github off
    flow.apply(Action::CursorDown);
    while flow.rows[flow.cursor].tool.id != "linear" {
        flow.apply(Action::CursorDown);
    }
    flow.apply(Action::Toggle); // linear off

    flow.apply(Action::Confirm);
    assert!(flow.is_done(), "no providers → straight to Done");
}

// ── Cursor + selection ─────────────────────────────────────────────

#[test]
fn cursor_only_lands_on_found_rows() {
    let mut flow = SetupFlow::new(realistic_report());
    flow.apply(Action::Confirm);
    flow.advance(Instant::now());

    // Step through with CursorDown; cursor should never park on
    // codex / cursor (missing rows).
    let mut visited = vec![flow.cursor];
    for _ in 0..10 {
        flow.apply(Action::CursorDown);
        visited.push(flow.cursor);
    }
    for idx in &visited {
        let row = &flow.rows[*idx];
        assert!(
            row.tool.state.is_found(),
            "cursor parked on missing row {}",
            row.tool.id
        );
    }
}

#[test]
fn cursor_wraps_in_both_directions() {
    let mut flow = SetupFlow::new(realistic_report());
    flow.apply(Action::Confirm);
    flow.advance(Instant::now());

    let start = flow.cursor;
    // Find selectable count by pressing Down until we wrap back.
    let mut steps = 0;
    loop {
        flow.apply(Action::CursorDown);
        steps += 1;
        if flow.cursor == start || steps > 10 {
            break;
        }
    }
    assert!(steps >= 2, "expected at least two selectable rows");
    assert_eq!(flow.cursor, start, "down-wraps back to start");

    flow.apply(Action::CursorUp);
    assert_ne!(flow.cursor, start, "up moves away");
}

#[test]
fn toggle_disables_and_re_enables_a_row() {
    let mut flow = SetupFlow::new(realistic_report());
    flow.apply(Action::Confirm);
    flow.advance(Instant::now());
    let initial_id = flow.rows[flow.cursor].tool.id;

    assert!(flow.is_enabled(initial_id));
    flow.apply(Action::Toggle);
    assert!(!flow.is_enabled(initial_id));
    flow.apply(Action::Toggle);
    assert!(flow.is_enabled(initial_id));
}

#[test]
fn missing_rows_are_never_enabled_even_unselected() {
    let flow = SetupFlow::new(realistic_report());
    assert!(!flow.is_enabled("codex"));
    assert!(!flow.is_enabled("cursor"));
}

// ── Outcome ────────────────────────────────────────────────────────

#[test]
fn outcome_keeps_only_enabled_found_tools() {
    let mut flow = SetupFlow::new(realistic_report());
    flow.apply(Action::Confirm);
    flow.advance(Instant::now());

    // Disable Linear specifically.
    while flow.rows[flow.cursor].tool.id != "linear" {
        flow.apply(Action::CursorDown);
    }
    flow.apply(Action::Toggle);

    let outcome = flow.into_outcome();
    assert!(outcome.enabled_providers.contains("github"));
    assert!(!outcome.enabled_providers.contains("linear"));
    assert!(outcome.enabled_agents.contains("claude"));
    assert!(!outcome.enabled_agents.contains("codex"));
    assert!(!outcome.enabled_agents.contains("cursor"));
}

#[test]
fn outcome_default_enables_every_found_tool() {
    let mut flow = SetupFlow::new(realistic_report());
    flow.apply(Action::Confirm);
    flow.advance(Instant::now());
    flow.apply(Action::Confirm);
    let outcome = flow.into_outcome();
    assert_eq!(outcome.enabled_providers.len(), 2);
    assert_eq!(outcome.enabled_agents.len(), 1);
}

// ── Key dispatch ───────────────────────────────────────────────────

#[test]
fn arrows_drive_cursor_in_integrations_phase() {
    let mut flow = SetupFlow::new(realistic_report());
    flow.apply(Action::Confirm);
    flow.advance(Instant::now());
    let action = handle_key_for_test(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &flow);
    assert_eq!(action, Some(Action::CursorDown));
}

#[test]
fn vim_keys_also_drive_cursor() {
    let mut flow = SetupFlow::new(realistic_report());
    flow.apply(Action::Confirm);
    flow.advance(Instant::now());
    assert_eq!(
        handle_key_for_test(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &flow),
        Some(Action::CursorDown),
    );
    assert_eq!(
        handle_key_for_test(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE), &flow),
        Some(Action::CursorUp),
    );
}

#[test]
fn detecting_phase_only_responds_to_confirm() {
    let flow = SetupFlow::new(realistic_report());
    // j/k during the spinner phase shouldn't do anything.
    assert!(
        handle_key_for_test(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &flow).is_none()
    );
    assert_eq!(
        handle_key_for_test(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &flow),
        Some(Action::Confirm)
    );
}

// ── Animation timing ───────────────────────────────────────────────

#[test]
fn rows_settle_only_after_min_visible_duration() {
    let mut flow = SetupFlow::new(realistic_report());
    let just_revealed = flow.started_at + Duration::from_millis(10);
    flow.advance(just_revealed);
    // Even though the first row is "revealed", it shouldn't have
    // settled — that's the whole point of MIN_VISIBLE_DURATION.
    assert!(
        flow.rows[0].settled_at.is_none(),
        "first row settled too eagerly"
    );

    let later = flow.started_at + Duration::from_millis(500);
    flow.advance(later);
    assert!(
        flow.rows[0].settled_at.is_some(),
        "first row should be settled by 500ms"
    );
}

#[test]
fn frame_counter_advances_with_time() {
    let mut flow = SetupFlow::new(realistic_report());
    flow.advance(flow.started_at);
    let f0 = flow.frame;
    flow.advance(flow.started_at + Duration::from_millis(800));
    assert!(flow.frame > f0, "frame should tick during animation");
}

// ── Provider config phase ───────────────────────────────────────────

fn into_provider_phase(report: SetupReport) -> SetupFlow {
    let mut flow = SetupFlow::new(report);
    flow.apply(Action::Confirm); // Detecting → Integrations
    flow.advance(Instant::now());
    flow.apply(Action::Confirm); // Integrations → ProviderConfig(github)
    flow
}

#[test]
fn entering_provider_phase_seeds_default_filter() {
    let flow = into_provider_phase(realistic_report());
    assert_eq!(flow.phase.current_provider(), Some("github"));
    let f = flow.provider_filters.get("github").expect("seeded");
    // Defaults: author + assignee + both item types.
    assert!(f.has("role.author"));
    assert!(f.has("role.assignee"));
    assert!(f.has("type.prs"));
    assert!(f.has("type.issues"));
    assert!(!f.has("role.reviewer"));
}

#[test]
fn provider_phase_toggle_flips_the_keyed_option() {
    let mut flow = into_provider_phase(realistic_report());
    let opts = options_for_provider("github");
    let target_idx = opts.iter().position(|o| o.key == "role.reviewer").unwrap();
    while flow.cursor != target_idx {
        flow.apply(Action::CursorDown);
    }
    assert!(!flow.provider_filters["github"].has("role.reviewer"));
    flow.apply(Action::Toggle);
    assert!(flow.provider_filters["github"].has("role.reviewer"));
    flow.apply(Action::Toggle);
    assert!(!flow.provider_filters["github"].has("role.reviewer"));
}

#[test]
fn provider_cursor_wraps() {
    let mut flow = into_provider_phase(realistic_report());
    let n = options_for_provider("github").len();
    assert!(n >= 2, "github has more than one option");
    let start = flow.cursor;
    for _ in 0..n {
        flow.apply(Action::CursorDown);
    }
    assert_eq!(flow.cursor, start, "wraps after one full cycle");
}

#[test]
fn outcome_carries_per_provider_filter() {
    let mut flow = into_provider_phase(realistic_report());
    // Toggle reviewer on for github, then advance to linear.
    let opts = options_for_provider("github");
    let idx = opts.iter().position(|o| o.key == "role.reviewer").unwrap();
    while flow.cursor != idx {
        flow.apply(Action::CursorDown);
    }
    flow.apply(Action::Toggle); // reviewer ON
    flow.apply(Action::Confirm); // → ProviderConfig(linear)
    flow.apply(Action::Confirm); // → Done

    let outcome = flow.into_outcome();
    let gh = outcome.provider_filters.get("github").unwrap();
    assert!(gh.has("role.reviewer"), "user's toggle persisted");
    let lin = outcome.provider_filters.get("linear").unwrap();
    assert_eq!(lin, &ProviderFilter::default_for("linear"));
}

#[test]
fn provider_filter_default_for_github_includes_both_types() {
    let f = ProviderFilter::default_for("github");
    assert!(f.has("type.prs"));
    assert!(f.has("type.issues"));
}

#[test]
fn provider_filter_default_for_linear_is_assignee_only() {
    let f = ProviderFilter::default_for("linear");
    assert!(f.has("role.assignee"));
    assert!(!f.has("role.author"));
}

// ── Persistence (kv-backed via Store) ────────────────────────────────

#[test]
fn persisted_setup_round_trips_through_store() {
    use pilot_core::PersistedSetup;
    use pilot_store::{MemoryStore, Store};
    use pilot_v2_tui::setup_flow::{load_persisted, save_persisted};

    let store = MemoryStore::new();
    let mut filters = std::collections::BTreeMap::new();
    filters.insert("github".into(), ProviderFilter::default_for("github"));
    let original = PersistedSetup {
        enabled_providers: ["github".to_string()].into_iter().collect(),
        enabled_agents: ["claude".to_string()].into_iter().collect(),
        provider_filters: filters,
        selected_scopes: Default::default(),
    };
    save_persisted(&store as &dyn Store, &original);

    let loaded = load_persisted(&store as &dyn Store).expect("loaded");
    assert_eq!(loaded.enabled_providers, original.enabled_providers);
    assert_eq!(loaded.enabled_agents, original.enabled_agents);
    assert_eq!(
        loaded.provider_filters.get("github"),
        original.provider_filters.get("github"),
    );
}

#[test]
fn load_persisted_returns_none_for_unpopulated_store() {
    use pilot_store::{MemoryStore, Store};
    use pilot_v2_tui::setup_flow::load_persisted;
    let store = MemoryStore::new();
    assert!(load_persisted(&store as &dyn Store).is_none());
}

#[test]
fn load_persisted_treats_corrupt_kv_value_as_missing() {
    use pilot_core::KV_KEY_SETUP;
    use pilot_store::{MemoryStore, Store};
    use pilot_v2_tui::setup_flow::load_persisted;
    let store = MemoryStore::new();
    store.set_kv(KV_KEY_SETUP, "{not-valid").unwrap();
    // Better empty than crash — corrupt config just means re-prompt.
    assert!(load_persisted(&store as &dyn Store).is_none());
}

// ── ScopePicker phase ─────────────────────────────────────────────

mod scope_picker {
    use super::*;
    use pilot_core::{Scope, ScopeKind};

    fn into_picker_phase(provider_id: &str) -> SetupFlow {
        // Walk to the first ScopePicker phase. Realistic report has
        // both github + linear enabled, so we hit ProviderConfig
        // twice before the picker queue starts.
        let mut flow = SetupFlow::new(realistic_report());
        flow.apply(Action::Confirm); // Detecting → Integrations
        flow.advance(Instant::now());
        flow.apply(Action::Confirm); // → ProviderConfig(github)
        flow.apply(Action::Confirm); // → ProviderConfig(linear)
        flow.apply(Action::Confirm); // → ScopePicker(github)
        match &flow.phase {
            Phase::ScopePicker {
                provider_id: pid, ..
            } => assert_eq!(pid, provider_id),
            other => panic!("expected ScopePicker(github), got {other:?}"),
        }
        flow
    }

    fn scope(id: &str, kind: ScopeKind, parent: Option<&str>) -> Scope {
        Scope {
            id: id.into(),
            label: id.into(),
            parent: parent.map(String::from),
            kind,
        }
    }

    #[test]
    fn picker_loading_until_set_scopes_arrives() {
        let mut flow = into_picker_phase("github");
        // Toggle while loading is a no-op (avoid accidental confirm).
        flow.apply(Action::Toggle);
        match &flow.phase {
            Phase::ScopePicker {
                selected, loading, ..
            } => {
                assert!(*loading);
                assert!(selected.is_empty(), "no toggle while loading");
            }
            other => panic!("expected ScopePicker still, got {other:?}"),
        }
    }

    #[test]
    fn set_scopes_with_two_orgs_queues_repo_drilldown() {
        // 2+ orgs to choose from. Picking acme should:
        //   1. record `github:acme` in selected_scopes
        //   2. queue a ScopePickerRepos phase for acme
        // Linear's picker comes AFTER all GitHub repo drill-downs.
        let mut flow = into_picker_phase("github");
        flow.set_scopes(
            "github",
            vec![
                scope("github:acme", ScopeKind::Org, None),
                scope("github:widgets", ScopeKind::Org, None),
            ],
        );
        assert!(matches!(
            flow.phase,
            Phase::ScopePicker { loading: false, .. }
        ));
        flow.apply(Action::Toggle); // cursor at 0 → select acme
        flow.apply(Action::Confirm);
        let selected = flow
            .selected_scopes
            .get("github")
            .cloned()
            .unwrap_or_default();
        assert_eq!(selected.len(), 1);
        assert!(selected.contains("github:acme"));

        // Confirm → ScopePickerRepos(github, acme), loading.
        match &flow.phase {
            Phase::ScopePickerRepos {
                provider_id,
                parent_id,
                loading,
                ..
            } => {
                assert_eq!(provider_id, "github");
                assert_eq!(parent_id, "github:acme");
                assert!(*loading);
            }
            other => panic!("expected ScopePickerRepos(acme), got {other:?}"),
        }
    }

    #[test]
    fn confirming_repo_picker_with_empty_keeps_org_scope() {
        let mut flow = into_picker_phase("github");
        flow.set_scopes("github", vec![scope("github:acme", ScopeKind::Org, None)]);
        // set_scopes with one org auto-confirms past the picker;
        // we should now be in ScopePickerRepos for that org.
        // (Or further along — fast-path through linear if its
        // picker auto-skipped.)
        // Set repos and confirm with no selection.
        flow.set_repo_scopes(
            "github",
            "github:acme",
            vec![
                scope("github:acme/web", ScopeKind::Repo, Some("github:acme")),
                scope("github:acme/api", ScopeKind::Repo, Some("github:acme")),
            ],
        );
        flow.apply(Action::Confirm); // empty selection → keep org
        let selected = flow
            .selected_scopes
            .get("github")
            .cloned()
            .unwrap_or_default();
        assert!(selected.contains("github:acme"));
        assert!(!selected.contains("github:acme/web"));
    }

    #[test]
    fn picking_repos_replaces_org_scope_with_repo_ids() {
        let mut flow = into_picker_phase("github");
        flow.set_scopes("github", vec![scope("github:acme", ScopeKind::Org, None)]);
        flow.set_repo_scopes(
            "github",
            "github:acme",
            vec![
                scope("github:acme/web", ScopeKind::Repo, Some("github:acme")),
                scope("github:acme/api", ScopeKind::Repo, Some("github:acme")),
            ],
        );
        flow.apply(Action::Toggle); // select acme/web (cursor at 0)
        flow.apply(Action::Confirm);
        let selected = flow
            .selected_scopes
            .get("github")
            .cloned()
            .unwrap_or_default();
        // Org scope replaced by the chosen repo.
        assert!(!selected.contains("github:acme"));
        assert!(selected.contains("github:acme/web"));
        assert!(!selected.contains("github:acme/api"));
    }

    #[test]
    fn fail_repo_scopes_keeps_org_scope_and_advances() {
        let mut flow = into_picker_phase("github");
        flow.set_scopes("github", vec![scope("github:acme", ScopeKind::Org, None)]);
        flow.fail_repo_scopes("github", "github:acme");
        let selected = flow
            .selected_scopes
            .get("github")
            .cloned()
            .unwrap_or_default();
        assert!(selected.contains("github:acme"));
    }

    #[test]
    fn set_scopes_with_one_org_autoselects_and_drills_down() {
        // One org = no choice on the org-picker side, so we auto-
        // select it AND queue the repo drill-down. Equivalent to
        // the user pressing Space + Enter on a single-row picker.
        let mut flow = into_picker_phase("github");
        flow.set_scopes(
            "github",
            vec![scope("github:only-org", ScopeKind::Org, None)],
        );
        let selected = flow
            .selected_scopes
            .get("github")
            .cloned()
            .unwrap_or_default();
        assert!(selected.contains("github:only-org"));
        match &flow.phase {
            Phase::ScopePickerRepos { parent_id, .. } => {
                assert_eq!(parent_id, "github:only-org");
            }
            other => panic!("expected ScopePickerRepos, got {other:?}"),
        }
    }

    #[test]
    fn fail_scopes_skips_picker_without_persistence() {
        let mut flow = into_picker_phase("github");
        flow.fail_scopes("github");
        // Past the github picker; no selected_scopes for github.
        assert!(!flow.selected_scopes.contains_key("github"));
    }

    #[test]
    fn cursor_walks_every_org_row() {
        let mut flow = into_picker_phase("github");
        flow.set_scopes(
            "github",
            vec![
                scope("github:acme", ScopeKind::Org, None),
                scope("github:widgets", ScopeKind::Org, None),
                scope("github:foundry", ScopeKind::Org, None),
            ],
        );
        // Cursor starts at 0 (first org).
        match &flow.phase {
            Phase::ScopePicker { cursor, .. } => assert_eq!(*cursor, 0),
            _ => panic!(),
        }
        flow.apply(Action::CursorDown);
        flow.apply(Action::CursorDown);
        match &flow.phase {
            Phase::ScopePicker { cursor, .. } => assert_eq!(*cursor, 2),
            _ => panic!(),
        }
    }
}
