//! Task provider abstraction. GitHub, Linear, Jira, etc. implement this trait.
//!
//! ## Error model
//!
//! [`ProviderError`] is classified at the boundary so the polling
//! layer can decide what to do without parsing strings:
//!
//! - **Retryable** — transient: network hiccup, 5xx, rate limit.
//!   Polling logs and retries on the next cycle. The user sees a
//!   terse "<provider> hiccup, retrying" hint, not a full stack.
//! - **Auth** — credentials are wrong / expired. Polling surfaces it
//!   loud (`Event::ProviderError`) with a user-facing message; user
//!   must rotate their token. Not retried until they do.
//! - **Permanent** — query/protocol/programming error. Surfaced with
//!   the diagnostic so dev / users can file a bug. Not retried.
//!
//! Every variant carries:
//! - `source` — provider id (e.g. `"github"`) for grouping in the UI.
//! - `detail` — full chained error string for logs / `diagnostic()`.
//!
//! Display defaults to the *terse* user-facing message; call
//! `diagnostic()` for the full text in dev tooling.

use crate::Task;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// Transient — try again on the next poll cycle.
    Retryable { source: String, detail: String },
    /// Credentials wrong / expired. Don't retry without user action.
    Auth { source: String, detail: String },
    /// Permanent failure. Surface, don't retry.
    Permanent { source: String, detail: String },
}

impl ProviderError {
    pub fn retryable(source: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Retryable {
            source: source.into(),
            detail: detail.into(),
        }
    }

    pub fn auth(source: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Auth {
            source: source.into(),
            detail: detail.into(),
        }
    }

    pub fn permanent(source: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Permanent {
            source: source.into(),
            detail: detail.into(),
        }
    }

    pub fn source(&self) -> &str {
        match self {
            Self::Retryable { source, .. }
            | Self::Auth { source, .. }
            | Self::Permanent { source, .. } => source,
        }
    }

    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Retryable { .. })
    }

    pub fn is_auth(&self) -> bool {
        matches!(self, Self::Auth { .. })
    }

    /// Full diagnostic — provider id + variant tag + the underlying
    /// error chain. Goes to the log file; not shown in the TUI by
    /// default (use `RUST_LOG=debug` or tail `/tmp/pilot.log`).
    pub fn diagnostic(&self) -> String {
        match self {
            Self::Retryable { source, detail } => {
                format!("[{source}] retryable: {detail}")
            }
            Self::Auth { source, detail } => format!("[{source}] auth: {detail}"),
            Self::Permanent { source, detail } => {
                format!("[{source}] permanent: {detail}")
            }
        }
    }

    /// Terse user-facing message. Stays short — the TUI's status bar
    /// is one row.
    pub fn user_message(&self) -> String {
        match self {
            Self::Retryable { source, .. } => {
                format!("{source} hiccup, retrying next cycle")
            }
            Self::Auth { source, .. } => {
                format!("{source} auth failed — rotate token then `pilot --fresh`")
            }
            Self::Permanent { source, detail } => {
                let summary = detail.lines().next().unwrap_or(detail);
                format!("{source}: {summary}")
            }
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Default Display is the user-facing message. Logs use
        // `diagnostic()` explicitly when they want the full chain.
        f.write_str(&self.user_message())
    }
}

impl std::error::Error for ProviderError {}

/// A source of tasks (PRs, issues, tickets).
///
/// Providers fetch tasks from external systems and convert them to
/// the generic `Task` type. The app polls providers periodically.
#[allow(async_fn_in_trait)]
pub trait TaskProvider: Send + Sync {
    /// Provider name (e.g., "github", "linear").
    fn name(&self) -> &str;

    /// Fetch all current tasks. Called once per poll cycle.
    async fn fetch_tasks(&self) -> Result<Vec<Task>, ProviderError>;

    /// The authenticated username, if known.
    fn username(&self) -> Option<&str> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_helpers() {
        let r = ProviderError::retryable("github", "tcp reset");
        assert!(r.is_retryable());
        assert!(!r.is_auth());

        let a = ProviderError::auth("github", "401 Unauthorized");
        assert!(a.is_auth());
        assert!(!a.is_retryable());

        let p = ProviderError::permanent("github", "missing field `repository`");
        assert!(!p.is_retryable());
        assert!(!p.is_auth());
    }

    #[test]
    fn user_message_is_terse_and_diagnostic_is_full() {
        let p = ProviderError::permanent(
            "github",
            "GraphQL: line 1\nstack trace line 2\nstack trace line 3",
        );
        let msg = p.user_message();
        assert!(msg.len() < 80, "user_message stays short: {msg}");
        assert!(p.diagnostic().contains("stack trace line 2"));
    }

    #[test]
    fn display_uses_user_message() {
        let r = ProviderError::retryable("github", "secret detail");
        let s = format!("{r}");
        assert!(!s.contains("secret detail"));
        assert!(s.contains("github"));
        assert!(s.contains("retrying"));
    }
}
