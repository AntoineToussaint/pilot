//! Linear provider — fetches Linear issues as source-agnostic `Task`s.
//!
//! Plugs into the same `TaskProvider` trait the GitHub provider
//! implements, so the TUI treats GitHub PRs, GitHub Issues, and Linear
//! tickets identically in the sidebar.
//!
//! ## Auth
//!
//! Reads the Linear personal API key from the `LINEAR_API_KEY`
//! environment variable. Linear's preferred auth is a bearer token in
//! `Authorization`; we send it without the `Bearer ` prefix per
//! Linear's docs.
//!
//! ## Scope
//!
//! Fetches issues the authenticated user is assigned to or created.
//! States `completed` / `canceled` are filtered out server-side.
//! Pagination support: up to 50 issues per page, up to 20 pages.

pub mod graphql;

use pilot_core::{ProviderError, Task, TaskProvider};
use serde::Serialize;

const LINEAR_GRAPHQL: &str = "https://api.linear.app/graphql";

#[derive(Debug, thiserror::Error)]
pub enum LinearError {
    #[error("missing LINEAR_API_KEY env var")]
    MissingKey,
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("graphql: {0}")]
    Graphql(String),
}

impl From<LinearError> for ProviderError {
    fn from(err: LinearError) -> Self {
        const SOURCE: &str = "linear";
        match &err {
            LinearError::MissingKey => ProviderError::auth(SOURCE, err.to_string()),
            LinearError::Http(_) => {
                let s = err.to_string().to_lowercase();
                if s.contains("401") || s.contains("403") || s.contains("unauthorized") {
                    ProviderError::auth(SOURCE, err.to_string())
                } else if s.contains("timeout")
                    || s.contains("connection")
                    || s.contains("network")
                    || s.contains("502")
                    || s.contains("503")
                    || s.contains("504")
                {
                    ProviderError::retryable(SOURCE, err.to_string())
                } else {
                    ProviderError::permanent(SOURCE, err.to_string())
                }
            }
            LinearError::Graphql(_) => {
                let s = err.to_string().to_lowercase();
                if s.contains("rate limit") || s.contains("temporarily") {
                    ProviderError::retryable(SOURCE, err.to_string())
                } else if s.contains("authentication") || s.contains("unauthorized") {
                    ProviderError::auth(SOURCE, err.to_string())
                } else {
                    ProviderError::permanent(SOURCE, err.to_string())
                }
            }
        }
    }
}

/// Client for Linear's GraphQL API.
#[derive(Clone)]
pub struct LinearClient {
    http: reqwest::Client,
    api_key: String,
    endpoint: String,
}

impl LinearClient {
    /// Build a client from the `LINEAR_API_KEY` env var. Fails if the
    /// env var isn't set.
    pub fn from_env() -> Result<Self, LinearError> {
        let key = std::env::var("LINEAR_API_KEY").map_err(|_| LinearError::MissingKey)?;
        Ok(Self::with_key(key))
    }

    /// Build a client with an explicit API key.
    pub fn with_key(api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.into(),
            endpoint: LINEAR_GRAPHQL.to_string(),
        }
    }

    /// Override the GraphQL endpoint. Used by tests to point at a
    /// local mock server.
    pub fn with_endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint = url.into();
        self
    }

    async fn graphql<T: serde::de::DeserializeOwned>(
        &self,
        body: impl Serialize,
    ) -> Result<T, LinearError> {
        let resp = self
            .http
            .post(&self.endpoint)
            .header("authorization", &self.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        let text = resp.text().await?;
        serde_json::from_str::<T>(&text).map_err(|e| {
            LinearError::Graphql(format!(
                "parse: {e}; body starts with {:?}",
                &text[..text.len().min(200)]
            ))
        })
    }

    /// Fetch all open issues for the authenticated viewer (assigned or
    /// created). Paginates. Results are converted to `Task`s.
    pub async fn fetch_all(&self) -> Result<Vec<Task>, LinearError> {
        // 1. Identify the viewer so we can assign TaskRole correctly.
        let viewer_body = serde_json::json!({
            "query": graphql::VIEWER_QUERY,
        });
        let viewer: graphql::ViewerResponse = self.graphql(&viewer_body).await?;
        let viewer_id = viewer
            .data
            .ok_or_else(|| LinearError::Graphql("no viewer data".into()))?
            .viewer
            .id;

        // 2. Page through issues.
        let mut tasks = Vec::new();
        let mut cursor: Option<String> = None;
        let mut page = 0usize;
        loop {
            let body = graphql::build_issues_body(cursor.as_deref());
            let resp: graphql::IssuesResponse = self.graphql(&body).await?;
            if let Some(errors) = resp.errors {
                let joined = errors
                    .iter()
                    .map(|e| e.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(LinearError::Graphql(joined));
            }
            let data = resp
                .data
                .ok_or_else(|| LinearError::Graphql("no data in issues response".into()))?;
            for issue in &data.issues.nodes {
                tasks.push(graphql::issue_to_task(issue, &viewer_id));
            }
            let page_info = data.issues.page_info;
            if !page_info.has_next_page {
                break;
            }
            cursor = page_info.end_cursor;
            if cursor.is_none() {
                break;
            }
            page += 1;
            if page >= 20 {
                tracing::warn!("Linear paged: bailing after {page} pages");
                break;
            }
        }
        Ok(tasks)
    }
}

impl TaskProvider for LinearClient {
    fn name(&self) -> &str {
        "linear"
    }

    async fn fetch_tasks(&self) -> Result<Vec<Task>, ProviderError> {
        self.fetch_all().await.map_err(Into::into)
    }

    fn username(&self) -> Option<&str> {
        None
    }
}
