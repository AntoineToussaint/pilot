//! Agent terminal state detection.
//!
//! Detects whether an embedded agent terminal is actively working,
//! idle at a prompt, or asking the user a question -- by inspecting PTY
//! output patterns. This is heuristic-based (agents don't emit
//! machine-readable state signals in interactive mode).

use std::time::Instant;

/// Detected state of an agent terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// Actively producing output (thinking/working).
    Active,
    /// No output for a while -- likely idle at prompt.
    Idle,
    /// Output stopped and last output looks like a question/prompt.
    Asking,
}

/// How long output must be silent before we consider the terminal idle.
const IDLE_THRESHOLD_MS: u128 = 1500;

/// Determine agent state from PTY output timing and content.
pub(crate) fn detect_state(
    last_output_at: Instant,
    recent_output: &[u8],
    prev_state: AgentState,
    asking_patterns: &[String],
) -> AgentState {
    let idle_ms = last_output_at.elapsed().as_millis();

    if idle_ms <= IDLE_THRESHOLD_MS {
        return AgentState::Active;
    }

    // Only re-evaluate once on the Active -> idle transition.
    if prev_state != AgentState::Active {
        return prev_state;
    }

    if detect_asking(recent_output, asking_patterns) {
        AgentState::Asking
    } else {
        AgentState::Idle
    }
}

/// Check recent output for question/prompt patterns.
///
/// Only used as a fallback when Claude's own lifecycle hooks aren't
/// available. Deliberately narrow: scans the last non-empty line for
/// user-configured patterns only, and Claude v2.x UI markers that are
/// part of a multi-line block ending near the bottom. Does NOT look
/// for a generic trailing `?` — the scrollback contains the user's
/// earlier questions, and those create false positives long after
/// Claude has answered and gone idle.
pub(crate) fn detect_asking(recent_output: &[u8], patterns: &[String]) -> bool {
    // 1KB is enough to capture a permission dialog without dragging
    // in the previous user prompt.
    let tail_start = recent_output.len().saturating_sub(1024);
    let tail = String::from_utf8_lossy(&recent_output[tail_start..]);
    let clean = strip_ansi(&tail);
    let trimmed = clean.trim();
    let lower_all = trimmed.to_lowercase();

    // Multi-line permission/confirmation blocks in Claude v2.x.
    // "esc to cancel" / "tab to amend" appear ON the dialog, and
    // only then — they're reliable markers the dialog is open right
    // now (not a leftover from earlier output).
    const CLAUDE_ASKING_MARKERS: &[&str] = &[
        "esc to cancel",
        "tab to amend",
        "enter to confirm",
    ];
    for marker in CLAUDE_ASKING_MARKERS {
        if lower_all.contains(marker) {
            return true;
        }
    }

    // Numbered-option prompt. Claude's mid-turn question UI renders:
    //
    //   What do you want to do?
    //   > 1. Stop and wait for limit to reset
    //     2. Request more
    //
    // Detect by scanning the last ~8 non-empty lines for two or more
    // that match `^\s*\d+\.\s` — a numbered choice block near the
    // cursor. Looser than the "Esc to cancel" markers but matches
    // the many internal Claude dialogs that skip the hook.
    // Tight match: cursor/indent-prefixed `1.` / `12.` followed by a
    // space and non-digit content. Rejects version numbers (`1.2.3`),
    // file-line refs (`foo.rs: 42.`), and `123.` in prose where the
    // number is just a list item in documentation.
    let numbered_count = trimmed
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(8)
        .filter(|l| {
            let t = l.trim_start_matches(['>', ' ']);
            let Some((digits, after)) = t.split_once('.') else {
                return false;
            };
            (1..=2).contains(&digits.len())
                && digits.chars().all(|c| c.is_ascii_digit())
                && after.starts_with(' ')
                && after.len() > 1
        })
        .count();
    if numbered_count >= 2 {
        return true;
    }

    // Turn-complete footer + trailing `?` in Claude's response.
    //
    // Claude renders a subtle footer after every assistant turn:
    //   "* Worked for 54s" / "* Churned for 3m 12s" / etc.
    // If the NEAREST non-empty line above that footer ends with `?`,
    // Claude just asked the user a question in free text (not via
    // the numbered-options UI, not via the permission hook) and is
    // now waiting for a reply. Only looking immediately above the
    // footer keeps the user's own earlier questions from triggering.
    if claude_turn_question(trimmed) {
        return true;
    }

    // User-configured patterns matched against the last non-empty line.
    let last_line = trimmed
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let lower_last = last_line.to_lowercase();
    for pattern in patterns {
        if lower_last.contains(&pattern.to_lowercase()) {
            return true;
        }
    }
    false
}

