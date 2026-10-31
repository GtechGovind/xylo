//! OAuth 2.0 authorization-code (PKCE) sign-in and token refresh for connectors.
//!
//! Refreshes are single-flight per credential reference within the process:
//! providers that rotate refresh tokens revoke the old one on first use, so two
//! concurrent refreshes with the same token would leave the loser holding a
//! revoked credential. A waiter re-reads the stored record after acquiring the
//! gate and skips its own exchange when another caller already rotated it.
use super::mcp::validate_endpoint;
use crate::CredentialVault;
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Weak},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// Endpoints and client registration for one OAuth provider.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OAuthConfig {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub scopes: Vec<String>,
}
/// Token material is deliberately not Debug. Persist only through CredentialVault.
#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at_ms: Option<i64>,
    pub scope: Option<String>,
}
impl OAuthTokens {
    /// True when the access token expires within the next 30 seconds.
    pub fn is_expired(&self) -> bool {
        self.expires_at_ms
            .is_some_and(|expiry| expiry <= xylo_core::now_ms() + 30_000)
    }
}
/// One-use PKCE state. Keep in host memory; never put verifier into UI, logs or SQLite.
pub struct PendingOAuth {
    pub authorization_url: String,
    state: String,
    verifier: String,
    redirect_uri: String,
    config: OAuthConfig,
    created_at: std::time::Instant,
}
impl PendingOAuth {
    /// Builds a browser authorization URL without network calls. Native clients
    /// should supply an ephemeral loopback redirect URI and receive its callback.
    pub fn begin(config: OAuthConfig, redirect_uri: &str) -> Result<Self> {
        let mut endpoint = validate_endpoint(&config.authorization_endpoint)?;
        validate_endpoint(&config.token_endpoint)?;
        let redirect = validate_endpoint(redirect_uri)?;
        ensure!(
            redirect.query().is_none() && redirect.fragment().is_none(),
            "OAuth redirect must not include query or fragment"
        );
        ensure!(
            !config.client_id.trim().is_empty(),
            "OAuth client ID is required"
        );
        let mut verifier_bytes = [0u8; 32];
        let mut state_bytes = [0u8; 32];
        getrandom::fill(&mut verifier_bytes)
            .map_err(|e| anyhow::anyhow!("OS randomness unavailable: {e}"))?;
        getrandom::fill(&mut state_bytes)
            .map_err(|e| anyhow::anyhow!("OS randomness unavailable: {e}"))?;
        let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
        let state = URL_SAFE_NO_PAD.encode(state_bytes);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        endpoint
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &config.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", &config.scopes.join(" "))
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(Self {
            authorization_url: endpoint.to_string(),
            state,
            verifier,
            redirect_uri: redirect_uri.into(),
            config,
            created_at: std::time::Instant::now(),
        })
    }
    /// Consuming self prevents duplicate exchanges. The host must parse the state and
    /// code from the actual browser callback, not from model-generated content.
    pub async fn complete(
        self,
        code: &str,
        returned_state: &str,
        cancel: &CancellationToken,
    ) -> Result<OAuthTokens> {
        ensure!(
            self.created_at.elapsed() < Duration::from_secs(600),
            "OAuth flow expired"
        );
        ensure!(
            constant_time_eq(returned_state.as_bytes(), self.state.as_bytes()),
            "OAuth state mismatch"
        );
        ensure!(!code.is_empty(), "OAuth authorization code is required");
        exchange(
            &self.config,
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", &self.redirect_uri),
                ("code_verifier", &self.verifier),
            ],
            None,
            cancel,
        )
        .await
    }
}
/// Refresh is explicit. Never retry a business mutation automatically after auth
/// failure: refreshing a credential and replaying a write are separate decisions.
///
/// Concurrent refreshes of one `reference` in this process perform a single
/// token exchange; callers that waited return once the rotated record is stored.
pub async fn refresh_oauth(
    config: &OAuthConfig,
    vault: &CredentialVault,
    reference: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    let observed = load_tokens(vault, reference).await?;
    let gate = refresh_gate(reference)?;
    let _flight = tokio::select! {
        _ = cancel.cancelled() => bail!("OAuth refresh cancelled"),
        guard = gate.lock() => guard,
    };
    let current = load_tokens(vault, reference).await?;
    if current.access_token != observed.access_token
        || current.refresh_token != observed.refresh_token
    {
        // Another caller rotated the credential while this one waited.
        return Ok(());
    }
    let refresh = current
        .refresh_token
        .as_deref()
        .context("Provider did not issue a refresh token; reconnect the account")?;
    let tokens = exchange(
        config,
        &[("grant_type", "refresh_token"), ("refresh_token", refresh)],
        current.refresh_token.clone(),
        cancel,
    )
    .await?;
    let v = vault.clone();
    let r = reference.to_owned();
    let json = serde_json::to_string(&tokens)?;
    tokio::task::spawn_blocking(move || v.set(&r, &json)).await??;
    Ok(())
}

