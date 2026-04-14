use crate::{Credential, CredentialError, CredentialProvider};

/// A provider that always returns the same token. Useful for testing
/// or when a token is loaded from config at startup.
pub struct StaticProvider {
    credential: Option<Credential>,
}

impl StaticProvider {
    pub fn new(token: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            credential: Some(Credential::new(token, source)),
        }
    }

    /// A provider that always fails. Useful as a sentinel at end of chain.
    pub fn empty() -> Self {
        Self { credential: None }
    }
}

impl CredentialProvider for StaticProvider {
    fn name(&self) -> &str {
        "static"
    }

    async fn resolve(&self, _scope: &str) -> Result<Credential, CredentialError> {
        self.credential
            .clone()
            .ok_or_else(|| CredentialError::NotFound("no static credential configured".into()))
    }
}
