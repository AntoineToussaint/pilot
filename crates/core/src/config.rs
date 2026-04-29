//! v2 setup configuration types.
//!
//! These live in pilot_core (rather than pilot_v2_tui) so the daemon
//! can read them out of the store and the providers can filter by
//! them, while the TUI's setup screen is the only thing that writes
//! them. Keys are opaque strings — provider crates know how to
//! interpret `role.author` / `type.prs`; pilot_core stays
//! source-agnostic.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Per-provider scope: a flat set of opaque keys describing which
/// item types and which user roles the daemon should pull. Keys
/// shape (provider-specific):
///
/// - GitHub: `role.author`, `role.reviewer`, `role.assignee`,
///   `role.mentioned`, `type.prs`, `type.issues`.
/// - Linear: `role.author`, `role.assignee`, `role.subscriber`,
///   `role.mentioned`.
///
/// Unknown keys are ignored at apply time so the option schema can
/// grow without invalidating saved configs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProviderConfig {
    pub enabled_keys: BTreeSet<String>,
}

impl ProviderConfig {
    pub fn has(&self, key: &str) -> bool {
        self.enabled_keys.contains(key)
    }

    /// Convenience: does the user want this `TaskRole` for this
    /// provider? Maps the role enum to the corresponding `role.*`
    /// key.
    pub fn allows_role(&self, role: crate::task::TaskRole) -> bool {
        let key = match role {
            crate::task::TaskRole::Author => "role.author",
            crate::task::TaskRole::Reviewer => "role.reviewer",
            crate::task::TaskRole::Assignee => "role.assignee",
            crate::task::TaskRole::Mentioned => "role.mentioned",
        };
        self.has(key)
    }

    pub fn allows_prs(&self) -> bool {
        self.has("type.prs")
    }

    pub fn allows_issues(&self) -> bool {
        self.has("type.issues")
    }

    pub fn toggle(&mut self, key: &str) {
        if !self.enabled_keys.remove(key) {
            self.enabled_keys.insert(key.to_string());
        }
    }

    /// Provider-specific defaults — what most users want without
    /// thinking. Lives here so daemon code can fall back to a
    /// reasonable filter when there's no saved config.
    pub fn default_for(provider_id: &str) -> Self {
        let mut keys = BTreeSet::new();
        match provider_id {
            "github" => {
                keys.insert("role.author".into());
                keys.insert("role.assignee".into());
                keys.insert("type.prs".into());
                keys.insert("type.issues".into());
            }
            "linear" => {
                keys.insert("role.assignee".into());
            }
            _ => {}
        }
        Self { enabled_keys: keys }
    }
}

/// Persisted setup state — read at daemon startup, written by the
/// TUI's setup flow on confirm. Stored in the store's kv table under
/// `KV_KEY_SETUP`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PersistedSetup {
    pub enabled_providers: BTreeSet<String>,
    pub enabled_agents: BTreeSet<String>,
    pub provider_filters: BTreeMap<String, ProviderConfig>,
    /// Per-provider scope selection (org/repo for GH, project for
    /// Linear, …). Empty for a provider means "subscribe to
    /// everything the token can see" — same default as before this
    /// field existed, so upgrading users don't need to re-pick.
    /// Non-empty narrows polling to exactly the listed scope ids.
    #[serde(default)]
    pub selected_scopes: BTreeMap<String, BTreeSet<String>>,
}

impl PersistedSetup {
    pub fn provider_config(&self, id: &str) -> ProviderConfig {
        self.provider_filters
            .get(id)
            .cloned()
            .unwrap_or_else(|| ProviderConfig::default_for(id))
    }

    /// Whether `scope_id` was selected for the given provider. An
    /// empty selection (no scopes ever picked) is treated as "all
    /// scopes allowed" so existing setups keep working — the user
    /// only narrows after explicitly visiting the picker.
    pub fn allows_scope(&self, provider_id: &str, scope_id: &str) -> bool {
        match self.selected_scopes.get(provider_id) {
            Some(set) if !set.is_empty() => set.contains(scope_id),
            _ => true,
        }
    }
}

/// Stable kv key for the persisted setup state. Externalized so the
/// daemon and TUI agree without spelling the same string twice.
pub const KV_KEY_SETUP: &str = "setup";

/// Stable kv key for the persisted pane layout (sidebar width, right
/// vertical split). Read at TUI startup, written whenever the user
/// resizes a pane.
pub const KV_KEY_LAYOUT: &str = "ui.layout";

