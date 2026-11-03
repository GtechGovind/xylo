//! Native server-side search requests stay separate from conversation tools.
//! Adapters preserve provider citation structures as untrusted JSON data.
use super::*;
use crate::{
    model::{Message, Reasoning},
    tools::web::SearchArguments,
};
use serde_json::json;

pub(super) fn search_request(
    provider: &NativeProvider,
    args: &SearchArguments,
) -> Result<Value, ModelError> {
    let mut query = format!(
        "Search the web for the following query and cite your sources: {}",
        args.query
    );
    // Gemini generateContent has no portable include-domain filter. The
    // query narrows retrieval; the result parser enforces host boundaries.
    if provider.endpoint.protocol == ProviderProtocol::GeminiGenerateContent {
        if !args.allowed_domains.is_empty() {
            query.push_str(&format!(
                " ({})",
                args.allowed_domains
                    .iter()
                    .map(|domain| format!("site:{domain}"))
                    .collect::<Vec<_>>()
                    .join(" OR ")
            ));
        }
        for domain in &args.blocked_domains {
            query.push_str(&format!(" -site:{domain}"));
        }
    }
    let mut body = request::body(
        provider,
        &[Message::user(query)],
        &[],
        4096,
        Reasoning::Lowest,
    )?;
    if provider.endpoint.protocol == ProviderProtocol::Responses
        && body.pointer("/reasoning/effort").and_then(Value::as_str) == Some("minimal")
    {
        // Hosted search rejects GPT-5's ordinary minimum. Select only an
        // advertised alternative; extraction still uses the ordinary builder.
        const ORDER: [&str; 6] = ["none", "low", "medium", "high", "xhigh", "max"];
        let efforts = provider.config.supported_reasoning_efforts();
        let effort = ORDER
            .into_iter()
            .find(|effort| efforts.contains(effort))
            .ok_or_else(|| {
                request::invalid(
                    "Web search requires a supported reasoning effort other than minimal",
                )
            })?;
        body["reasoning"]["effort"] = json!(effort);
    }
    body["stream"] = json!(false);
    match provider.endpoint.protocol {
        ProviderProtocol::Responses => {
            let mut tool = json!({"type":"web_search","search_context_size":"low"});
            if !args.allowed_domains.is_empty() {
                tool["filters"] = json!({"allowed_domains":args.allowed_domains});
            } else if !args.blocked_domains.is_empty() {
                tool["filters"] = json!({"blocked_domains":args.blocked_domains});
            }
            body["tools"] = json!([tool]);
            body["tool_choice"] = json!({"type":"web_search"});
            body["include"] = json!(["web_search_call.action.sources"]);
        }
        ProviderProtocol::AnthropicMessages => {
            let mut tool = json!({"type":"web_search_20250305","name":"web_search","max_uses":3});
            if !args.allowed_domains.is_empty() {
                tool["allowed_domains"] = json!(args.allowed_domains);
            } else if !args.blocked_domains.is_empty() {
                tool["blocked_domains"] = json!(args.blocked_domains);
            }
            body["tools"] = json!([tool]);
        }
        ProviderProtocol::GeminiGenerateContent => {
            body.as_object_mut()
                .expect("request object")
                .remove("stream");
            body["tools"] = json!([{"google_search":{}}]);
        }
        _ => return Err(request::invalid("This API family has no native search")),
    }
    Ok(body)
}

pub(super) async fn execute(
    provider: &NativeProvider,
    arguments: Value,
    addresses: Vec<std::net::SocketAddr>,
    cancel: CancellationToken,
) -> Result<Value, ModelError> {
    let args = SearchArguments::parse(&arguments)
        .map_err(|_| request::invalid("Invalid search query or domain lists"))?;
    let body = search_request(provider, &args)?;
    let endpoint = if provider.endpoint.protocol == ProviderProtocol::GeminiGenerateContent {
        provider.endpoint.inference_url.clone()
    } else {
        provider.endpoint.stream_url.clone()
    };
    let host = endpoint
        .host_str()
        .ok_or_else(|| request::invalid("Search endpoint has no host"))?
        .trim_matches(['[', ']']);
    if addresses.is_empty() {
        return Err(request::invalid(
            "Search endpoint has no approved addresses",
        ));
    }
    let client = reqwest::Client::builder()
        .default_headers(provider.search_headers.clone())
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .resolve_to_addrs(host, &addresses)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| ModelError::permanent("Create search client"))?;
    let response = tokio::select! { biased; _ = cancel.cancelled() => return Err(ModelError::Interrupted), response = client.post(endpoint).header("accept","application/json").json(&body).send() => response.map_err(|_|ModelError::permanent("Search provider connection failed"))? };
    let status = response.status().as_u16();
    let payload = read_json(response).await?;
    if !(200..300).contains(&status) {
        let code = payload
            .pointer("/error/code")
            .or_else(|| payload.pointer("/error/type"))
            .and_then(Value::as_str)
            .and_then(|code| diagnostics::identifier(code, &provider.error_redactions))
            .unwrap_or_else(|| "request_failed".into());
        return Err(provider.http_error(status, &code, None, None, &body));
    }
    let mut result = parse(provider.endpoint.protocol, &payload, &args)?;
    result["model"] = json!(
        payload
            .get("model")
            .and_then(Value::as_str)
            .or_else(|| payload.get("modelVersion").and_then(Value::as_str))
    );
    result["requested_model"] = json!(provider.config.model);
    Ok(result)
}

