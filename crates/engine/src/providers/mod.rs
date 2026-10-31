//! Native HTTP protocol adapters for the five supported wire protocols: Chat
//! Completions, Responses, Anthropic Messages, Gemini generateContent and
//! Bedrock Converse. No provider selection starts a subprocess, reads another
//! application's auth cache, or silently changes to another API family.
mod bedrock;
mod diagnostics;
mod renewal;
mod request;
mod stream;

pub(crate) use renewal::build_renewable;

use crate::model::{EventStream, ModelError, ModelProvider, ModelRequest, ModelUsage};
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use xylo_core::{ApiFamily, ProviderConfig};
use xylo_services::providers::{
    NormalizedProvider, ProviderCredential, ProviderProtocol, normalize_provider,
};

/// Builds the adapter for `config` with the stored `secret`.
pub(crate) fn build(
    config: &ProviderConfig,
    secret: Option<String>,
) -> Result<Arc<dyn ModelProvider>> {
    let credential = ProviderCredential::parse(secret)?;
    if matches!(config.family, ApiFamily::Bedrock) {
        return Ok(Arc::new(bedrock::BedrockProvider::new(config, credential)?));
    }
    Ok(Arc::new(NativeProvider::new(config.clone(), credential)?))
}

/// Header values that must never appear in an error message.
fn redactions(headers: &reqwest::header::HeaderMap) -> Arc<Vec<String>> {
    let mut redactions = Vec::new();
    for value in headers.values().filter_map(|value| value.to_str().ok()) {
        if !value.is_empty() {
            redactions.push(value.to_owned());
        }
        if let Some((scheme, secret)) = value.split_once(' ')
            && (scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("basic"))
            && !secret.is_empty()
        {
            redactions.push(secret.to_owned());
        }
    }
    redactions.sort_by_key(|value| std::cmp::Reverse(value.len()));
    redactions.dedup();
    Arc::new(redactions)
}

/// The HTTP client every adapter uses: no redirects, no automatic retries.
fn http_client(headers: reqwest::header::HeaderMap) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .default_headers(headers)
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .build()
        .context("Create provider HTTP client")
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
            "Save a provider credential before connecting"
        );
        ensure!(
            !matches!(credential, ProviderCredential::Aws { .. }),
            "AWS credentials require the Bedrock API family"
        );
        let headers = credential.headers(&config.family, &config.headers)?;
        let error_redactions = redactions(&headers);
        Ok(Self {
            client: http_client(headers)?,
            config,
            endpoint,
            chatgpt_plan,
            error_redactions,
        })
    }

    async fn send(&self, body: &Value) -> Result<reqwest::Response, ModelError> {
        let response = self
            .client
            .post(self.endpoint.stream_url.clone())
            .header("accept", "text/event-stream")
            .json(body)
            .send()
            .await
            .map_err(|error| ModelError::Provider {
                // Once request transmission may have started, replay is unsafe.
                retryable: error.is_connect(),
                retry_after: None,
                message: format!("Provider transport failed: {}", error.without_url()),
            })?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| retry_after_seconds(value, chrono::Utc::now().timestamp()))
            .map(Duration::from_secs);
        let payload = read_json(response).await.unwrap_or(Value::Null);
        let code = payload
            .pointer("/error/code")
            .or_else(|| payload.pointer("/error/type"))
            .and_then(Value::as_str)
            .and_then(|code| diagnostics::identifier(code, &self.error_redactions))
            .unwrap_or_else(|| "request_failed".into());
        let reason = diagnostics::reason(&payload, &self.error_redactions);
        Err(self.http_error(status, &code, reason.as_deref(), retry_after, body))
    }

    fn http_error(
        &self,
        status: u16,
        code: &str,
        reason: Option<&str>,
        retry_after: Option<Duration>,
        body: &Value,
    ) -> ModelError {
        let unavailable_model = matches!(
            code,
            "model_not_found" | "invalid_model" | "model_not_available"
        );
        let model_hint = if unavailable_model {
            let model = &self.config.model;
            let model = if model.len() <= 160
                && !model.contains("://")
                && !model.is_empty()
                && model.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'/')
                }) {
                model.as_str()
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
        let image_hint = if matches!(status, 400 | 413 | 415 | 422) && contains_images(body) {
            ". The selected provider rejected a request containing images. Check that the chosen model supports image input and its limits; no other model was selected."
        } else {
            ""
        };
        let reason = reason
            .map(|reason| format!(": {reason}"))
            .unwrap_or_default();
        let message = if code == "request_failed" {
            format!("Provider returned HTTP {status}{reason}{model_hint}{image_hint}")
        } else {
            format!("Provider returned HTTP {status} ({code}){reason}{model_hint}{image_hint}")
        };
        if self.chatgpt_plan && status == 401 && code == "token_expired" {
            ModelError::Authentication(renewal::TOKEN_EXPIRED.into())
        } else if matches!(status, 401 | 403) || code == "invalid_api_key" {
            ModelError::Authentication(message)
        } else if status == 429 {
            ModelError::RateLimited {
                retry_after,
                message,
            }
        } else if matches!(
            code,
            "context_length_exceeded" | "string_above_max_length" | "request_too_large"
        ) {
            ModelError::ContextOverflow(message)
        } else if unavailable_model
            || matches!(
                code,
                "invalid_request_error" | "invalid_request" | "invalid_schema"
            )
        {
            ModelError::InvalidRequest(message)
        } else {
            ModelError::Provider {
                retryable: matches!(status, 408 | 409 | 500..=599),
                retry_after,
                message,
            }
        }
    }
}

