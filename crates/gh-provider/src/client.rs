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
        let inner = Octocrab::builder()
            .personal_token(cred.into_token())
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
        let body = graphql::query_body(&search_query);

        tracing::info!("GraphQL search: {search_query}");
        tracing::debug!("GraphQL body: {}", serde_json::to_string(&body).unwrap_or_default());

        let response: graphql::GqlResponse = self
            .inner
            .post("/graphql", Some(&body))
            .await
            .map_err(|e| {
                tracing::error!("GraphQL HTTP error: {e}");
                GhError::Api(e)
            })?;

        if let Some(errors) = &response.errors {
            let msgs: Vec<_> = errors.iter().map(|e| e.message.as_str()).collect();
            let joined = msgs.join("; ");
            tracing::error!("GraphQL errors: {joined}");
            tracing::error!("GraphQL search was: {search_query}");
            tracing::error!("GraphQL body was: {}", serde_json::to_string(&body).unwrap_or_default());
            return Err(GhError::Graphql(joined));
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

        let mut tasks: Vec<Task> = data
            .search
            .nodes
            .iter()
            .map(|pr| graphql::pr_to_task(pr, &self.user))
            .collect();

        // Fetch watched repos (all open PRs, not just involves:user).
        for repo in &self.watch_repos {
            let watch_query = format!("is:open is:pr repo:{repo} archived:false");
            let watch_body = graphql::query_body(&watch_query);
            tracing::debug!("Watch query for {repo}: {watch_query}");

            match self.inner.post::<_, graphql::GqlResponse>("/graphql", Some(&watch_body)).await {
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
                        let msgs: Vec<_> = errors.iter().map(|e| e.message.as_str()).collect();
                        tracing::warn!("Watch query errors for {repo}: {}", msgs.join("; "));
                    }
                }
                Err(e) => {
                    tracing::warn!("Watch query failed for {repo}: {e}");
                }
            }
        }

        tracing::info!("GraphQL returned {} PRs (incl. {} watched repos)", tasks.len(), self.watch_repos.len());
        Ok(tasks)
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
