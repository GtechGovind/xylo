//! Only documented numeric quota headers are exposed to the application.
use reqwest::header::HeaderMap;
use xylo_core::RateLimits;

pub(super) fn remaining(headers: &HeaderMap) -> Option<RateLimits> {
    let number = |names: &[&str]| {
        names.iter().find_map(|name| {
            headers
                .get(*name)?
                .to_str()
                .ok()?
                .trim()
                .parse::<u64>()
                .ok()
        })
    };
    let limits = RateLimits {
        requests: number(&[
            "x-ratelimit-remaining-requests",
            "anthropic-ratelimit-requests-remaining",
        ]),
        tokens: number(&[
            "x-ratelimit-remaining-tokens",
            "anthropic-ratelimit-tokens-remaining",
        ]),
        input_tokens: number(&["anthropic-ratelimit-input-tokens-remaining"]),
        output_tokens: number(&["anthropic-ratelimit-output-tokens-remaining"]),
        observed_at_ms: xylo_core::now_ms(),
    };
    [
        limits.requests,
        limits.tokens,
        limits.input_tokens,
        limits.output_tokens,
    ]
    .iter()
    .any(Option::is_some)
    .then_some(limits)
}
