//! Native HTTP protocol adapters. No provider selection starts a subprocess, reads
//! another application's auth cache, or silently changes to another API family.
mod diagnostics;
mod request;
mod stream;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use futures_util::{StreamExt, stream::BoxStream};
use hermes_core::{AgentError, LlmProvider, LlmResponse, Message, StreamChunk, ToolSchema};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use xylo_core::{ApiFamily, ProviderConfig};
use xylo_services::providers::{
    NormalizedProvider, ProviderCredential, ProviderProtocol, normalize_provider,
};

pub(crate) fn build(
    config: &ProviderConfig,
    secret: Option<String>,
) -> Result<Arc<dyn LlmProvider>> {
    let credential = ProviderCredential::parse(secret)?;
    if matches!(config.family, ApiFamily::Bedrock) {
        let normalized = normalize_provider(config)?;
        let mut provider = hermes_agent::BedrockProvider::new()
            .with_model(&config.model)
            .with_base_url(normalized.base_url.as_str())
            .with_request_timeout_seconds(config.timeout_seconds);
        if let Some(region) = normalized.aws_region {
            provider = provider.with_region(region);
        }
        provider = match credential {
            ProviderCredential::ApiKey(token) | ProviderCredential::Bearer(token) => {
                provider.with_bearer_token(token)
            }
            ProviderCredential::Aws {
                access_key_id,
                secret_access_key,
                session_token,
            } => provider.with_aws_credentials(hermes_agent::bedrock::AwsCredentials {
                access_key_id,
                secret_access_key,
                session_token,
            }),
            ProviderCredential::None => provider,
            ProviderCredential::ChatGpt { .. } => {
                anyhow::bail!("ChatGPT credentials can only be used with the public Responses API")
            }
        };
        return Ok(Arc::new(provider));
    }
    Ok(Arc::new(NativeProvider::new(config.clone(), credential)?))
}

#[derive(Clone)]
struct NativeProvider {
    config: ProviderConfig,
    endpoint: NormalizedProvider,
    client: reqwest::Client,
    chatgpt_plan: bool,
    error_redactions: Arc<Vec<String>>,
}
impl NativeProvider {
    fn new(config: ProviderConfig, credential: ProviderCredential) -> Result<Self> {
        let endpoint = normalize_provider(&config)?;
        let chatgpt_plan = matches!(credential, ProviderCredential::ChatGpt { .. });
        if chatgpt_plan {
            ensure!(
                endpoint.protocol == ProviderProtocol::Responses
                    && endpoint.base_url.as_str().trim_end_matches('/')
                        == "https://api.openai.com/v1",
                "ChatGPT plan credentials require the official public Responses API endpoint"
            );
        }
        let local = matches!(
            endpoint.base_url.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]")
        );
        ensure!(
            local || !matches!(credential, ProviderCredential::None),
            "Save a provider credential in the OS credential store before connecting"
        );
        ensure!(
            !matches!(credential, ProviderCredential::Aws { .. }),
            "AWS credentials require the Bedrock API family"
        );
        let headers = credential.headers(&config.family, &config.headers)?;
        let mut error_redactions = Vec::new();
        for value in headers.values().filter_map(|value| value.to_str().ok()) {
            if !value.is_empty() {
                error_redactions.push(value.to_owned());
            }
            if let Some((scheme, secret)) = value.split_once(' ') {
                if scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("basic") {
                    if !secret.is_empty() {
                        error_redactions.push(secret.to_owned());
                    }
                }
            }
        }
        error_redactions.sort_by_key(|value| std::cmp::Reverse(value.len()));
        error_redactions.dedup();
        let error_redactions = Arc::new(error_redactions);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(config.timeout_seconds))
            .build()
            .context("Create provider HTTP client")?;
        Ok(Self {
            config,
            endpoint,
            client,
            chatgpt_plan,
            error_redactions,
        })
    }
    #[allow(clippy::too_many_arguments)] // Mirrors the provider trait plus stream selection.
    fn body(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra: Option<&Value>,
        stream: bool,
    ) -> Result<Value, AgentError> {
        request::body(
            self,
            messages,
            tools,
            max_tokens,
            temperature,
            model,
            extra,
            stream,
        )
    }
    async fn send(
        &self,
        body: &Value,
        stream: bool,
        model: Option<&str>,
    ) -> Result<reqwest::Response, AgentError> {
        let overridden;
        let endpoint = if self.endpoint.protocol == ProviderProtocol::GeminiGenerateContent
            && model.is_some_and(|model| model != self.config.model)
        {
            let mut config = self.config.clone();
            config.model = model.unwrap().to_owned();
            overridden = normalize_provider(&config)
                .map_err(|error| AgentError::Config(error.to_string()))?;
            &overridden
        } else {
            &self.endpoint
        };
        let response = self
            .client
            .post(if stream {
                endpoint.stream_url.clone()
            } else {
                endpoint.inference_url.clone()
            })
            .header(
                "accept",
                if stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            )
            .json(body)
            .send()
            .await
            .map_err(|error| {
                AgentError::LlmApi(format!(
                    "Provider transport failed: {}",
                    error.without_url()
                ))
            })?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let payload = read_json(response).await.unwrap_or(Value::Null);
        let code = payload
            .pointer("/error/code")
            .or_else(|| payload.pointer("/error/type"))
            .and_then(Value::as_str)
            .and_then(|code| diagnostics::identifier(code, &self.error_redactions))
            .unwrap_or_else(|| "request_failed".into());
        let model_error = if matches!(
            code.as_str(),
            "model_not_found" | "invalid_model" | "model_not_available"
        ) {
            let model = body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(&self.config.model);
            let model = if model.len() <= 160
                && !model.contains("://")
                && !model.is_empty()
                && model.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'/')
                }) {
                model
            } else {
                "<configured model>"
            };
            format!(
                ". Requested model {model} using {:?} is unavailable. Refresh models in Settings > Providers, choose an available model, and retry.",
                self.endpoint.protocol
            )
        } else {
            String::new()
        };
        let message = format!(
            "Provider returned HTTP {} ({code}){model_error}{}",
            status.as_u16(),
            if matches!(status.as_u16(), 400 | 413 | 415 | 422) && contains_images(body) {
                ". The selected provider rejected a request containing images. Check that the chosen model supports image input and its limits; no other model was selected."
            } else {
                ""
            }
        );
        Err(if matches!(status.as_u16(), 401 | 403) {
            AgentError::AuthFailed(message)
        } else {
            AgentError::LlmApi(message)
        })
    }
}

