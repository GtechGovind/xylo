//! Application-owned provider protocols and configuration. Capability flags describe
//! the wire protocol; actual model/account permissions still come from the service.
mod aws_signing;
mod discovery;
pub use discovery::*;
mod served_context;
pub use served_context::{ServedContextCache, served_context_limits, served_context_tokens};
mod chatgpt;
pub use chatgpt::*;
mod sign_in_service;
pub use sign_in_service::*;

use anyhow::{Context, Result, bail, ensure};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use url::Url;
use xylo_core::{ApiFamily, ProviderConfig, now_ms};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol {
    ChatCompletions,
    Responses,
    AnthropicMessages,
    GeminiGenerateContent,
    BedrockConverse,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub protocol: ProviderProtocol,
    pub streaming: bool,
    pub function_tools: bool,
    pub image_input: bool,
    pub structured_output: bool,
    pub account_or_model_dependent: bool,
}

pub fn protocol_capabilities(family: &ApiFamily) -> ProviderCapabilities {
    ProviderCapabilities {
        protocol: match family {
            ApiFamily::OpenAiChat | ApiFamily::Azure => ProviderProtocol::ChatCompletions,
            ApiFamily::OpenAiResponses => ProviderProtocol::Responses,
            ApiFamily::Anthropic => ProviderProtocol::AnthropicMessages,
            ApiFamily::Gemini | ApiFamily::Vertex => ProviderProtocol::GeminiGenerateContent,
            ApiFamily::Bedrock => ProviderProtocol::BedrockConverse,
        },
        streaming: true,
        function_tools: true,
        image_input: true,
        structured_output: !matches!(family, ApiFamily::Bedrock),
        account_or_model_dependent: true,
    }
}

#[derive(Clone, Debug)]
pub struct NormalizedProvider {
    pub base_url: Url,
    pub inference_url: Url,
    pub stream_url: Url,
    pub models_url: Option<Url>,
    pub protocol: ProviderProtocol,
    pub aws_region: Option<String>,
}

fn append(url: &Url, suffix: &str) -> Result<Url> {
    let mut result = url.clone();
    result.set_path(&format!(
        "{}/{}",
        url.path().trim_end_matches('/'),
        suffix.trim_start_matches('/')
    ));
    Ok(result)
}

