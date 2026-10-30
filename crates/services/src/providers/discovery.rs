use super::{ProviderCredential, normalize_provider};
use crate::CredentialVault;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use xylo_core::{ApiFamily, ProviderConfig};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderModel {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub context_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning_options: Option<Vec<String>>,
    #[serde(default)]
    pub default_reasoning_effort: Option<String>,
}

/// Account-specific catalog, bounded to eight pages / 2,000 models. Discovery is
/// read-only and never starts a model process or falls back to invented choices.
pub async fn discover_provider_models(
    config: &ProviderConfig,
    vault: &CredentialVault,
    cancel: &CancellationToken,
) -> Result<Vec<ProviderModel>> {
    let secret = if let Some(reference) = &config.credential_ref {
        let vault = vault.clone();
        let reference = reference.clone();
        Some(
            tokio::task::spawn_blocking(move || vault.get(&reference))
                .await??
                .context("Provider credential is not configured")?,
        )
    } else {
        None
    };
    let credential = ProviderCredential::parse(secret)?;
    discover_with_credential(config, &credential, cancel).await
}

pub(crate) async fn bounded_json(
    mut response: reqwest::Response,
    cancel: &CancellationToken,
) -> Result<Value> {
    let status = response.status();
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= 8 * 1024 * 1024),
        "Provider response exceeds 8 MiB"
    );
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::select! {_ = cancel.cancelled()=>bail!("Provider request cancelled"),chunk=response.chunk()=>chunk?};
        let Some(chunk) = chunk else { break };
        ensure!(
            bytes.len() + chunk.len() <= 8 * 1024 * 1024,
            "Provider response exceeds 8 MiB"
        );
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        // Return service error metadata rather than echoing an arbitrary response
        // body that could contain a reflected credential or account content.
        let value = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
        let code = value
            .pointer("/error/code")
            .or_else(|| value.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("request_failed");
        bail!(
            "Provider returned HTTP {} ({})",
            status.as_u16(),
            code.chars().take(128).collect::<String>()
        );
    }
    serde_json::from_slice(&bytes).context("Provider returned invalid JSON")
}

