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
fn detect_asking(recent_output: &[u8], patterns: &[String]) -> bool {
    let tail_start = recent_output.len().saturating_sub(512);
    let tail = String::from_utf8_lossy(&recent_output[tail_start..]);
    let clean = strip_ansi(&tail);
    let trimmed = clean.trim();

    let last_line = trimmed
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let lower = last_line.to_lowercase();

    // Check configurable patterns.
    for pattern in patterns {
        if lower.contains(&pattern.to_lowercase()) {
            return true;
        }
    }
    // Also check if line ends with '?'
    lower.ends_with('?')
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
    fn test_asking_on_question_mark() {
        let old = Instant::now() - Duration::from_secs(5);
        let patterns: Vec<String> = vec![];
        let state = detect_state(old, b"Do you want to continue?", AgentState::Active, &patterns);
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
