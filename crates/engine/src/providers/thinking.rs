//! Capability-checked native thinking request controls.
use crate::model::Reasoning;
use serde_json::{Value, json};
use xylo_core::{ApiFamily, ProviderConfig};

pub(super) fn apply(
    config: &ProviderConfig,
    reasoning: Reasoning,
    max_tokens: u32,
    body: &mut Value,
) {
    let options = config.supported_reasoning_efforts();
    if options.is_empty() {
        return;
    }
    const ORDER: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];
    let default = if options.contains(&"medium") {
        "medium"
    } else if options.contains(&"high") {
        "high"
    } else {
        options[0]
    };
    let effort = match reasoning {
        Reasoning::Lowest => options
            .iter()
            .copied()
            .min_by_key(|effort| {
                ORDER
                    .iter()
                    .position(|known| known == effort)
                    .unwrap_or(ORDER.len())
            })
            .unwrap_or(default),
        Reasoning::Configured => config
            .effective_reasoning_effort()
            .or(config
                .default_reasoning_effort
                .as_deref()
                .filter(|value| options.contains(value)))
            .unwrap_or(default),
    };
    let budget = match effort {
        "none" => 0,
        "minimal" | "low" => 1024,
        "medium" => 8192,
        "high" => 16384,
        _ => 32768,
    };
    match config.family {
        ApiFamily::Anthropic => {
            if config.adaptive_thinking() {
                body["thinking"] = json!({"type":"adaptive"});
                body["output_config"] = json!({"effort":effort});
            } else if max_tokens > 1024 && budget > 0 {
                body["thinking"] =
                    json!({"type":"enabled","budget_tokens":budget.min(max_tokens - 1)});
                if config.model.contains("opus-4-5") || config.model.contains("opus-4.5") {
                    body["output_config"] = json!({"effort":effort});
                }
            }
        }
        ApiFamily::Gemini | ApiFamily::Vertex => {
            body["generationConfig"]["thinkingConfig"] = if config.model.starts_with("gemini-3") {
                json!({"thinkingLevel":if matches!(effort, "none" | "minimal" | "low") { "low" } else { "high" },"includeThoughts":true})
            } else {
                // Pro's protocol minimum still applies to short utility requests.
                let minimum = if config.model.contains("pro") { 128 } else { 0 };
                let maximum = if config.model.contains("flash") {
                    24576
                } else {
                    32768
                };
                let budget = budget
                    .min(max_tokens.saturating_sub(1))
                    .clamp(minimum, maximum);
                json!({"thinkingBudget":budget,"includeThoughts":true})
            };
        }
        _ => {}
    }
}
