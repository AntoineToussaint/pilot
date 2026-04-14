//! Task provider abstraction. GitHub, Linear, Jira, etc. implement this trait.

use crate::Task;

/// Errors from task providers.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("API error: {0}")]
    Api(String),
    #[error("Authentication error: {0}")]
    Auth(String),
    #[error("{0}")]
    Other(String),
}

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
