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

/// Time-based "armed for `delay`" latch. Same family as
/// `ConfirmLatch`, different trigger — instead of a second press,
/// the elapsed time gates the fire. Used by the right pane's
/// auto-mark-read timer (which used to live as a hand-rolled
/// `Option<Instant>` field with mutations scattered across ~10
/// call sites).
///
/// Contract:
/// - `arm()` records "now" as the start of the countdown.
/// - `disarm()` clears the latch.
/// - `ready(delay)` returns `true` iff the latch is armed and at
///   least `delay` has elapsed.
/// - `progress(delay)` returns `Some(0.0..=1.0)` while armed, for
///   rendering a progress bar.
#[derive(Debug, Default, Clone)]
pub struct TimerLatch {
    armed_at: Option<std::time::Instant>,
}

impl TimerLatch {
    pub fn new() -> Self {
        Self { armed_at: None }
    }

    pub fn arm(&mut self) {
        self.armed_at = Some(std::time::Instant::now());
    }

    pub fn disarm(&mut self) {
        self.armed_at = None;
    }

    pub fn is_armed(&self) -> bool {
        self.armed_at.is_some()
    }

    /// True iff armed and `delay` has elapsed since `arm`.
    pub fn ready(&self, delay: std::time::Duration) -> bool {
        self.armed_at
            .map(|t| t.elapsed() >= delay)
            .unwrap_or(false)
    }

    /// Elapsed fraction of `delay`, clamped to `[0.0, 1.0]`. None
    /// when disarmed.
    pub fn progress(&self, delay: std::time::Duration) -> Option<f32> {
        let elapsed = self.armed_at?.elapsed();
        let ratio = elapsed.as_secs_f32() / delay.as_secs_f32();
        Some(ratio.clamp(0.0, 1.0))
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

    // ── TimerLatch tests ─────────────────────────────────────────

    #[test]
    fn timer_starts_disarmed() {
        let t = TimerLatch::new();
        assert!(!t.is_armed());
        assert!(!t.ready(std::time::Duration::from_millis(10)));
        assert_eq!(t.progress(std::time::Duration::from_millis(10)), None);
    }

    #[test]
    fn timer_arm_sets_armed_flag() {
        let mut t = TimerLatch::new();
        t.arm();
        assert!(t.is_armed());
    }

    #[test]
    fn timer_disarm_clears_flag() {
        let mut t = TimerLatch::new();
        t.arm();
        t.disarm();
        assert!(!t.is_armed());
    }

    #[test]
    fn timer_progress_grows_while_armed() {
        let mut t = TimerLatch::new();
        t.arm();
        // Immediately after arm the ratio is near 0 but not None.
        let p = t.progress(std::time::Duration::from_secs(1));
        assert!(p.is_some_and(|r| (0.0..=1.0).contains(&r)));
    }

    #[test]
    fn timer_ready_after_delay() {
        let mut t = TimerLatch::new();
        t.arm();
        std::thread::sleep(std::time::Duration::from_millis(15));
        assert!(t.ready(std::time::Duration::from_millis(10)));
    }
}
