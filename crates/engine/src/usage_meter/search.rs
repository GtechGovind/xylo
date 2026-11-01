//! Normalizes provider-native search receipts without inventing search-service fees.
use crate::model::ModelUsage;
use serde_json::Value;

pub(super) fn usage(result: &Value) -> Option<ModelUsage> {
    let value = result.get("usage")?.as_object()?;
    let count = |name: &str| value.get(name).and_then(Value::as_u64);
    if let Some(prompt) = count("promptTokenCount") {
        // Gemini total includes prompt, tool-use prompt, thoughts and candidates.
        // https://ai.google.dev/api/generate-content#UsageMetadata
        return Some(ModelUsage {
            input_tokens: prompt.saturating_add(count("toolUsePromptTokenCount").unwrap_or(0)),
            output_tokens: count("candidatesTokenCount")
                .unwrap_or(0)
                .saturating_add(count("thoughtsTokenCount").unwrap_or(0)),
            cache_read_tokens: count("cachedContentTokenCount"),
            cache_write_tokens: None,
        });
    }
    let mut input = count("input_tokens").or_else(|| count("prompt_tokens"))?;
    let output = count("output_tokens")
        .or_else(|| count("completion_tokens"))
        .unwrap_or(0);
    let anthropic = value.contains_key("cache_read_input_tokens")
        || value.contains_key("cache_creation_input_tokens");
    let (read, write) = if anthropic {
        let read = count("cache_read_input_tokens");
        let write = count("cache_creation_input_tokens");
        input = input
            .saturating_add(read.unwrap_or(0))
            .saturating_add(write.unwrap_or(0));
        (read, write)
    } else {
        (
            value
                .get("input_tokens_details")
                .or_else(|| value.get("prompt_tokens_details"))
                .and_then(|details| details.get("cached_tokens"))
                .and_then(Value::as_u64),
            None,
        )
    };
    Some(ModelUsage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: read,
        cache_write_tokens: write,
    })
}
