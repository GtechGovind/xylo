use super::*;

#[test]
fn cached_tokens_are_not_charged_twice_and_overrides_win() {
    let calls = [UsageCall {
        model: "custom".into(),
        input_tokens: 1_000_000,
        output_tokens: 100_000,
        cache_read_tokens: 600_000,
        cache_write_tokens: 100_000,
    }];
    let overrides = BTreeMap::from([(
        "custom".into(),
        ModelPrice {
            input: 2.,
            output: 10.,
            cache_read: 0.2,
            cache_write: 2.5,
        },
    )]);
    let cost = estimate(&calls, &overrides);
    assert!((cost.usd - 1.97).abs() < 1e-9);
    assert_eq!(cost.unpriced_calls, 0);
}

#[test]
fn long_context_tiers_are_chosen_per_request_and_unknown_cost_stays_unknown() {
    let calls = [
        UsageCall {
            model: "gemini-2.5-pro".into(),
            input_tokens: 100_000,
            ..Default::default()
        },
        UsageCall {
            model: "gemini-2.5-pro".into(),
            input_tokens: 300_000,
            ..Default::default()
        },
        UsageCall {
            model: "unpriced-local-model".into(),
            input_tokens: 30,
            ..Default::default()
        },
    ];
    let cost = estimate(&calls, &BTreeMap::new());
    assert!((cost.usd - 0.875).abs() < 1e-9);
    assert_eq!(cost.unpriced_calls, 1);
    assert!(builtin("claude-sonnet-4-6-impostor", 1).is_none());
}

#[test]
fn invalid_overrides_are_not_used_as_prices() {
    let mut rate = ModelPrice {
        input: f64::NAN,
        ..Default::default()
    };
    assert!(!rate.valid());
    rate.input = -1.;
    assert!(!rate.valid());
    let cost = estimate(
        &[UsageCall {
            model: "custom".into(),
            input_tokens: 1,
            ..Default::default()
        }],
        &BTreeMap::from([("custom".into(), rate)]),
    );
    assert_eq!(cost.unpriced_calls, 1);
    assert_eq!(cost.usd, 0.);
}
