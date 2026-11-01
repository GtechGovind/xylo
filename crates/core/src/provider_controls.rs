//! Provider-native reasoning capabilities and turn defaults.
use crate::{ApiFamily, AppSettings, ProviderConfig};

impl ProviderConfig {
    /// Whether the Claude model accepts adaptive thinking and effort.
    pub fn adaptive_thinking(&self) -> bool {
        let model = self.model.replace('.', "-").to_ascii_lowercase();
        matches!(self.family, ApiFamily::Anthropic)
            && [
                "claude-opus-4-6",
                "claude-sonnet-4-6",
                "claude-opus-4-7",
                "claude-opus-4-8",
                "claude-opus-5",
                "claude-sonnet-5",
            ]
            .iter()
            .any(|prefix| model.starts_with(prefix))
    }

    /// Native protocol choices when a catalog does not advertise them.
    pub(crate) fn native_reasoning_efforts(&self) -> Vec<&str> {
        let model = self.model.to_ascii_lowercase();
        match self.family {
            ApiFamily::Anthropic if self.adaptive_thinking() => {
                vec!["low", "medium", "high", "max"]
            }
            ApiFamily::Anthropic
                if model.starts_with("claude-opus-4-5") || model.starts_with("claude-opus-4.5") =>
            {
                vec!["low", "medium", "high"]
            }
            ApiFamily::Anthropic
                if model.starts_with("claude-sonnet-4")
                    || model.starts_with("claude-opus-4")
                    || model.starts_with("claude-haiku-4-5")
                    || model.starts_with("claude-3-7-sonnet") =>
            {
                vec!["low", "medium", "high"]
            }
            ApiFamily::Gemini | ApiFamily::Vertex if model.starts_with("gemini-3") => {
                vec!["low", "high"]
            }
            ApiFamily::Gemini | ApiFamily::Vertex if model.starts_with("gemini-2.5") => {
                vec!["low", "medium", "high"]
            }
            _ => Vec::new(),
        }
    }
}

impl AppSettings {
    /// Resolves the global fallback without changing the saved model choice.
    pub fn turn_provider(&self) -> ProviderConfig {
        let mut provider = self.provider.clone();
        if provider.fallback_model.is_none() {
            provider.fallback_model.clone_from(&self.fallback_model);
        }
        provider
    }
}
