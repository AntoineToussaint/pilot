//! Generic "first press arms, second press fires" latch.
//!
//! Three sidebar actions follow the same contract: `Shift-X` (kill),
//! `Shift-M` (merge), `Shift-Z` (long snooze). Each used to maintain
//! its own `Option<SessionKey>` field + disarm-on-other-key check
//! inline in `Sidebar::handle_key`. Same shape, three copies —
//! perfect for a generic.
//!
//! Contract:
//! - `arm_or_fire(target)` returns `true` when the latch already held
//!   `target` (the user pressed twice in a row on the same row); the
//!   caller treats that as "fire."
//! - Returns `false` on the first press; the caller treats that as
//!   "arm" and renders the appropriate `[…?]` indicator.
//! - `disarm()` clears the latch (called from the handler when the
//!   user presses something other than the latch's key).
//!
//! Tested in isolation here; the call sites pin their per-action
//! semantics separately.

/// "First-press arms, second-press fires" latch. `K` is whatever the
/// caller uses to identify the action target — typically a workspace
/// key. The latch only fires when the second press matches the
/// armed key, so navigating to a different row between presses
/// disarms automatically.
#[derive(Debug, Default, Clone)]
pub struct ConfirmLatch<K> {
    armed: Option<K>,
}

impl<K: PartialEq + Clone> ConfirmLatch<K> {
    pub fn new() -> Self {
        Self { armed: None }
    }

    /// First press on `target` → arms; returns `false` ("don't fire
    /// yet"). Second press on the same `target` → fires; returns
    /// `true` and clears the latch. Press on a different `target`
    /// re-arms with the new one.
    pub fn arm_or_fire(&mut self, target: K) -> bool {
        if self.armed.as_ref() == Some(&target) {
            self.armed = None;
            return true;
        }
        self.armed = Some(target);
        false
    }

    /// Force-clear the latch. Called when any non-latch key arrives
    /// so a user typing `Shift-X j` doesn't leave the kill prompt
    /// armed.
    pub fn disarm(&mut self) {
        self.armed = None;
    }

    /// Currently armed target, for rendering `[kill?]` /
    /// `[merge?]` markers next to the row.
    pub fn armed(&self) -> Option<&K> {
        self.armed.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_press_arms_does_not_fire() {
        let mut latch: ConfirmLatch<&'static str> = ConfirmLatch::new();
        assert!(!latch.arm_or_fire("a"));
        assert_eq!(latch.armed(), Some(&"a"));
    }

    #[test]
    fn second_press_on_same_target_fires_and_clears() {
        let mut latch: ConfirmLatch<&'static str> = ConfirmLatch::new();
        latch.arm_or_fire("a");
        assert!(latch.arm_or_fire("a"));
        assert_eq!(latch.armed(), None);
    }

    #[test]
    fn second_press_on_different_target_re_arms() {
        // User pressed Shift-X on row A, then walked to row B and
        // pressed Shift-X again — that's a fresh arm, not a fire.
        let mut latch: ConfirmLatch<&'static str> = ConfirmLatch::new();
        latch.arm_or_fire("a");
        assert!(!latch.arm_or_fire("b"));
        assert_eq!(latch.armed(), Some(&"b"));
    }

    #[test]
    fn disarm_clears_armed_state() {
        let mut latch: ConfirmLatch<&'static str> = ConfirmLatch::new();
        latch.arm_or_fire("a");
        latch.disarm();
        assert_eq!(latch.armed(), None);
    }

    #[test]
    fn fire_after_disarm_just_re_arms() {
        let mut latch: ConfirmLatch<&'static str> = ConfirmLatch::new();
        latch.arm_or_fire("a");
        latch.disarm();
        assert!(!latch.arm_or_fire("a"));
        assert_eq!(latch.armed(), Some(&"a"));
    }
}