pub(crate) async fn discover_with_credential(
    config: &ProviderConfig,
    credential: &ProviderCredential,
    cancel: &CancellationToken,
) -> Result<Vec<ProviderModel>> {
    let normalized = normalize_provider(config)?;
    let mut url=normalized.models_url.context(match config.family {ApiFamily::Vertex=>"Vertex model selection requires an explicit publisher model ID; this client does not list project deployments",ApiFamily::Azure=>"Enter your Azure deployment name; model catalog IDs do not identify your deployments",_=>"Provider model discovery is unavailable for this endpoint; enter the model or deployment ID"})?;
    if matches!(credential, ProviderCredential::ChatGpt { .. }) {
        ensure!(
            matches!(config.family, ApiFamily::OpenAiResponses)
                && normalized.base_url.as_str().trim_end_matches('/')
                    == "https://api.openai.com/v1"
                && url.origin() == normalized.base_url.origin(),
            "ChatGPT catalog requires the public Responses API endpoint"
        );
    }
    if matches!(config.family, ApiFamily::Bedrock) {
        return bedrock_catalog(
            config,
            credential,
            url,
            normalized.aws_region.as_deref().unwrap_or("us-east-1"),
            cancel,
        )
        .await;
    }
    ensure!(
        !matches!(credential, ProviderCredential::Aws { .. }),
        "AWS credentials require the Bedrock API family"
    );
    let headers = credential.headers(&config.family, &config.headers)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(
            config.timeout_seconds.clamp(1, 60),
        ))
        .build()?;
    let mut output = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..8 {
        let response = tokio::select! {_ = cancel.cancelled()=>bail!("Model discovery cancelled"),response=client.get(url.clone()).headers(headers.clone()).send()=>response?};
        let value = bounded_json(response, cancel).await?;
        let rows = value
            .get("data")
            .or_else(|| value.get("models"))
            .or_else(|| value.get("modelSummaries"))
            .and_then(Value::as_array)
            .context("Provider returned no model catalog")?;
        for row in rows {
            // Gateways may advertise multiple protocols and non-chat models.
            // Respect explicit catalog metadata; plain OpenAI catalogs omit it.
            let api = match config.family {
                ApiFamily::OpenAiChat => Some("chatCompletions"),
                ApiFamily::OpenAiResponses => Some("responses"),
                _ => None,
            };
            if api.is_some_and(|api| {
                row.get("supported_apis")
                    .and_then(Value::as_array)
                    .is_some_and(|apis| !apis.iter().any(|value| value.as_str() == Some(api)))
            }) {
                continue;
            }
            if matches!(config.family, ApiFamily::Gemini)
                && row
                    .get("supportedGenerationMethods")
                    .and_then(Value::as_array)
                    .is_some_and(|methods| {
                        !methods
                            .iter()
                            .any(|method| method.as_str() == Some("generateContent"))
                    })
            {
                continue;
            }
            if matches!(credential, ProviderCredential::ChatGpt { .. })
                && row.get("visibility").and_then(Value::as_str) != Some("list")
            {
                continue;
            }
            if matches!(config.family, ApiFamily::Bedrock)
                && row
                    .get("outputModalities")
                    .and_then(Value::as_array)
                    .is_some_and(|modalities| {
                        !modalities
                            .iter()
                            .any(|modality| modality.as_str() == Some("TEXT"))
                    })
            {
                continue;
            }
            let Some(id) = model_identifier(
                row,
                matches!(credential, ProviderCredential::ChatGpt { .. }),
            ) else {
                continue;
            };
            let id = id.trim_start_matches("models/").to_owned();
            if seen.insert(id.clone()) {
                let display_name = row
                    .get("display_name")
                    .or_else(|| row.get("displayName"))
                    .or_else(|| row.get("modelName"))
                    .and_then(Value::as_str)
                    .unwrap_or(&id)
                    .to_owned();
                let context_tokens = [
                    "/context_window",
                    "/context_length",
                    "/inputTokenLimit",
                    "/metadata/context_length",
                ]
                .iter()
                .find_map(|path| row.pointer(path).and_then(Value::as_u64))
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n >= 1024);
                let (reasoning_options, default_reasoning_effort) = catalog_reasoning(row);
                output.push(ProviderModel {
                    reasoning_options,
                    default_reasoning_effort,
                    id,
                    display_name,
                    context_tokens,
                });
            }
            if output.len() >= 2000 {
                return Ok(output);
            }
        }
        let continuation = if matches!(config.family, ApiFamily::Gemini) {
            value
                .get("nextPageToken")
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
                .map(|token| ("pageToken", token))
        } else if value.get("has_more").and_then(Value::as_bool) == Some(true) {
            Some((
                "after_id",
                value
                    .get("last_id")
                    .and_then(Value::as_str)
                    .context("Provider omitted model pagination cursor")?,
            ))
        } else {
            None
        };
        let Some((name, token)) = continuation else {
            return Ok(output);
        };
        let mut pairs = url
            .query_pairs()
            .filter(|(key, _)| key != name)
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        pairs.push((name.into(), token.into()));
        url.set_query(None);
        url.query_pairs_mut().extend_pairs(pairs);
    }
    Ok(output)
}

