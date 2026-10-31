//! The context window a local inference server actually serves.
//!
//! A catalog advertises what a model was trained for; a local server serves
//! what it was started with, and may silently truncate longer prompts (Ollama
//! defaults to a small context on machines with little memory). Budgets must
//! use the served value, so it is asked of the server itself:
//! - llama.cpp: `GET /props` → `default_generation_settings.n_ctx` (per slot).
//! - Ollama: `GET /api/ps` → the loaded model's `context_length`; otherwise
//!   `POST /api/show` → a `num_ctx` model parameter.
//! - LM Studio: `GET /api/v0/models` → a loaded model's `loaded_context_length`.
//!
//! Only loopback endpoints are probed, directly (never through a proxy) and
//! without credentials, under a short overall deadline; any failure means
//! "unknown" and leaves the configured window in force. Probing never starts
//! or loads a model. A [`ServedContextCache`] remembers each answer for its
//! owner, so a session pays for a successful probe once.

use super::discovery::bounded_json;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use url::Url;
use xylo_core::ProviderConfig;

/// The whole probe never takes longer than this.
const PROBE_DEADLINE: Duration = Duration::from_millis(2500);
/// One request's deadline.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(1000);
/// Ollama models whose parameters a catalog probe reads.
const MAX_SHOWN_MODELS: usize = 32;
/// Smallest believable served window.
const MIN_SERVED_TOKENS: u64 = 256;

/// What a local server reported about its context.
#[derive(Debug, Default, PartialEq, Eq)]
enum Server {
    /// Not a recognised local server, or it reported nothing.
    #[default]
    Unknown,
    /// llama.cpp serves one model with this per-slot context.
    LlamaCpp(u32),
    /// Ollama, with the context of each loaded model by name.
    Ollama(HashMap<String, u32>),
    /// LM Studio, with the context of each loaded model by id.
    LmStudio(HashMap<String, u32>),
}

/// The context the local server behind `config` serves for `config.model`,
/// or `None` when the endpoint is remote or the server does not say. Probes
/// every time; [`ServedContextCache`] remembers answers.
pub async fn served_context_tokens(
    config: &ProviderConfig,
    cancel: &CancellationToken,
) -> Option<u32> {
    local_root(&config.endpoint)?;
    let models = [config.model.clone()];
    served_context_limits(config, &models, cancel)
        .await
        .remove(&config.model)
}

/// Served contexts one owner (an engine, so one worker's session) learned,
/// by endpoint URL and model. Only answers are kept: a server that said
/// nothing, failed or was not running yet is asked again next time.
#[derive(Clone, Debug, Default)]
pub struct ServedContextCache(Arc<Mutex<HashMap<(String, String), u32>>>);

impl ServedContextCache {
    /// [`served_context_tokens`], probing each endpoint and model until it
    /// first answers and reusing that answer afterwards.
    pub async fn served_context_tokens(
        &self,
        config: &ProviderConfig,
        cancel: &CancellationToken,
    ) -> Option<u32> {
        let key = (config.endpoint.trim().to_owned(), config.model.clone());
        if let Some(known) = self
            .0
            .lock()
            .ok()
            .and_then(|known| known.get(&key).copied())
        {
            return Some(known);
        }
        let served = served_context_tokens(config, cancel).await?;
        if let Ok(mut known) = self.0.lock() {
            known.insert(key, served);
        }
        Some(served)
    }
}

