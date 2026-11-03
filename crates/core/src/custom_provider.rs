//! Declarative custom gateway configuration; no vendor identity or executable transforms.
use crate::{ApiFamily, ProviderConfig};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct CustomProviderConfig {
    pub automatic_protocol: bool,
    pub paths: CustomApiPaths,
    pub auth_header: String,
    pub auth_prefix: String,
    /// Generation defaults only. Conversation, tool and transport fields are owned by Xylo.
    pub request_defaults: BTreeMap<String, Value>,
    pub request_defaults_by_api: BTreeMap<String, BTreeMap<String, Value>>,
    /// Explicit metadata for deployments whose catalog omits capabilities.
    pub model_overrides: BTreeMap<String, ModelCapabilities>,
}

impl Default for CustomProviderConfig {
    fn default() -> Self {
        Self {
            automatic_protocol: true,
            paths: CustomApiPaths::default(),
            auth_header: "Authorization".into(),
            auth_prefix: "Bearer ".into(),
            request_defaults: BTreeMap::new(),
            request_defaults_by_api: BTreeMap::new(),
            model_overrides: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CustomApiPaths {
    pub chat_completions: String,
    pub responses: String,
    pub messages: String,
    pub generate_content: String,
    pub stream_generate_content: String,
    pub models: String,
}

impl Default for CustomApiPaths {
    fn default() -> Self {
        Self {
            chat_completions: "chat/completions".into(),
            responses: "responses".into(),
            messages: "v1/messages".into(),
            generate_content: "v1beta/models/{model}:generateContent".into(),
            stream_generate_content: "v1beta/models/{model}:streamGenerateContent?alt=sse".into(),
            models: "models".into(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ModelCapabilities {
    /// None means unknown, Some(empty) means no supported conversational API.
    pub supported_apis: Option<Vec<ApiFamily>>,
    pub native_family: Option<ApiFamily>,
    pub chat_tools_with_reasoning: Option<bool>,
    pub chat_output_limit: Option<ChatOutputLimit>,
    pub max_output_tokens: Option<u32>,
    pub access_restricted: Option<bool>,
    pub adaptive_thinking: Option<bool>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChatOutputLimit {
    MaxTokens,
    MaxCompletionTokens,
}

impl ChatOutputLimit {
    pub fn field(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
        }
    }
}

impl CustomProviderConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            serde_json::to_vec(self)?.len() <= 64 * 1024,
            "Custom configuration exceeds 64 KiB"
        );
        ensure!(
            self.model_overrides.len() <= 2000,
            "Too many model overrides"
        );
        fn validate_defaults(values: &BTreeMap<String, Value>) -> Result<()> {
            for (key, value) in values {
                ensure!(
                    !matches!(
                        key.as_str(),
                        "model"
                            | "messages"
                            | "input"
                            | "instructions"
                            | "system"
                            | "contents"
                            | "tools"
                            | "tool_choice"
                            | "stream"
                            | "stream_options"
                            | "max_tokens"
                            | "max_completion_tokens"
                            | "max_output_tokens"
                            | "reasoning"
                            | "reasoning_effort"
                            | "thinking"
                            | "thinkingConfig"
                            | "output_config"
                    ) && !["auth", "token", "key", "secret", "password", "credential"]
                        .iter()
                        .any(|part| key.to_ascii_lowercase().contains(part)),
                    "Request default {key:?} is managed by Xylo or belongs in the credential vault"
                );
                ensure!(
                    key != "generationConfig" || value.is_object(),
                    "generationConfig defaults must be an object"
                );
                if let Some(nested) = value.as_object() {
                    validate_defaults(
                        &nested.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    )?;
                }
            }
            Ok(())
        }
        validate_defaults(&self.request_defaults)?;
        for (api, defaults) in &self.request_defaults_by_api {
            ensure!(
                matches!(
                    api.as_str(),
                    "open_ai_chat" | "open_ai_responses" | "anthropic" | "gemini"
                ),
                "Unknown API in request_defaults_by_api"
            );
            validate_defaults(defaults)?;
        }
        Ok(())
    }
}

impl ProviderConfig {
    pub fn capabilities(&self) -> Option<&ModelCapabilities> {
        self.custom
            .as_ref()
            .and_then(|c| c.model_overrides.get(&self.model))
            .or(self.model_capabilities.as_ref())
    }

    /// Automatic routing uses advertised protocols only; unknown catalogs retain the chosen API.
    pub fn effective_family(&self) -> ApiFamily {
        if !self.custom.as_ref().is_some_and(|c| c.automatic_protocol) {
            return self.family.clone();
        }
        let routes = &self.custom.as_ref().unwrap().paths;
        let enabled = |family: &ApiFamily| match family {
            ApiFamily::OpenAiChat => !routes.chat_completions.is_empty(),
            ApiFamily::OpenAiResponses => !routes.responses.is_empty(),
            ApiFamily::Anthropic => !routes.messages.is_empty(),
            ApiFamily::Gemini => {
                !routes.generate_content.is_empty() && !routes.stream_generate_content.is_empty()
            }
            _ => false,
        };
        let Some(capabilities) = self.capabilities() else {
            return self.family.clone();
        };
        let Some(apis) = &capabilities.supported_apis else {
            return self.family.clone();
        };
        if let Some(native) = &capabilities.native_family
            && matches!(native, ApiFamily::Anthropic | ApiFamily::Gemini)
            && apis.contains(native)
            && enabled(native)
        {
            return native.clone();
        }
        [
            ApiFamily::OpenAiResponses,
            ApiFamily::Anthropic,
            ApiFamily::Gemini,
            ApiFamily::OpenAiChat,
        ]
        .into_iter()
        .find(|family| apis.contains(family) && enabled(family))
        .unwrap_or_else(|| self.family.clone())
    }
}