fn contains_images(value: &Value) -> bool {
    match value {
        Value::Object(fields) => {
            fields.contains_key("image_url")
                || fields.contains_key("inline_data")
                || fields
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| matches!(kind, "image" | "input_image"))
                || fields.values().any(contains_images)
        }
        Value::Array(values) => values.iter().any(contains_images),
        _ => false,
    }
}

async fn read_json(mut response: reqwest::Response) -> Result<Value, AgentError> {
    const MAX: usize = 8 * 1024 * 1024;
    if response
        .content_length()
        .is_some_and(|length| length > MAX as u64)
    {
        return Err(AgentError::LlmApi("Provider response exceeds 8 MiB".into()));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        AgentError::LlmApi(format!(
            "Provider response read failed: {}",
            error.without_url()
        ))
    })? {
        if bytes.len() + chunk.len() > MAX {
            return Err(AgentError::LlmApi("Provider response exceeds 8 MiB".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| AgentError::LlmApi(format!("Provider returned invalid JSON: {error}")))
}

#[async_trait]
impl LlmProvider for NativeProvider {
    async fn chat_completion(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra: Option<&Value>,
    ) -> Result<LlmResponse, AgentError> {
        if self.chatgpt_plan {
            let mut chunks =
                self.chat_completion_stream(messages, tools, max_tokens, temperature, model, extra);
            let mut collected = stream::Collected::default();
            while let Some(chunk) = chunks.next().await {
                collected.push(&chunk?)?;
            }
            return collected.response(model.unwrap_or(&self.config.model));
        }
        let body = self.body(
            messages,
            tools,
            max_tokens,
            temperature,
            model,
            extra,
            false,
        )?;
        let response = read_json(self.send(&body, false, model).await?).await?;
        request::parse_response(
            self.endpoint.protocol,
            &response,
            model.unwrap_or(&self.config.model),
        )
    }
    fn chat_completion_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra: Option<&Value>,
    ) -> BoxStream<'static, Result<StreamChunk, AgentError>> {
        let provider = self.clone();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        let model = model.map(str::to_owned);
        let extra = extra.cloned();
        Box::pin(async_stream::try_stream! {
            let body=provider.body(&messages,&tools,max_tokens,temperature,model.as_deref(),extra.as_ref(),true)?;
            let response=provider.send(&body,true,model.as_deref()).await?;
            let request_id=response.headers().get("x-request-id").and_then(|value|value.to_str().ok()).and_then(|value|diagnostics::identifier(value,&provider.error_redactions));
            let mut bytes=response.bytes_stream();
            let mut framing=stream::SseDecoder::default();
            let mut state=stream::EventState::new(provider.endpoint.protocol, provider.error_redactions.clone(), request_id);
            while let Some(chunk)=bytes.next().await {
                let chunk=chunk.map_err(|error|AgentError::LlmApi(format!("Provider stream read failed: {}",error.without_url())))?;
                for event in framing.feed(&chunk)? {
                    for chunk in state.event(event)?{yield chunk;}
                }
            }
            framing.finish()?;
            state.finish()?;
        })
    }
}
