//! Token-price estimates in USD, with explicit user overrides for every model.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Standard text-token prices per million tokens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// One successful provider request, retained so context tiers are priced correctly.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCall {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

/// Known token cost and the number of requests whose price is not configured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CostEstimate {
    pub usd: f64,
    pub unpriced_calls: u64,
}

impl ModelPrice {
    /// Rejects non-finite, negative, and unreasonable rate inputs.
    pub fn valid(&self) -> bool {
        [self.input, self.output, self.cache_read, self.cache_write]
            .iter()
            .all(|rate| rate.is_finite() && (0.0..=1_000_000.0).contains(rate))
    }
}

/// Finds a published standard price, never guessing a price for unknown models.
pub fn builtin(model: &str, input_tokens: u64) -> Option<ModelPrice> {
    // Standard text-token rates checked 2026-11-01. Tool fees, cache storage,
    // regional/priority premiums, subscriptions and discounts are excluded.
    // https://developers.openai.com/api/docs/pricing
    // https://platform.claude.com/docs/en/about-claude/pricing
    // https://ai.google.dev/gemini-api/docs/pricing
    let (input, output, cache_read, cache_write) = match model {
        "gpt-6-astra" => (10., 50., 1., 12.5),
        "gpt-6.1-sol" => (2., 10., 0.1, 2.5),
        "gpt-6.1-terra" => (1., 5., 0.05, 1.25),
        "gpt-6-luna" => (0.1, 0.5, 0.01, 0.125),
        "gpt-5.3-codex" => (1.75, 14., 0.175, 1.75),
        "claude-fable-5-1" => (10., 50., 0.25, 12.5),
        "claude-opus-5-5" => (4., 20., 0.2, 5.),
        "claude-sonnet-5-5" => (2., 10., 0.1, 2.5),
        "claude-haiku-5-5" if input_tokens > 100_000 => (0.5, 2.5, 0.05, 0.625),
        "claude-haiku-5-5" => (0.1, 0.5, 0.01, 0.125),
        "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6"
        | "claude-opus-4-5" => (5., 25., 0.5, 6.25),
        "claude-sonnet-5" => (2., 10., 0.2, 2.5),
        "claude-sonnet-4-6" | "claude-sonnet-4-5" => (3., 15., 0.3, 3.75),
        "claude-haiku-4-5" => (1., 5., 0.1, 1.25),
        "gemini-2.5-pro" if input_tokens > 200_000 => (2.5, 15., 0.25, 2.5),
        "gemini-2.5-pro" => (1.25, 10., 0.125, 1.25),
        "gemini-2.5-flash" => (0.3, 2.5, 0.03, 0.3),
        "gemini-2.5-flash-lite" => (0.1, 0.4, 0.01, 0.1),
        _ => return None,
    };
    Some(ModelPrice {
        input,
        output,
        cache_read,
        cache_write,
    })
}

/// Estimates token charges, excluding tool fees, cache storage and negotiated discounts.
pub fn estimate(calls: &[UsageCall], overrides: &BTreeMap<String, ModelPrice>) -> CostEstimate {
    let mut result = CostEstimate::default();
    for call in calls {
        let price = overrides
            .get(&call.model)
            .copied()
            .filter(ModelPrice::valid)
            .or_else(|| builtin(&call.model, call.input_tokens));
        let Some(price) = price else {
            result.unpriced_calls = result.unpriced_calls.saturating_add(1);
            continue;
        };
        let read = call.cache_read_tokens.min(call.input_tokens);
        let write = call.cache_write_tokens.min(call.input_tokens - read);
        let ordinary = call.input_tokens - read - write;
        result.usd += (ordinary as f64 * price.input
            + call.output_tokens as f64 * price.output
            + read as f64 * price.cache_read
            + write as f64 * price.cache_write)
            / 1_000_000.;
    }
    result
}

#[cfg(test)]
mod tests;
