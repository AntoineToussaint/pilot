//! # pilot-config
//!
//! YAML-based configuration for pilot. Loads from `~/.pilot/config.yaml`
//! with sensible defaults if the file is missing.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config: {0}")]
    Parse(#[from] serde_yaml::Error),
}

/// Top-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub providers: ProvidersConfig,
    pub display: DisplayConfig,
    pub slack: SlackConfig,
    pub agent: AgentSection,
    pub shell: ShellSection,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            providers: ProvidersConfig::default(),
            display: DisplayConfig::default(),
            slack: SlackConfig::default(),
            agent: AgentSection::default(),
            shell: ShellSection::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSection {
    #[serde(flatten)]
    pub config: pilot_core::AgentConfig,
}

impl Default for AgentSection {
    fn default() -> Self {
        Self {
            config: pilot_core::AgentConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellSection {
    pub command: String,
}

impl Default for ShellSection {
    fn default() -> Self {
        Self {
            command: "bash".into(),
        }
    }
}

impl Config {
    /// Load from `~/.pilot/config.yaml`, falling back to defaults.
    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::default_path();
        if path.exists() {
            Self::load_from(&path)
        } else {
            tracing::info!("No config file at {}, using defaults", path.display());
            Ok(Self::default())
        }
    }

    /// Load from a specific path.
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = serde_yaml::from_str(&contents)?;
        tracing::info!("Loaded config from {}", path.display());
        Ok(config)
    }

    /// Write a default config file (for first-run).
    pub fn write_default(path: &Path) -> Result<(), ConfigError> {
        let config = Self::default();
        let yaml = serde_yaml::to_string(&config)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, yaml)?;
        Ok(())
    }

    pub fn default_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".pilot").join("config.yaml")
    }
}

// ─── Provider configs ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProvidersConfig {
    pub github: GithubConfig,
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        Self {
            github: GithubConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GithubConfig {
    /// Poll interval in seconds.
    #[serde(with = "duration_secs")]
    pub poll_interval: Duration,
    /// Org/repo filters. Only PRs matching these appear in the inbox.
    /// Empty = show everything.
    pub filters: Vec<Filter>,
    /// Whether to fetch comment authors for needs-reply detection.
    pub detect_needs_reply: bool,
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(30),
            filters: vec![],
            detect_needs_reply: true,
        }
    }
}

/// A filter for narrowing which tasks to show.
///
/// YAML format:
/// ```yaml
/// filters:
///   - org: tensorzero
///   - repo: owner/name
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Filter {
    /// Filter to a GitHub organization (only PRs involving you).
    #[serde(default)]
    pub org: Option<String>,
    /// Filter to a specific repo (only PRs involving you).
    #[serde(default)]
    pub repo: Option<String>,
    /// Watch ALL open PRs in this repo (regardless of involvement).
    #[serde(default)]
    pub watch: Option<String>,
}

impl Filter {
    /// Convert to a GitHub search query qualifier for the "involves" query.
    pub fn to_search_qualifier(&self) -> Option<String> {
        if let Some(org) = &self.org {
            Some(format!("org:{org}"))
        } else if let Some(repo) = &self.repo {
            Some(format!("repo:{repo}"))
        } else {
            None
        }
    }

    /// If this is a "watch" filter, return the repo to watch.
    pub fn watch_repo(&self) -> Option<&str> {
        self.watch.as_deref()
    }
}

// ─── Display config ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayConfig {
    pub sort_by: SortMode,
    pub show_archived: bool,
    /// Only show sessions with activity within this many days.
    /// 0 = show all. Default: 7.
    pub activity_days: u32,
    /// Hide PRs you've already approved (you've done your part).
    pub hide_approved_by_me: bool,
    /// Treat assignees as reviewers (some teams use assignees for review tracking).
    pub assignee_is_reviewer: bool,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            sort_by: SortMode::Priority,
            show_archived: false,
            activity_days: 7,
            hide_approved_by_me: true,
            assignee_is_reviewer: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortMode {
    Priority,
    Updated,
}

// ─── Slack config ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SlackConfig {
    /// Slack incoming webhook URL for sending messages.
    pub webhook_url: Option<String>,
}

impl Default for SlackConfig {
    fn default() -> Self {
        Self { webhook_url: None }
    }
}

// ─── Serde helper for Duration as seconds ──────────────────────────────────

mod duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{}s", d.as_secs()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let s = String::deserialize(d)?;
        let s = s.trim_end_matches('s');
        let secs: u64 = s.parse().map_err(serde::de::Error::custom)?;
        Ok(Duration::from_secs(secs))
    }
}
