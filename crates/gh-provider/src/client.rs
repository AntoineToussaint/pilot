use octocrab::Octocrab;
use pilot_auth::Credential;
use pilot_core::*;

use crate::graphql;

#[derive(Debug, thiserror::Error)]
pub enum GhError {
    #[error("GitHub API error: {0}")]
    Api(#[from] octocrab::Error),
    #[error("GraphQL error: {0}")]
    Graphql(String),
}

#[derive(Clone)]
pub struct GhClient {
    inner: Octocrab,
    user: String,
    credential_source: String,
    filters: Vec<String>,
    watch_repos: Vec<String>,
}

impl GhClient {
    pub async fn from_credential(cred: Credential) -> Result<Self, GhError> {
        let source = cred.source.clone();
        // Disable octocrab's built-in retry: its `OctoBody` clone only
        // Arc-clones a single-use body stream, so on a 429/5xx retry the
        // second attempt goes out with an empty `{}` body. GitHub answers
        // with the infamous "A query attribute must be specified and must
        // be a string" — ~1 in every 5 GraphQL polls during rate-limited
        // periods. We eat the retry feature; polling runs every few seconds
        // so we just try again on the next tick.
        let inner = Octocrab::builder()
            .personal_token(cred.into_token())
            .add_retry_config(octocrab::service::middleware::retry::RetryConfig::None)
            .build()
            .map_err(GhError::Api)?;
        let user = inner.current().user().await.map_err(GhError::Api)?.login;
        Ok(Self {
            inner,
            user,
            credential_source: source,
            filters: vec![],
            watch_repos: vec![],
        })
    }

    pub fn with_filters(mut self, filters: Vec<String>) -> Self {
        self.filters = filters;
        self
    }

    pub fn with_watch_repos(mut self, repos: Vec<String>) -> Self {
        self.watch_repos = repos;
        self
    }

    pub fn with_needs_reply(self, _enabled: bool) -> Self {
        self
    }

    pub fn username(&self) -> &str {
        &self.user
    }

    pub fn credential_source(&self) -> &str {
        &self.credential_source
    }

    /// Fetch ALL relevant PRs in a single GraphQL query.
    /// `involves:username` covers author, reviewer, assignee, mentioned.
    /// **One API call instead of 68.**
    pub fn authenticated_user(&self) -> &str {
        &self.user
    }

    pub async fn fetch_all_prs(&self) -> Result<Vec<Task>, GhError> {
        let search_query = graphql::build_query(&self.user, &self.filters);

        tracing::info!("GraphQL search: {search_query}");

        // Paginate until GitHub reports no more pages. Without this, users
        // with >100 inbox PRs lose the tail on first poll, which the stale
        // purge then deletes from SQLite — PRs "disappear" after restart.
        let mut tasks: Vec<Task> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut page = 0usize;
        loop {
            let body = graphql::query_body_after(&search_query, cursor.as_deref());
            tracing::debug!(
                "GraphQL page {page} body: {}",
                serde_json::to_string(&body).unwrap_or_default()
            );

            // Fetch the raw JSON first so that on error we can dump the
            // complete response body (not just the serde-parsed message) to
            // `/tmp/pilot.log` — invaluable when debugging queries GitHub
            // rejects with terse messages like "A query attribute must be
            // specified and must be a string".
            let raw: serde_json::Value =
                self.inner
                    .post("/graphql", Some(&body))
                    .await
                    .map_err(|e| {
                        tracing::error!("GraphQL HTTP error (page {page}): {e}");
                        tracing::error!(
                            "GraphQL request body was: {}",
                            serde_json::to_string_pretty(&body).unwrap_or_default()
                        );
                        GhError::Api(e)
                    })?;

            let response: graphql::GqlResponse = match serde_json::from_value(raw.clone()) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(
                        "GraphQL response did not match schema (page {page}): {e}\n\
                         Full response body:\n{}",
                        serde_json::to_string_pretty(&raw).unwrap_or_default()
                    );
                    return Err(GhError::Graphql(format!(
                        "response schema mismatch (page {page}): {e}"
                    )));
                }
            };

            if let Some(errors) = &response.errors {
                let detailed: Vec<_> = errors.iter().map(|e| e.full()).collect();
                let joined = detailed.join("; ");
                tracing::error!("GraphQL errors (search page {page}): {joined}");
                tracing::error!("GraphQL search was: {search_query}");
                tracing::error!(
                    "GraphQL request body:\n{}",
                    serde_json::to_string_pretty(&body).unwrap_or_default()
                );
                tracing::error!(
                    "GraphQL full response body:\n{}",
                    serde_json::to_string_pretty(&raw).unwrap_or_default()
                );
                return Err(GhError::Graphql(format!(
                    "search `{search_query}` (page {page}): {joined}"
                )));
            }

            let data = response
                .data
                .ok_or_else(|| GhError::Graphql("No data in response".into()))?;

            if let Some(rl) = &data.rate_limit {
                tracing::info!(
                    "GitHub rate limit: {}/5000 remaining, resets {}",
                    rl.remaining,
                    rl.reset_at
                );
            }

            tasks.extend(
                data.search
                    .nodes
                    .iter()
                    .map(|pr| graphql::pr_to_task(pr, &self.user)),
            );

            let page_info = data.search.page_info.unwrap_or_default();
            if !page_info.has_next_page {
                break;
            }
            cursor = page_info.end_cursor;
            if cursor.is_none() {
                // Defensive: hasNextPage=true but no cursor. Bail out rather
                // than looping forever.
                tracing::warn!("GraphQL paged: hasNextPage=true but endCursor=null");
                break;
            }
            page += 1;
            if page >= 20 {
                tracing::warn!("GraphQL paged: bailing after {page} pages (safety cap)");
                break;
            }
        }