async fn load_tokens(vault: &CredentialVault, reference: &str) -> Result<OAuthTokens> {
    let v = vault.clone();
    let r = reference.to_owned();
    let json = tokio::task::spawn_blocking(move || v.get(&r))
        .await??
        .context("OAuth credential is absent")?;
    Ok(serde_json::from_str(&json)?)
}

type RefreshGates = std::sync::Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>;

/// Per-reference refresh gates. Entries are weak so finished references do not
/// accumulate; a gate lives exactly as long as some caller holds or awaits it.
static REFRESH_GATES: LazyLock<RefreshGates> = LazyLock::new(Default::default);

fn refresh_gate(reference: &str) -> Result<Arc<tokio::sync::Mutex<()>>> {
    ensure!(
        !reference.trim().is_empty() && reference.len() <= 256,
        "Invalid credential reference"
    );
    let mut gates = REFRESH_GATES
        .lock()
        .map_err(|_| anyhow::anyhow!("OAuth refresh coordination is unavailable"))?;
    gates.retain(|_, gate| gate.strong_count() > 0);
    if let Some(gate) = gates.get(reference).and_then(Weak::upgrade) {
        return Ok(gate);
    }
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    gates.insert(reference.to_owned(), Arc::downgrade(&gate));
    Ok(gate)
}

/// Compares secrets without an early exit on the first differing byte, so the
/// response time does not reveal how much of a guessed `state` was correct.
/// Lengths are not secret (the state is a fixed-size encoding).
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let difference = left
        .iter()
        .zip(right)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    std::hint::black_box(difference) == 0
}

async fn exchange(
    config: &OAuthConfig,
    form: &[(&str, &str)],
    old_refresh: Option<String>,
    cancel: &CancellationToken,
) -> Result<OAuthTokens> {
    validate_endpoint(&config.token_endpoint)?;
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut body = form.to_vec();
    body.push(("client_id", &config.client_id));
    let response = tokio::select! {_ = cancel.cancelled()=>bail!("OAuth exchange cancelled"),result=http.post(&config.token_endpoint).form(&body).send()=>result?};
    ensure!(
        response.status().is_success(),
        "OAuth token endpoint returned HTTP {}",
        response.status()
    );
    ensure!(
        response.content_length().is_none_or(|size| size <= 65536),
        "OAuth response exceeds limit"
    );
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    loop {
        let chunk = tokio::select! {_ = cancel.cancelled()=>bail!("OAuth exchange cancelled"),chunk=stream.next()=>chunk};
        let Some(chunk) = chunk else { break };
        let chunk = chunk?;
        ensure!(
            bytes.len() + chunk.len() <= 65536,
            "OAuth response exceeds limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let access = value
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .context("OAuth response has no access token")?;
    ensure!(
        value
            .get("token_type")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|s| s.eq_ignore_ascii_case("bearer")),
        "Unsupported OAuth token type"
    );
    let expires = value
        .get("expires_in")
        .and_then(serde_json::Value::as_i64)
        .filter(|seconds| *seconds > 0)
        .map(|seconds| xylo_core::now_ms().saturating_add(seconds.saturating_mul(1000)));
    Ok(OAuthTokens {
        access_token: access.into(),
        refresh_token: value
            .get("refresh_token")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .or(old_refresh),
        expires_at_ms: expires,
        scope: value
            .get("scope")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    })
}

#[cfg(test)]
mod tests;