/// Pane geometry the TUI persists across launches. Numbers are
/// clamped to sensible bounds at apply time so a hand-edited config
/// never paints an unusable layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneLayout {
    /// Sidebar width in columns. Clamp range: 12..=80.
    pub sidebar_width: u16,
    /// Percentage of the right column the activity pane (top half)
    /// gets. The terminal gets the remainder. Clamp range: 0..=100;
    /// values below 5 collapse the activity pane, values above 95
    /// collapse the terminal pane — both legal as a "pin everything
    /// to one half" gesture.
    pub right_top_pct: u16,
}

impl PaneLayout {
    /// Reasonable starting layout: 32-col sidebar, terminal-dominant
    /// right column (activity = 25%, terminal = 75%). The right pane
    /// is intentionally NOT 50/50 — once the user has a session
    /// running, the agent terminal is what they're actively watching
    /// and typing into.
    pub const DEFAULT: PaneLayout = PaneLayout {
        sidebar_width: 32,
        right_top_pct: 25,
    };

    pub fn clamp(self) -> Self {
        Self {
            sidebar_width: self.sidebar_width.clamp(12, 80),
            right_top_pct: self.right_top_pct.clamp(0, 100),
        }
    }

    /// Adjust by deltas. Capped at the clamp range; never panics.
    pub fn nudge(self, sidebar_delta: i16, top_delta: i16) -> Self {
        let sidebar = (self.sidebar_width as i16 + sidebar_delta).max(0) as u16;
        let top = (self.right_top_pct as i16 + top_delta).max(0) as u16;
        PaneLayout {
            sidebar_width: sidebar,
            right_top_pct: top,
        }
        .clamp()
    }
}

impl Default for PaneLayout {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::TaskRole;

    #[test]
    fn allows_role_maps_to_role_dot_key() {
        let mut c = ProviderConfig::default();
        c.enabled_keys.insert("role.author".into());
        assert!(c.allows_role(TaskRole::Author));
        assert!(!c.allows_role(TaskRole::Reviewer));
    }

    #[test]
    fn default_github_includes_both_types_and_two_roles() {
        let c = ProviderConfig::default_for("github");
        assert!(c.allows_role(TaskRole::Author));
        assert!(c.allows_role(TaskRole::Assignee));
        assert!(!c.allows_role(TaskRole::Reviewer));
        assert!(c.allows_prs());
        assert!(c.allows_issues());
    }

    #[test]
    fn default_linear_is_assignee_only() {
        let c = ProviderConfig::default_for("linear");
        assert!(c.allows_role(TaskRole::Assignee));
        assert!(!c.allows_role(TaskRole::Author));
    }

    #[test]
    fn toggle_flips_keys() {
        let mut c = ProviderConfig::default();
        c.toggle("role.author");
        assert!(c.has("role.author"));
        c.toggle("role.author");
        assert!(!c.has("role.author"));
    }

    #[test]
    fn persisted_setup_provider_config_falls_back_to_default() {
        let p = PersistedSetup::default();
        let c = p.provider_config("github");
        assert!(c.allows_prs(), "fallback to GitHub default");
    }

    // ── PaneLayout ────────────────────────────────────────────────

    #[test]
    fn pane_layout_default_is_terminal_dominant() {
        let l = PaneLayout::DEFAULT;
        assert_eq!(l.sidebar_width, 32);
        assert_eq!(l.right_top_pct, 25, "activity at 25%, terminal at 75%");
    }

    #[test]
    fn pane_layout_clamp_bounds_sidebar_width() {
        assert_eq!(
            PaneLayout {
                sidebar_width: 5,
                right_top_pct: 25
            }
            .clamp()
            .sidebar_width,
            12,
            "tiny sidebar widths floor to 12"
        );
        assert_eq!(
            PaneLayout {
                sidebar_width: 200,
                right_top_pct: 25
            }
            .clamp()
            .sidebar_width,
            80,
            "huge sidebar widths cap at 80"
        );
    }

    #[test]
    fn pane_layout_nudge_grows_and_shrinks_safely() {
        let base = PaneLayout::DEFAULT;
        assert_eq!(base.nudge(2, 0).sidebar_width, 34);
        assert_eq!(base.nudge(-2, 0).sidebar_width, 30);
        assert_eq!(base.nudge(0, 5).right_top_pct, 30);
        assert_eq!(base.nudge(0, -5).right_top_pct, 20);
    }

    #[test]
    fn pane_layout_nudge_handles_negative_overflow() {
        // Even a wildly-negative delta from a small starting point
        // never wraps around to a u16 underflow — the clamp catches.
        let small = PaneLayout {
            sidebar_width: 12,
            right_top_pct: 0,
        };
        let bumped = small.nudge(-100, -100);
        assert!(bumped.sidebar_width >= 12);
        assert!(bumped.right_top_pct <= 100);
    }
}
