//! `StatusCtx` — transient UI status: the footer notice and the
//! first-poll spinner. Extracted from `Model` so the three related
//! fields (and the tick / fade helpers that manage them) live in
//! one place instead of being scattered across the orchestrator.

use crate::realm::components::footer::{Notice, NoticeSeverity};
use crate::realm::components::polling::Polling;
use crate::realm::model::Msg;
use std::time::{Duration, Instant};

/// How long retryable notices stay visible before fading. Permanent
/// + auth notices ignore this — they stay until dismissed.
const RETRYABLE_FADE: Duration = Duration::from_secs(5);
/// How long info notices ("Spawning shell…", "Setup saved", etc.)
/// stay visible if their triggering event never lands. Longer than
/// retryable so a slow worktree creation doesn't fade mid-flight.
const INFO_FADE: Duration = Duration::from_secs(15);
/// One-shot hints (e.g. "scroll: this view manages its own scrollback")
/// fade quickly so they don't follow the user around the UI.
const HINT_FADE: Duration = Duration::from_secs(3);
/// Heartbeat interval for the polling-modal spinner. Cheap, keeps
/// the spinner glyph advancing at ~12 fps.
const POLLING_TICK_INTERVAL: Duration = Duration::from_millis(80);

pub(crate) struct StatusCtx {
    /// Most recent footer notice — error, warning, or info. Replaces
    /// the modal-on-every-error UX. Retryable severities auto-fade
    /// after `RETRYABLE_FADE`; permanent + auth stay until cleared.
    pub notice: Option<Notice>,
    /// First-poll progress modal. Set by the on-setup-complete hook
    /// (and the returning-user kickoff path) so users see "Pulling
    /// from github + linear…" instead of an empty sidebar while the
    /// initial poll cycle runs. Cleared on first `WorkspaceUpserted`,
    /// timeout, or any-key dismiss.
    pub polling: Option<Polling>,
    /// Last `tick_direct` instant — drives spinner cadence + timeout
    /// checks at ~50ms granularity from the run loop.
    pub polling_last_tick: Instant,
}

impl StatusCtx {
    pub fn new() -> Self {
        Self {
            notice: None,
            polling: None,
            polling_last_tick: Instant::now(),
        }
    }

    /// Clear an in-flight "Spawning…" notice when the matching spawn
    /// event lands. Other notice messages aren't disturbed.
    pub fn clear_spawning_notice(&mut self) {
        if let Some(n) = &self.notice
            && n.message.starts_with("Spawning")
        {
            self.notice = None;
        }
    }

    /// Auto-fade transient notices. Returns `true` if the notice was
    /// cleared so the caller can redraw. Severity decides the timeout:
    /// - Retryable: 5s. Hiccups self-heal, no need to linger.
    /// - Info: 15s. Long enough for a slow spawn; short enough that
    ///   a stuck notice doesn't follow the user around forever.
    /// - Permanent / Auth: stay until dismissed.
    pub fn tick_notice(&mut self) -> bool {
        let Some(n) = &self.notice else { return false };
        let timeout = match n.severity {
            NoticeSeverity::Retryable => Some(RETRYABLE_FADE),
            NoticeSeverity::Info => Some(INFO_FADE),
            NoticeSeverity::Hint => Some(HINT_FADE),
            NoticeSeverity::Auth | NoticeSeverity::Permanent => None,
        };
        if let Some(t) = timeout
            && n.set_at.elapsed() >= t
        {
            self.notice = None;
            return true;
        }
        false
    }

    /// Drive the polling spinner + termination check from the run
    /// loop. Cheap; called every iteration. Returns Some(msg) when
    /// the polling modal wants to be torn down. Caller redraws when
    /// the inner state actually changed.
    pub fn polling_tick(&mut self) -> Option<Msg> {
        if self.polling_last_tick.elapsed() < POLLING_TICK_INTERVAL {
            return None;
        }
        self.polling_last_tick = Instant::now();
        let polling = self.polling.as_mut()?;
        polling.tick_direct()
    }

    /// Tear down the polling modal. Returns true if there was one to
    /// dismiss (so the caller can redraw).
    pub fn dismiss_polling(&mut self) -> bool {
        self.polling.take().is_some()
    }

    /// Spin up the first-poll progress modal. `sources` is the list
    /// of provider IDs the daemon is about to poll.
    pub fn show_polling(&mut self, sources: Vec<String>) {
        self.polling = Some(Polling::new(sources));
        self.polling_last_tick = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(severity: NoticeSeverity, age: Duration) -> Notice {
        Notice {
            message: "x".into(),
            severity,
            set_at: Instant::now() - age,
        }
    }

    #[test]
    fn empty_status_is_noop_on_tick() {
        let mut s = StatusCtx::new();
        assert!(!s.tick_notice());
        assert!(s.polling_tick().is_none());
        assert!(!s.dismiss_polling());
    }

    #[test]
    fn tick_clears_a_faded_retryable_notice() {
        let mut s = StatusCtx::new();
        s.notice = Some(notice(NoticeSeverity::Retryable, Duration::from_secs(10)));
        assert!(s.tick_notice());
        assert!(s.notice.is_none());
    }

    #[test]
    fn tick_leaves_a_fresh_retryable_alone() {
        let mut s = StatusCtx::new();
        s.notice = Some(notice(NoticeSeverity::Retryable, Duration::from_secs(1)));
        assert!(!s.tick_notice());
        assert!(s.notice.is_some());
    }

    #[test]
    fn tick_never_fades_permanent_or_auth() {
        for sev in [NoticeSeverity::Auth, NoticeSeverity::Permanent] {
            let mut s = StatusCtx::new();
            // Even ancient — should not fade.
            s.notice = Some(notice(sev, Duration::from_secs(60 * 60)));
            assert!(!s.tick_notice(), "{sev:?} should not auto-fade");
            assert!(s.notice.is_some());
        }
    }

    #[test]
    fn info_uses_longer_fade_than_retryable() {
        // 7s old: retryable (5s) fades, info (15s) does not.
        let mut retry = StatusCtx::new();
        retry.notice = Some(notice(NoticeSeverity::Retryable, Duration::from_secs(7)));
        assert!(retry.tick_notice());

        let mut info = StatusCtx::new();
        info.notice = Some(notice(NoticeSeverity::Info, Duration::from_secs(7)));
        assert!(!info.tick_notice());
    }

    #[test]
    fn clear_spawning_notice_only_clears_spawn_messages() {
        let mut s = StatusCtx::new();
        s.notice = Some(Notice::new("Saved", NoticeSeverity::Info));
        s.clear_spawning_notice();
        assert!(s.notice.is_some(), "non-spawn notices must be preserved");

        s.notice = Some(Notice::new("Spawning shell…", NoticeSeverity::Info));
        s.clear_spawning_notice();
        assert!(s.notice.is_none());
    }

    #[test]
    fn show_and_dismiss_polling_round_trip() {
        let mut s = StatusCtx::new();
        s.show_polling(vec!["github".into()]);
        assert!(s.polling.is_some());
        assert!(s.dismiss_polling());
        assert!(s.polling.is_none());
        // Second dismiss is a no-op.
        assert!(!s.dismiss_polling());
    }
}