async fn bedrock_catalog(
    config: &ProviderConfig,
    credential: &ProviderCredential,
    mut url: url::Url,
    region: &str,
    cancel: &CancellationToken,
) -> Result<Vec<ProviderModel>> {
    ensure!(
        !matches!(credential, ProviderCredential::None),
        "Bedrock catalog requires an explicit API key or AWS credential; environment/profile inference can use a manually entered model ID"
    );
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(
            config.timeout_seconds.clamp(1, 60),
        ))
        .build()?;
    let mut output = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for path in ["/foundation-models", "/inference-profiles"] {
        url.set_path(path);
        url.set_query(None);
        for _ in 0..8 {
            let headers = if matches!(credential, ProviderCredential::Aws { .. }) {
                super::aws_signing::catalog_headers(&url, region, credential)?
            } else {
                credential.headers(&ApiFamily::Bedrock, &config.headers)?
            };
            let response = tokio::select! {_ = cancel.cancelled()=>bail!("Model discovery cancelled"),response=client.get(url.clone()).headers(headers).send()=>response?};
            let value = bounded_json(response, cancel).await?;
            let profiles = path == "/inference-profiles";
            let rows = value[if profiles {
                "inferenceProfileSummaries"
            } else {
                "modelSummaries"
            }]
            .as_array()
            .context("Bedrock returned no catalog rows")?;
            for row in rows {
                if row["status"]
                    .as_str()
                    .is_some_and(|status| status != "ACTIVE")
                    || row["modelLifecycle"]["status"]
                        .as_str()
                        .is_some_and(|status| status != "ACTIVE")
                    || row["responseStreamingSupported"] == false
                    || row["outputModalities"]
                        .as_array()
                        .is_some_and(|modalities| !modalities.iter().any(|value| value == "TEXT"))
                {
                    continue;
                }
                let Some(id) = row[if profiles {
                    "inferenceProfileId"
                } else {
                    "modelId"
                }]
                .as_str()
                .filter(|id| !id.is_empty()) else {
                    continue;
                };
                if seen.insert(id.to_owned()) {
                    output.push(ProviderModel {
                        context_tokens: None,
                        reasoning_options: None,
                        default_reasoning_effort: None,
                        id: id.into(),
                        display_name: row[if profiles {
                            "inferenceProfileName"
                        } else {
                            "modelName"
                        }]
                        .as_str()
                        .unwrap_or(id)
                        .into(),
                    });
                }
                if output.len() >= 2000 {
                    return Ok(output);
                }
            }
            let Some(next) = value["nextToken"]
                .as_str()
                .filter(|token| !token.is_empty())
            else {
                break;
            };
            ensure!(
                next.len() <= 2048,
                "Bedrock catalog continuation exceeds its limit"
            );
            url.set_query(None);
            url.query_pairs_mut().append_pair("nextToken", next);
        }
    }
    Ok(output)
}

/// Provider-native model identifiers must survive discovery unchanged.
pub(super) fn model_identifier(row: &Value, chatgpt_plan: bool) -> Option<&str> {
    if chatgpt_plan {
        return row.get("slug").and_then(Value::as_str);
    }
    row.get("id")
        .or_else(|| row.get("slug"))
        .or_else(|| row.get("name"))
        .or_else(|| row.get("modelId"))
        .and_then(Value::as_str)
}

/// Preserve explicit catalog capability (including an empty list), without
/// inferring support from a model name or from a default field alone.
pub(super) fn catalog_reasoning(row: &Value) -> (Option<Vec<String>>, Option<String>) {
    let options = row
        .get("supported_reasoning_levels")
        .and_then(Value::as_array)
        .map(|levels| {
            let mut options = Vec::new();
            for level in levels.iter().take(16) {
                if let Some(effort) = level
                    .get("effort")
                    .and_then(Value::as_str)
                    .filter(|v| xylo_core::valid_reasoning_option(v))
                {
                    if !options.iter().any(|v| v == effort) {
                        options.push(effort.to_owned());
                    }
                }
            }
            options
        });
    let default = row
        .get("default_reasoning_level")
        .and_then(Value::as_str)
        .filter(|effort| {
            options
                .as_ref()
                .is_some_and(|options| options.iter().any(|v| v == effort))
        })
        .map(str::to_owned);
    (options, default)
}