        // Fetch watched repos (all open PRs, not just involves:user).
        for repo in &self.watch_repos {
            let watch_query = format!("is:open is:pr repo:{repo} archived:false");
            let watch_body = graphql::query_body(&watch_query);
            tracing::debug!("Watch query for {repo}: {watch_query}");

            match self
                .inner
                .post::<_, graphql::GqlResponse>("/graphql", Some(&watch_body))
                .await
            {
                Ok(resp) => {
                    if let Some(data) = resp.data {
                        let existing_keys: std::collections::HashSet<String> =
                            tasks.iter().map(|t| t.id.key.clone()).collect();
                        for pr in &data.search.nodes {
                            let task = graphql::pr_to_task(pr, &self.user);
                            if !existing_keys.contains(&task.id.key) {
                                tasks.push(task);
                            }
                        }
                    }
                    if let Some(errors) = resp.errors {
                        let detailed: Vec<_> = errors.iter().map(|e| e.full()).collect();
                        tracing::warn!("Watch query errors for {repo}: {}", detailed.join("; "));
                    }
                }
                Err(e) => {
                    tracing::warn!("Watch query failed for {repo}: {e}");
                }
            }
        }

        tracing::info!(
            "GraphQL returned {} PRs (incl. {} watched repos)",
            tasks.len(),
            self.watch_repos.len()
        );
        Ok(tasks)
    }

    /// Fetch all open GitHub Issues involving the authenticated user,
    /// paginated. Separate from `fetch_all_prs` so callers opt in
    /// explicitly — v1 doesn't call this; v2 does.
    pub async fn fetch_all_issues(&self) -> Result<Vec<Task>, GhError> {
        let search_query = graphql::build_issues_query(&self.user, &self.filters);
        tracing::info!("GraphQL issues search: {search_query}");

        let mut tasks: Vec<Task> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut page = 0usize;
        loop {
            let body = graphql::issues_query_body(&search_query, cursor.as_deref());
            let response: graphql::GqlIssueResponse = self
                .inner
                .post("/graphql", Some(&body))
                .await
                .map_err(|e| {
                    tracing::error!("Issues HTTP error (page {page}): {e}");
                    GhError::Api(e)
                })?;

            if let Some(errors) = &response.errors {
                let joined = errors
                    .iter()
                    .map(|e| e.full())
                    .collect::<Vec<_>>()
                    .join("; ");
                tracing::error!("Issues GraphQL errors (page {page}): {joined}");
                return Err(GhError::Graphql(joined));
            }

            let data = response
                .data
                .ok_or_else(|| GhError::Graphql("No data in response".into()))?;

            if let Some(rl) = &data.rate_limit {
                tracing::debug!(
                    "GitHub rate limit after issues: {}/5000 remaining",
                    rl.remaining
                );
            }

            tasks.extend(
                data.search
                    .nodes
                    .iter()
                    .map(|issue| graphql::issue_to_task(issue, &self.user)),
            );

            let page_info = data.search.page_info.unwrap_or_default();
            if !page_info.has_next_page {
                break;
            }
            cursor = page_info.end_cursor;
            if cursor.is_none() {
                tracing::warn!("Issues paged: hasNextPage=true but endCursor=null");
                break;
            }
            page += 1;
            if page >= 20 {
                tracing::warn!("Issues paged: bailing after {page} pages");
                break;
            }
        }

        tracing::info!("GraphQL returned {} issues", tasks.len());
        Ok(tasks)
    }

    /// Fetch PRs + Issues in parallel, combine into one `Vec<Task>`.
    /// Errors from either side surface; we prefer a partial result
    /// over a hard failure — the TUI degrades gracefully if one
    /// source is down.
    pub async fn fetch_all(&self) -> Result<Vec<Task>, GhError> {
        let (prs, issues) = tokio::join!(self.fetch_all_prs(), self.fetch_all_issues());
        let mut tasks = Vec::new();
        match prs {
            Ok(v) => tasks.extend(v),
            Err(e) => tracing::warn!("PRs fetch failed: {e}"),
        }
        match issues {
            Ok(v) => tasks.extend(v),
            Err(e) => tracing::warn!("Issues fetch failed: {e}"),
        }
        if tasks.is_empty() {
            return Err(GhError::Graphql("both PR and issue fetches failed".into()));
        }
        Ok(tasks)
    }

    /// Merge the base branch into this PR's head — same as the "Update
    /// branch" button on github.com. Requires the PR's GraphQL node ID.
    pub async fn update_branch(&self, pull_request_node_id: &str) -> Result<(), GhError> {
        let body = graphql::update_branch_body(pull_request_node_id);
        let response: graphql::GqlResponse = self
            .inner
            .post("/graphql", Some(&body))
            .await
            .map_err(GhError::Api)?;
        if let Some(errors) = response.errors {
            let joined = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            tracing::error!("updatePullRequestBranch errors: {joined}");
            return Err(GhError::Graphql(joined));
        }
        Ok(())
    }
}

impl pilot_core::TaskProvider for GhClient {
    fn name(&self) -> &str {
        "github"
    }

    async fn fetch_tasks(&self) -> Result<Vec<pilot_core::Task>, pilot_core::ProviderError> {
        self.fetch_all_prs()
            .await
            .map_err(|e| pilot_core::ProviderError::Api(e.to_string()))
    }

    fn username(&self) -> Option<&str> {
        Some(&self.user)
    }
}