fn parse(
    protocol: ProviderProtocol,
    payload: &Value,
    args: &SearchArguments,
) -> Result<Value, ModelError> {
    let mut results = Vec::new();
    let mut citations = Vec::new();
    let mut answer = String::new();
    let mut add = |item: &Value, url: &str, title: &str, snippet: &str| {
        if protocol == ProviderProtocol::GeminiGenerateContent || args.permits(url) {
            results.push(json!({"title":title,"url":url,"snippet":snippet}));
            citations.push(item.clone());
        }
    };
    match protocol {
        ProviderProtocol::Responses => {
            for output in payload["output"].as_array().into_iter().flatten() {
                for source in output
                    .pointer("/action/sources")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(url) = source["url"].as_str() {
                        add(source, url, source["title"].as_str().unwrap_or(url), "");
                    }
                }
                for content in output["content"].as_array().into_iter().flatten() {
                    if let Some(text) = content["text"].as_str() {
                        answer.push_str(text);
                    }
                    let text = content["text"].as_str().unwrap_or("");
                    let boundaries: Vec<usize> = text
                        .char_indices()
                        .map(|(index, _)| index)
                        .chain(std::iter::once(text.len()))
                        .collect();
                    for citation in content["annotations"].as_array().into_iter().flatten() {
                        if let Some(url) = citation["url"].as_str() {
                            add(
                                citation,
                                url,
                                citation["title"].as_str().unwrap_or(url),
                                cited_range(text, &boundaries, citation),
                            );
                        }
                    }
                }
            }
        }
        ProviderProtocol::AnthropicMessages => {
            for block in payload["content"].as_array().into_iter().flatten() {
                if let Some(text) = block["text"].as_str() {
                    answer.push_str(text);
                }
                if block["type"] == "web_search_tool_result" {
                    if block["content"]["type"] == "web_search_tool_result_error" {
                        return Err(ModelError::permanent(
                            "The provider's web search tool failed",
                        ));
                    }
                    for item in block["content"].as_array().into_iter().flatten() {
                        if let Some(url) = item["url"].as_str() {
                            add(item, url, item["title"].as_str().unwrap_or(url), "");
                        }
                    }
                }
                for citation in block["citations"].as_array().into_iter().flatten() {
                    if let Some(url) = citation["url"].as_str() {
                        add(
                            citation,
                            url,
                            citation["title"].as_str().unwrap_or(url),
                            citation["cited_text"].as_str().unwrap_or(""),
                        );
                    }
                }
            }
        }
        ProviderProtocol::GeminiGenerateContent => {
            for candidate in payload["candidates"].as_array().into_iter().flatten() {
                for part in candidate
                    .pointer("/content/parts")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(text) = part["text"].as_str() {
                        answer.push_str(text);
                    }
                }
                let metadata = &candidate["groundingMetadata"];
                for chunk in metadata["groundingChunks"].as_array().into_iter().flatten() {
                    if let Some(url) = chunk.pointer("/web/uri").and_then(Value::as_str) {
                        add(
                            chunk,
                            url,
                            chunk
                                .pointer("/web/title")
                                .and_then(Value::as_str)
                                .unwrap_or(url),
                            "",
                        );
                    }
                }
            }
        }
        _ => return Err(request::invalid("Unsupported native search response")),
    }
    let mut merged: Vec<Value> = Vec::new();
    for item in results {
        if let Some(prior) = merged.iter_mut().find(|prior| prior["url"] == item["url"]) {
            if prior["snippet"].as_str().unwrap_or_default().is_empty() {
                prior["snippet"] = item["snippet"].clone();
            }
            if prior["title"] == prior["url"] {
                prior["title"] = item["title"].clone();
            }
        } else {
            merged.push(item);
        }
    }
    let results = merged;
    // A provider answer can mention removed domains; domain-filtered calls
    // return only the permitted sources and their source-specific excerpts.
    if !args.allowed_domains.is_empty() || !args.blocked_domains.is_empty() {
        answer.clear();
    }
    let mut result = json!({"results":results,"citations":citations,"answer":answer,"backend":"provider","usage":payload.get("usage").or_else(||payload.get("usageMetadata")),"grounding_metadata":if protocol == ProviderProtocol::GeminiGenerateContent && args.allowed_domains.is_empty() && args.blocked_domains.is_empty() {payload.pointer("/candidates/0/groundingMetadata").cloned()}else{None}});
    if protocol == ProviderProtocol::GeminiGenerateContent {
        // Grounding URIs can be Google redirects. The broker owns the network
        // authority needed to identify publishers before applying domain rules.
        result["source_resolution"] = json!("gemini_grounding");
    }
    Ok(result)
}

/// Returns only the annotation's character range, never the surrounding answer.
fn cited_range<'a>(text: &'a str, boundaries: &[usize], citation: &Value) -> &'a str {
    let range = || {
        let start = usize::try_from(citation["start_index"].as_u64()?).ok()?;
        let end = usize::try_from(citation["end_index"].as_u64()?).ok()?;
        if start >= end {
            return None;
        }
        text.get(*boundaries.get(start)?..*boundaries.get(end)?)
    };
    range().unwrap_or("")
}

#[cfg(test)]
mod tests;