/// Validate both inference and an explicitly supplied catalog destination.
/// Never accept embedded credentials or a silent redirect destination.
fn provider_url(endpoint: &str) -> Result<Url> {
    let base = Url::parse(endpoint.trim()).context("Invalid provider endpoint URL")?;
    ensure!(
        base.username().is_empty() && base.password().is_none() && base.fragment().is_none(),
        "Provider endpoint must not contain credentials or a fragment"
    );
    let loopback = matches!(base.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    ensure!(
        base.scheme() == "https" || (base.scheme() == "http" && loopback),
        "Use HTTPS or a loopback HTTP provider endpoint"
    );
    ensure!(
        !base.query_pairs().any(|(name, _)| {
            let normalized: String = name
                .bytes()
                .filter(u8::is_ascii_alphanumeric)
                .map(|byte| char::from(byte.to_ascii_lowercase()))
                .collect();
            matches!(
                normalized.as_str(),
                "key"
                    | "apikey"
                    | "xapikey"
                    | "token"
                    | "accesstoken"
                    | "refreshtoken"
                    | "clientsecret"
                    | "password"
                    | "authorization"
                    | "signature"
                    | "sig"
                    | "credential"
                    | "xamzsignature"
                    | "xamzcredential"
                    | "xgoogsignature"
                    | "xgoogcredential"
            )
        }),
        "Keep provider credentials in the OS credential store"
    );
    ensure!(
        !base
            .host_str()
            .is_some_and(|host| host == "chatgpt.com" || host.ends_with(".chatgpt.com"))
            && !base.path().contains("/backend-api"),
        "Use the official public API endpoint; ChatGPT backend-api endpoints are not supported"
    );
    Ok(base)
}

/// Accept a service root, version root, or complete inference endpoint. Queries
/// are preserved (legacy Azure api-version), credentials and fragments rejected.
pub fn normalize_provider(config: &ProviderConfig) -> Result<NormalizedProvider> {
    // Validate metadata before persisting a connection or updating its secret.
    ProviderCredential::None.headers(&config.family, &config.headers)?;
    let mut base = provider_url(&config.endpoint)?;
    let loopback = matches!(base.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    let path = base.path().trim_end_matches('/').to_owned();
    let protocol = protocol_capabilities(&config.family).protocol;
    let mut aws_region = None;
    let (inference_url, stream_url, models_url) = match config.family {
        ApiFamily::OpenAiChat | ApiFamily::OpenAiResponses | ApiFamily::Azure => {
            let suffix = if protocol == ProviderProtocol::Responses {
                "responses"
            } else {
                "chat/completions"
            };
            let root = path
                .strip_suffix("/chat/completions")
                .or_else(|| path.strip_suffix("/responses"))
                .unwrap_or(&path)
                .to_owned();
            base.set_path(&root);
            let azure = matches!(config.family, ApiFamily::Azure)
                || base.host_str().is_some_and(|host| {
                    host.ends_with(".openai.azure.com") || host.ends_with(".services.ai.azure.com")
                });
            if root.is_empty() {
                base.set_path(if azure { "/openai/v1" } else { "/v1" });
            }
            let legacy = base.path().contains("/openai/deployments/");
            if legacy {
                ensure!(
                    base.query_pairs()
                        .any(|(name, value)| name == "api-version" && !value.is_empty()),
                    "Legacy Azure deployment endpoints require an api-version query parameter"
                );
            }
            let inference = append(&base, suffix)?;
            (
                inference.clone(),
                inference,
                if legacy || azure {
                    None
                } else {
                    Some(append(&base, "models")?)
                },
            )
        }
        ApiFamily::Anthropic => {
            let root = path.strip_suffix("/messages").unwrap_or(&path);
            base.set_path(if root.is_empty() { "/v1" } else { root });
            let inference = append(&base, "messages")?;
            (inference.clone(), inference, Some(append(&base, "models")?))
        }
        ApiFamily::Gemini | ApiFamily::Vertex => {
            let root = path
                .split("/models/")
                .next()
                .unwrap_or(&path)
                .trim_end_matches("/models")
                .to_owned();
            if matches!(config.family, ApiFamily::Gemini) {
                base.set_path(if root.is_empty() { "/v1beta" } else { &root });
            } else {
                ensure!(
                    root.contains("/projects/")
                        && root.contains("/locations/")
                        && root.ends_with("/publishers/google"),
                    "Vertex endpoint must include /v1/projects/PROJECT/locations/LOCATION/publishers/google"
                );
                base.set_path(&root);
            }
            let model = config.model.trim().trim_start_matches("models/");
            ensure!(
                !model.contains(['?', '#', ':', '/']),
                "Use the Gemini model ID, not a full model URL"
            );
            let inference = append(&base, &format!("models/{model}:generateContent"))?;
            let mut streaming = append(&base, &format!("models/{model}:streamGenerateContent"))?;
            streaming.query_pairs_mut().append_pair("alt", "sse");
            let models = if matches!(config.family, ApiFamily::Gemini) {
                Some(append(&base, "models")?)
            } else {
                None
            };
            (inference, streaming, models)
        }
        ApiFamily::Bedrock => {
            ensure!(
                path.is_empty(),
                "Bedrock endpoint must be the runtime origin, without /model paths"
            );
            if let Some(host) = base.host_str() {
                aws_region = host
                    .strip_prefix("bedrock-runtime.")
                    .and_then(|host| host.strip_suffix(".amazonaws.com"))
                    .map(str::to_owned);
            }
            ensure!(
                loopback || aws_region.is_some(),
                "Use https://bedrock-runtime.REGION.amazonaws.com for Bedrock"
            );
            let mut inference = base.clone();
            inference
                .path_segments_mut()
                .map_err(|_| anyhow::anyhow!("Invalid Bedrock endpoint"))?
                .clear()
                .extend(["model", config.model.trim(), "converse"]);
            let mut streaming = inference.clone();
            let path = streaming.path().trim_end_matches("converse").to_owned();
            streaming.set_path(&format!("{path}converse-stream"));
            let catalog = aws_region
                .as_ref()
                .map(|region| {
                    Url::parse(&format!(
                        "https://bedrock.{region}.amazonaws.com/foundation-models"
                    ))
                })
                .transpose()?
                .or(if loopback {
                    Some(append(&base, "foundation-models")?)
                } else {
                    None
                });
            (inference, streaming, catalog)
        }
    };
    let models_url = if let Some(custom) = config
        .discovery_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        ensure!(
            !matches!(config.family, ApiFamily::Bedrock),
            "Custom catalog URLs are not supported for signed Bedrock discovery"
        );
        Some(provider_url(custom)?)
    } else {
        models_url
    };
    Ok(NormalizedProvider {
        base_url: base,
        inference_url,
        stream_url,
        models_url,
        protocol,
        aws_region,
    })
}

/// Secret-bearing variants intentionally do not implement Debug or Serialize.
#[derive(Clone)]
pub enum ProviderCredential {
    None,
    ApiKey(String),
    Bearer(String),
    Aws {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    },
    ChatGpt {
        access_token: String,
        client_id: String,
        subject: String,
        host_id: String,
        scopes: Vec<String>,
    },
}
impl ProviderCredential {
    pub fn parse(secret: Option<String>) -> Result<Self> {
        let Some(secret) = secret.filter(|secret| !secret.trim().is_empty()) else {
            return Ok(Self::None);
        };
        if !secret.trim_start().starts_with('{') {
            return Ok(Self::ApiKey(secret));
        }
        let value: serde_json::Value =
            serde_json::from_str(&secret).context("Invalid provider credential record")?;
        if let Some(expiry) = value
            .get("expires_at_ms")
            .and_then(serde_json::Value::as_i64)
        {
            ensure!(
                expiry > now_ms() + 30_000,
                "Provider credential has expired; reconnect or refresh it"
            );
        }
        let string = |name: &str| {
            value
                .get(name)
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .with_context(|| format!("Provider credential requires {name}"))
        };
        if value.get("access_key_id").is_some() {
            return Ok(Self::Aws {
                access_key_id: string("access_key_id")?,
                secret_access_key: string("secret_access_key")?,
                session_token: value
                    .get("session_token")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            });
        }
        if value.get("type").and_then(serde_json::Value::as_str) == Some("chatgpt") {
            let scopes: Vec<String> =
                serde_json::from_value(value.get("scopes").cloned().unwrap_or_default())
                    .context("ChatGPT credential requires granted scopes")?;
            ensure!(
                scopes
                    .iter()
                    .any(|scope| scope == "chatgpt.tokens.use.direct"),
                "This ChatGPT account has not authorized plan usage"
            );
            return Ok(Self::ChatGpt {
                access_token: string("access_token")?,
                client_id: string("client_id")?,
                subject: string("subject")?,
                host_id: string("ext_agent_host_id")?,
                scopes,
            });
        }
        if value.get("access_token").is_some() {
            return Ok(Self::Bearer(string("access_token")?));
        }
        if value.get("api_key").is_some() {
            return Ok(Self::ApiKey(string("api_key")?));
        }
        bail!("Unsupported provider credential record")
    }
    pub fn headers(
        &self,
        family: &ApiFamily,
        extra: &BTreeMap<String, String>,
    ) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", HeaderValue::from_static("Xylo/0.1"));
        if matches!(family, ApiFamily::Anthropic) {
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        }
        match self {
            Self::ApiKey(key) => {
                let name = match family {
                    ApiFamily::Anthropic => "x-api-key",
                    ApiFamily::Gemini => "x-goog-api-key",
                    ApiFamily::Azure => "api-key",
                    _ => "authorization",
                };
                let value = if name == "authorization" {
                    format!("Bearer {key}")
                } else {
                    key.clone()
                };
                headers.insert(
                    HeaderName::from_static(name),
                    HeaderValue::from_str(&value).context("Invalid credential header")?,
                );
            }
            Self::Bearer(token)
            | Self::ChatGpt {
                access_token: token,
                ..
            } => {
                headers.insert(
                    "authorization",
                    HeaderValue::from_str(&format!("Bearer {token}"))
                        .context("Invalid bearer credential")?,
                );
            }
            Self::None | Self::Aws { .. } => {}
        }
        validate_custom_headers(extra)?;
        for (name, value) in extra {
            let name = HeaderName::from_bytes(name.as_bytes())
                .context("Invalid custom provider header name")?;
            headers.insert(
                name,
                HeaderValue::from_str(value).context("Invalid custom provider header value")?,
            );
        }
        Ok(headers)
    }
}

