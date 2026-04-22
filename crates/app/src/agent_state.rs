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
    ];
    for marker in CLAUDE_ASKING_MARKERS {
        if lower_all.contains(marker) {
            return true;
        }
    }

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