/// True when Claude's turn-complete footer is present AND the nearest
/// content line above it ends with `?`. See the comment in
/// `detect_asking` for the reasoning behind this narrow scope.
fn claude_turn_question(trimmed: &str) -> bool {
    let lines: Vec<&str> = trimmed.lines().collect();
    // Scan from the bottom for a footer in the last ~6 lines.
    let footer_rel = lines.iter().rev().take(6).position(|l| {
        let t = l.trim_start_matches(['*', ' ']);
        t.starts_with("Worked for ")
            || t.starts_with("Churned for ")
            || t.starts_with("Dilly-dallying")
    });
    let Some(rev_idx) = footer_rel else {
        return false;
    };
    let footer_abs = lines.len().saturating_sub(1 + rev_idx);
    // Find the nearest non-empty line above the footer.
    for i in (0..footer_abs).rev() {
        let t = lines[i].trim_end();
        if !t.is_empty() {
            return t.ends_with('?');
        }
    }
    false
}

#[cfg(test)]
mod turn_question_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn worked_for_footer_plus_trailing_q_triggers_asking() {
        let old = Instant::now() - Duration::from_secs(5);
        let patterns: Vec<String> = vec![];
        let out = b"Verified builds: clean.\n\nThe rebased commit is ready. Want me to push (git push --force-with-lease) or hold?\n\n* Worked for 54s\n";
        assert_eq!(
            detect_state(old, out, AgentState::Active, &patterns),
            AgentState::Asking
        );
    }

    #[test]
    fn worked_for_footer_without_trailing_q_stays_idle() {
        let old = Instant::now() - Duration::from_secs(5);
        let patterns: Vec<String> = vec![];
        let out = b"All done. CI is green.\n\n* Worked for 2m 14s\n";
        assert_eq!(
            detect_state(old, out, AgentState::Active, &patterns),
            AgentState::Idle
        );
    }

    #[test]
    fn churned_for_and_trailing_q_also_triggers() {
        let old = Instant::now() - Duration::from_secs(5);
        let patterns: Vec<String> = vec![];
        let out = b"Should I retry or skip this check?\n* Churned for 3m 12s\n";
        assert_eq!(
            detect_state(old, out, AgentState::Active, &patterns),
            AgentState::Asking
        );
    }

    #[test]
    fn trailing_q_without_footer_does_not_trigger() {
        // User's earlier "is CI green?" with no turn-complete footer
        // afterward (e.g. Claude is still mid-turn) must NOT fire.
        let old = Instant::now() - Duration::from_secs(5);
        let patterns: Vec<String> = vec![];
        let out = b"is CI green?\n\nChecking now...\n";
        assert_eq!(
            detect_state(old, out, AgentState::Active, &patterns),
            AgentState::Idle
        );
    }
}

fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() || next == '~' {
                        break;
                    }
                }
            } else {
                chars.next();
            }
        } else {
            result.push(c);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn test_active_when_recent_output() {
        let now = Instant::now();
        let patterns = vec!["(y/n)".into()];
        let state = detect_state(now, b"output", AgentState::Active, &patterns);
        assert_eq!(state, AgentState::Active);
    }

    #[test]
    fn test_idle_after_timeout() {
        let old = Instant::now() - Duration::from_secs(5);
        let patterns: Vec<String> = vec![];
        let state = detect_state(old, b"normal output\n> ", AgentState::Active, &patterns);
        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn test_asking_on_yn_pattern() {
        let old = Instant::now() - Duration::from_secs(5);
        let patterns = vec!["(y/n)".into(), "allow ".into()];
        let state = detect_state(old, b"Allow Bash(git push)? (y/n)", AgentState::Active, &patterns);
        assert_eq!(state, AgentState::Asking);
    }

    #[test]
    fn test_does_not_match_trailing_question_mark_alone() {
        // REGRESSION: "is CI green?" from the user's own scrollback
        // used to trigger Asking. The heuristic now only matches
        // configured patterns + specific Claude UI markers.
        let old = Instant::now() - Duration::from_secs(5);
        let patterns: Vec<String> = vec![];
        let state = detect_state(old, b"Do you want to continue?", AgentState::Active, &patterns);
        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn test_asking_on_claude_marker() {
        let old = Instant::now() - Duration::from_secs(5);
        let patterns: Vec<String> = vec![];
        let state = detect_state(
            old,
            b"Bash command\n  foo\n\nEsc to cancel - Tab to amend",
            AgentState::Active,
            &patterns,
        );
        assert_eq!(state, AgentState::Asking);
    }

    #[test]
    fn test_asking_on_numbered_options() {
        // Mid-turn Claude dialog: "What do you want to do? 1. ... 2. ..."
        let old = Instant::now() - Duration::from_secs(5);
        let patterns: Vec<String> = vec![];
        let out = b"What do you want to do?\n\n> 1. Stop and wait for limit to reset\n  2. Request more\n\nEnter to confirm \xc2\xb7 Esc to cancel";
        let state = detect_state(old, out, AgentState::Active, &patterns);
        assert_eq!(state, AgentState::Asking);
    }

    #[test]
    fn test_idle_single_numbered_line_does_not_trigger() {
        // A single "1. foo" in the scrollback shouldn't trigger Asking —
        // needs at least two numbered lines in close proximity.
        let old = Instant::now() - Duration::from_secs(5);
        let patterns: Vec<String> = vec![];
        let state = detect_state(
            old,
            b"Done.\n\nHere's the list:\n 1. only one item\nsomething else\n",
            AgentState::Active,
            &patterns,
        );
        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn test_version_numbers_do_not_trigger() {
        // Semver in prose should not match the numbered-options rule.
        let old = Instant::now() - Duration::from_secs(5);
        let patterns: Vec<String> = vec![];
        let out = b"bumped to 1.2.3 and 0.15.0\nshipped with 2.0.1 yesterday\n";
        let state = detect_state(old, out, AgentState::Active, &patterns);
        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn test_file_paths_do_not_trigger() {
        // Hunk paths / file refs should not be misread as numbered options.
        let old = Instant::now() - Duration::from_secs(5);
        let patterns: Vec<String> = vec![];
        let out = b"edited foo.rs and bar.ts\nrewrote main.py\n";
        let state = detect_state(old, out, AgentState::Active, &patterns);
        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn test_trailing_question_from_user_not_asking() {
        // Regression: the user's OWN question text in scrollback was
        // triggering Asking via a trailing `?`. That heuristic has been
        // removed; ensure it stays removed.
        let old = Instant::now() - Duration::from_secs(5);
        let patterns: Vec<String> = vec![];
        let state = detect_state(
            old,
            b"is CI green?\n\nYes. All 12 checks pass.\n",
            AgentState::Active,
            &patterns,
        );
        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn test_two_digit_numbered_options_match() {
        let old = Instant::now() - Duration::from_secs(5);
        let patterns: Vec<String> = vec![];
        let out = b"Pick one:\n 10. foo\n 11. bar\n 12. baz\n";
        let state = detect_state(old, out, AgentState::Active, &patterns);
        assert_eq!(state, AgentState::Asking);
    }

    #[test]
    fn test_three_digit_prose_does_not_match() {
        // "100." or longer as a bare list marker in docs shouldn't fire.
        let old = Instant::now() - Duration::from_secs(5);
        let patterns: Vec<String> = vec![];
        let out = b"Pass rate was\n 100. percent\n 200. total runs\n";
        let state = detect_state(old, out, AgentState::Active, &patterns);
        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn test_stays_idle_once_transitioned() {
        let old = Instant::now() - Duration::from_secs(5);
        let patterns: Vec<String> = vec![];
        let state = detect_state(old, b"text", AgentState::Idle, &patterns);
        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn test_strip_ansi() {
        assert_eq!(strip_ansi("hello"), "hello");
        assert_eq!(strip_ansi("\x1b[32mgreen\x1b[0m"), "green");
        assert_eq!(strip_ansi(""), "");
    }
}