/// Fragments that mark a header name as carrying a secret. Matching is by
/// substring on the lowercased name, so `proxy-authorization`, `x-auth-token`,
/// `cookie`, `x-session-id` and vendor variants are all refused.
const SECRET_HEADER_FRAGMENTS: &[&str] = &[
    "auth",
    "token",
    "key",
    "secret",
    "cookie",
    "session",
    "signature",
    "password",
    "credential",
    "bearer",
];

/// Headers the HTTP client owns; overriding them breaks or smuggles requests.
const TRANSPORT_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "keep-alive",
    "proxy-connection",
];

/// Custom headers are stored in plain settings and turn snapshots, so only
/// non-secret metadata (tenant, project, version and routing headers) may go
/// there. Credentials belong in the secret vault. The same rule applies to
/// chat and image providers.
pub fn validate_custom_headers(headers: &BTreeMap<String, String>) -> Result<()> {
    ensure!(headers.len() <= 16, "Use at most 16 custom headers");
    for (name, value) in headers {
        let parsed = HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("Invalid custom header name {name:?}"))?;
        let lower = parsed.as_str();
        ensure!(
            lower.len() <= 128 && value.len() <= 4096,
            "Custom header {name:?} is too long"
        );
        ensure!(
            !TRANSPORT_HEADERS.contains(&lower) && !lower.starts_with("proxy-"),
            "The {name:?} header is managed by the connection and cannot be set"
        );
        ensure!(
            !lower.starts_with("x-amz-")
                && !SECRET_HEADER_FRAGMENTS
                    .iter()
                    .any(|fragment| lower.contains(fragment)),
            "The {name:?} header looks like a credential; save secrets as the connection's key, not as a custom header"
        );
        HeaderValue::from_str(value)
            .with_context(|| format!("Invalid value for custom header {name:?}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