/// The served context of each of `models` the local server behind `config`
/// reports; empty for remote endpoints.
pub async fn served_context_limits(
    config: &ProviderConfig,
    models: &[String],
    cancel: &CancellationToken,
) -> HashMap<String, u32> {
    let Some(root) = local_root(&config.endpoint) else {
        return HashMap::new();
    };
    let Ok(client) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(REQUEST_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
    else {
        return HashMap::new();
    };
    let probe = async {
        match detect(&client, &root, cancel).await {
            Server::Unknown => HashMap::new(),
            Server::LlamaCpp(tokens) => models.iter().map(|m| (m.clone(), tokens)).collect(),
            Server::LmStudio(loaded) => matching(models, &loaded),
            Server::Ollama(loaded) => {
                let mut limits = matching(models, &loaded);
                let unloaded = models
                    .iter()
                    .filter(|model| !limits.contains_key(*model))
                    .take(MAX_SHOWN_MODELS)
                    .cloned()
                    .collect::<Vec<_>>();
                for model in &unloaded {
                    if let Some(tokens) = ollama_num_ctx(&client, &root, model, cancel).await {
                        limits.insert(model.clone(), tokens);
                    }
                }
                limits
            }
        }
    };
    tokio::time::timeout(PROBE_DEADLINE, probe)
        .await
        .unwrap_or_default()
}

/// The server root of a loopback endpoint (`http://127.0.0.1:11434/v1` →
/// `http://127.0.0.1:11434/`).
fn local_root(endpoint: &str) -> Option<Url> {
    let mut url = Url::parse(endpoint.trim()).ok()?;
    if !matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
        || !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    Some(url)
}

async fn get(
    client: &reqwest::Client,
    url: Url,
    body: Option<Value>,
    cancel: &CancellationToken,
) -> Option<Value> {
    let request = match body {
        Some(body) => client.post(url).json(&body),
        None => client.get(url),
    };
    let response = tokio::select! {
        _ = cancel.cancelled() => return None,
        response = request.send() => response.ok()?,
    };
    bounded_json(response, cancel).await.ok()
}

async fn detect(client: &reqwest::Client, root: &Url, cancel: &CancellationToken) -> Server {
    let probe = |path: &str| {
        let url = root.join(path).ok();
        async move { get(client, url?, None, cancel).await }
    };
    if let Some(props) = probe("props").await {
        return parse_llama_props(&props).map_or(Server::Unknown, Server::LlamaCpp);
    }
    if let Some(ps) = probe("api/ps").await {
        return Server::Ollama(parse_ollama_ps(&ps));
    }
    if let Some(models) = probe("api/v0/models").await {
        return Server::LmStudio(parse_lm_studio(&models));
    }
    Server::Unknown
}

async fn ollama_num_ctx(
    client: &reqwest::Client,
    root: &Url,
    model: &str,
    cancel: &CancellationToken,
) -> Option<u32> {
    let shown = get(
        client,
        root.join("api/show").ok()?,
        Some(json!({"model": model})),
        cancel,
    )
    .await?;
    parse_ollama_num_ctx(&shown)
}

fn tokens(value: Option<&Value>) -> Option<u32> {
    value
        .and_then(Value::as_u64)
        .filter(|tokens| *tokens >= MIN_SERVED_TOKENS)
        .and_then(|tokens| u32::try_from(tokens).ok())
}

/// `n_ctx` of a llama.cpp `/props` response.
fn parse_llama_props(props: &Value) -> Option<u32> {
    tokens(props.pointer("/default_generation_settings/n_ctx"))
        .or_else(|| tokens(props.get("n_ctx")))
}

/// Loaded Ollama models and their context, by both `name` and `model`.
fn parse_ollama_ps(ps: &Value) -> HashMap<String, u32> {
    let mut loaded = HashMap::new();
    for model in ps
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(context) = tokens(model.get("context_length")) else {
            continue;
        };
        for key in ["name", "model"] {
            if let Some(name) = model.get(key).and_then(Value::as_str) {
                loaded.insert(name.to_owned(), context);
            }
        }
    }
    loaded
}

/// The `num_ctx` parameter of an Ollama `/api/show` response.
fn parse_ollama_num_ctx(shown: &Value) -> Option<u32> {
    shown
        .get("parameters")
        .and_then(Value::as_str)?
        .lines()
        .find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some("num_ctx")).then(|| words.next())?
        })
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|tokens| *tokens >= MIN_SERVED_TOKENS)
        .and_then(|tokens| u32::try_from(tokens).ok())
}

/// Loaded LM Studio models and their loaded context, by id.
fn parse_lm_studio(models: &Value) -> HashMap<String, u32> {
    models
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|model| model.get("state").and_then(Value::as_str) == Some("loaded"))
        .filter_map(|model| {
            Some((
                model.get("id")?.as_str()?.to_owned(),
                tokens(model.get("loaded_context_length"))?,
            ))
        })
        .collect()
}

/// The entries of `loaded` for `models`; Ollama names match with or without
/// the implicit `:latest` tag.
fn matching(models: &[String], loaded: &HashMap<String, u32>) -> HashMap<String, u32> {
    models
        .iter()
        .filter_map(|model| {
            let tokens = loaded
                .get(model)
                .or_else(|| loaded.get(&format!("{model}:latest")))?;
            Some((model.clone(), *tokens))
        })
        .collect()
}

#[cfg(test)]
mod tests;
