//! Rough USD-per-million-tokens for common models, used for the
//! `estimated_cost_usd` field on `ProxyRecord`. Prices change slowly
//! but do change — update when they drift. Not authoritative; users
//! should check their provider dashboards for billing.

use crate::ApiProvider;

/// (input per MTok, output per MTok) in USD. Returns None for unknown
/// models so we don't lie with a bad estimate.
pub fn rate_per_mtok(provider: ApiProvider, model: &str) -> Option<(f64, f64)> {
    match provider {
        ApiProvider::Anthropic => anthropic(model),
        ApiProvider::OpenAI => openai(model),
        ApiProvider::Unknown => None,
    }
}

fn anthropic(model: &str) -> Option<(f64, f64)> {
    // Prefix match — "claude-sonnet-4-6" and "claude-sonnet-4-6-20251001"
    // should resolve to the same rate.
    if model.starts_with("claude-opus-4") {
        return Some((15.0, 75.0));
    }
    if model.starts_with("claude-sonnet-4") {
        return Some((3.0, 15.0));
    }
    if model.starts_with("claude-haiku-4") {
        return Some((0.80, 4.0));
    }
    None
}

fn openai(model: &str) -> Option<(f64, f64)> {
    if model.starts_with("gpt-4o-mini") {
        return Some((0.15, 0.60));
    }
    if model.starts_with("gpt-4o") {
        return Some((2.50, 10.0));
    }
    if model.starts_with("o1-mini") {
        return Some((3.0, 12.0));
    }
    if model.starts_with("o1") {
        return Some((15.0, 60.0));
    }
    None
}

/// Compute cost in USD from token counts. Returns None if we don't
/// know the rate. Cache pricing is provider-specific; we'll wire it
/// in with the real records.
pub fn estimate_cost(
    provider: ApiProvider,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> Option<f64> {
    let (input_rate, output_rate) = rate_per_mtok(provider, model)?;
    let cost =
        (input_tokens as f64 * input_rate + output_tokens as f64 * output_rate) / 1_000_000.0;
    Some(cost)
}
