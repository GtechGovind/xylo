//! Provider-native reasoning capabilities and turn defaults.
use crate::{ApiFamily, AppSettings, ProviderConfig};

impl ProviderConfig {
    /// Whether the Claude model accepts adaptive thinking and effort.
    pub fn adaptive_thinking(&self) -> bool {
        self.default_adaptive_thinking()
            || self.anthropic_model(&[
                "claude-opus-4-6",
                "claude-sonnet-4-6",
                "claude-opus-4-7",
                "claude-opus-4-8",
            ])
    }

    /// Whether omitting thinking controls still selects adaptive thinking.
    /// Older adaptive-capable models default off and must not be enabled just
    /// to attach replay controls.
    /// https://platform.claude.com/docs/en/build-with-claude/thinking-troubleshooting
    pub fn default_adaptive_thinking(&self) -> bool {
        self.anthropic_model(&[
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-haiku-5-5",
            "claude-fable-5",
            "claude-mythos-5",
            "claude-mythos-preview",
        ])
    }

    fn anthropic_model(&self, prefixes: &[&str]) -> bool {
        let model = self.model.replace('.', "-").to_ascii_lowercase();
        matches!(self.family, ApiFamily::Anthropic)
            && prefixes.iter().any(|prefix| {
                model
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('-'))
            })
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
