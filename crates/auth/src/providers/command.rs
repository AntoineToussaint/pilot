use crate::{Credential, CredentialError, CredentialProvider};

/// Resolves a credential by running a shell command and reading stdout.
///
/// ```rust,no_run
/// use pilot_auth::CommandProvider;
/// // Use `gh auth token` to get a GitHub token
/// let provider = CommandProvider::new("gh", &["auth", "token"]);
/// // Use `vault read` to get a secret
/// // let provider = CommandProvider::new("vault", &["read", "-field=token", "secret/github"]);
/// ```
pub struct CommandProvider {
    program: String,
    args: Vec<String>,
    /// Label for logging/display.
    label: String,
}

impl CommandProvider {
    pub fn new(program: impl Into<String>, args: &[&str]) -> Self {
        let program = program.into();
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let label = format!("{} {}", program, args.join(" "));
        Self {
            program,
            args,
            label,
        }
    }

    /// Override the display label.
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }
}

impl CredentialProvider for CommandProvider {
    fn name(&self) -> &str {
        "command"
    }

    async fn resolve(&self, _scope: &str) -> Result<Credential, CredentialError> {
        let output = tokio::process::Command::new(&self.program)
            .args(&self.args)
            .output()
            .await
            .map_err(|e| {
                CredentialError::Provider(format!("failed to run `{}`: {e}", self.label))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CredentialError::Provider(format!(
                "`{}` exited with {}: {}",
                self.label,
                output.status,
                stderr.trim()
            )));
        }

        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if token.is_empty() {
            return Err(CredentialError::NotFound(format!(
                "`{}` returned empty output",
                self.label
            )));
        }

        Ok(Credential::new(token, format!("cmd:{}", self.label)))
    }
}