impl ModelProvider for NativeProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> EventStream {
        let provider = self.clone();
        Box::pin(async_stream::try_stream! {
            let body = request::body(&provider, &request.messages, &request.tools, request.max_tokens, request.reasoning)?;
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(ModelError::Interrupted),
                response = provider.send(&body) => response,
            }?;
            let request_id = response
                .headers()
                .get("x-request-id")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| diagnostics::identifier(value, &provider.error_redactions));
            let mut bytes = response.bytes_stream();
            let mut framing = stream::SseDecoder::default();
            let mut state = stream::EventState::new(
                provider.endpoint.protocol,
                provider.error_redactions.clone(),
                request_id,
            );
            loop {
                let chunk = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(ModelError::Interrupted),
                    chunk = bytes.next() => Ok(chunk),
                }?;
                let Some(chunk) = chunk else { break };
                let chunk = chunk.map_err(|error| {
                    ModelError::transient(format!(
                        "Provider stream read failed: {}",
                        error.without_url()
                    ))
                })?;
                for event in framing.feed(&chunk)? {
                    for event in state.event(event)? {
                        yield event;
                    }
                }
            }
            framing.finish()?;
            state.finish()?;
        })
    }
}

/// Parses a `Retry-After` header given as delta seconds or an HTTP date.
fn retry_after_seconds(value: &str, now: i64) -> Option<u64> {
    value.trim().parse().ok().or_else(|| {
        chrono::DateTime::parse_from_rfc2822(value)
            .ok()
            .map(|date| date.timestamp().saturating_sub(now).max(0) as u64)
    })
}

fn contains_images(value: &Value) -> bool {
    match value {
        Value::Object(fields) => {
            fields.contains_key("image_url")
                || fields.contains_key("inlineData")
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

/// Reads a bounded JSON error body.
async fn read_json(response: reqwest::Response) -> Result<Value, ModelError> {
    let bytes = read_bounded(response).await?;
    serde_json::from_slice(&bytes)
        .map_err(|error| ModelError::permanent(format!("Provider returned invalid JSON: {error}")))
}

/// Reads at most 8 MiB of a response body.
async fn read_bounded(mut response: reqwest::Response) -> Result<Vec<u8>, ModelError> {
    const MAX: usize = 8 * 1024 * 1024;
    let oversized = || ModelError::permanent("Provider response exceeds 8 MiB");
    if response
        .content_length()
        .is_some_and(|length| length > MAX as u64)
    {
        return Err(oversized());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        ModelError::transient(format!(
            "Provider response read failed: {}",
            error.without_url()
        ))
    })? {
        if bytes.len() + chunk.len() > MAX {
            return Err(oversized());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn usage(input_tokens: u64, output_tokens: u64) -> ModelUsage {
    ModelUsage {
        input_tokens,
        output_tokens,
    }
}

#[cfg(test)]
mod tests;
