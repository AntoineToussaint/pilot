//! LLM proxy — transparent pass-through that records structured
//! telemetry from agent API traffic.
//!
//! **Rationale.** Scraping PTY frames for "what did the agent just do"
//! works for coarse state (working vs asking) but falls apart for
//! anything structured: tokens, tool calls, cost, assistant text for
//! search. The agent is already speaking a well-defined JSON protocol
//! to Anthropic / OpenAI. Proxying that protocol gives us reliable,
//! structured data for free.
//!
//! **Boundaries.** The proxy is deliberately read-only. It forwards
//! requests byte-for-byte (modulo adding our session tag header) and
//! forwards responses byte-for-byte. It never rewrites model names,
//! never swaps providers, never modifies tool results. Observability
//! only — so users can trust the agent is doing exactly what it
//! claims.
//!
//! **Trust.** Binds `127.0.0.1:<ephemeral>` only. Never accepts remote
//! connections. User's API key lives in the agent's env and hits the
//! proxy in the `Authorization` header; the proxy forwards it verbatim
//! and doesn't log auth headers.
//!
//! Skeleton only — the actual hyper server lands in Week 3.

use chrono::{DateTime, Utc};
use pilot_core::SessionKey;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub mod pricing;

/// Which upstream API this record corresponds to. Extensible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiProvider {
    Anthropic,
    OpenAI,
    /// Fallback for hosts pilot doesn't recognize. We still forward the
    /// request and record coarse metadata (url, status, duration) but
    /// can't parse provider-specific token counts.
    Unknown,
}

/// One request/response pair through the proxy. Written to SQLite by
/// the daemon and surfaced to the TUI as part of the session dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyRecord {
    pub session_key: SessionKey,
    pub started_at: DateTime<Utc>,
    #[serde(with = "humantime_serde")]
    pub duration: Duration,
    pub provider: ApiProvider,
    pub endpoint: String,
    pub request_model: Option<String>,
    pub tokens_input: Option<u64>,
    pub tokens_output: Option<u64>,
    pub tokens_cache_read: Option<u64>,
    pub tokens_cache_create: Option<u64>,
    pub estimated_cost_usd: Option<f64>,
    pub tool_calls: Vec<ToolCall>,
    /// Full assistant text if `record_bodies` is enabled. Populates
    /// session search ("find where I asked about the config migration").
    pub assistant_text: Option<String>,
    pub status: u16,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    /// Short, truncated, human-readable. Not the raw JSON — we keep
    /// that off the hot path of serialization.
    pub args_summary: String,
    pub result_size_bytes: Option<u64>,
    #[serde(with = "humantime_serde", default)]
    pub duration: Option<Duration>,
}

/// Per-spawn proxy configuration handed to the agent via env vars.
///
/// When the daemon spawns a wrapped agent command, it allocates one of
/// these, binds a fresh port if needed, and sets ANTHROPIC_BASE_URL /
/// OPENAI_BASE_URL in the child env. Each terminal gets its own
/// `session_tag`; the proxy uses that to attribute requests to the
/// right pilot session without exposing pilot internals to the agent.
#[derive(Debug, Clone)]
pub struct ProxyCtx {
    pub anthropic_url: Option<String>,
    pub openai_url: Option<String>,
    /// Header value the proxy recognizes. The daemon injects it as
    /// `X-Pilot-Session: <tag>` into every agent request (via the
    /// upstream envvar trick when possible, or a header-rewriting
    /// layer if needed).
    pub session_tag: String,
}

/// Shared configuration for the daemon's proxy server. One proxy
/// instance can serve many agents (keyed by `session_tag`).
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub listen: std::net::SocketAddr,
    /// Record raw assistant text + tool arg/result payloads. True by
    /// default; flip off for privacy-sensitive workflows.
    pub record_bodies: bool,
    /// Header names (lowercased) stripped from stored records.
    pub redact_headers: Vec<String>,
    /// JSON paths stripped from stored records. Simple dotted paths;
    /// not a full JSONPath engine (we don't need one).
    pub redact_json_paths: Vec<String>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:0".parse().expect("static addr parses"),
            record_bodies: true,
            redact_headers: vec![
                "authorization".into(),
                "x-api-key".into(),
                "cookie".into(),
            ],
            redact_json_paths: vec![],
        }
    }
}

// Placeholder. Real hyper server + parsers + upstream forwarding land
// in Week 3 of the rewrite. The types above are already referenceable
// from the daemon so proxy records can be wired into IPC first.
pub struct ProxyServer {
    _config: ProxyConfig,
}

impl ProxyServer {
    pub fn new(config: ProxyConfig) -> Self {
        Self { _config: config }
    }
}
