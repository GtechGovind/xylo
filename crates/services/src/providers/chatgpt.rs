//! Official local/open-source Sign in with ChatGPT public-client flow. See
//! developers.openai.com/siwc/token-sharing-open-source/sign-in. Commercial/hosted
//! registration is a separate partner flow and is not emulated by this module.
use super::discovery::bounded_json;
use crate::{CredentialVault, Store};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use url::Url;

const AUTHORITY: &str = "https://auth.openai.com";
const RESOURCE: &str = "https://api.openai.com/v1";
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";

/// Keep this entire record in CredentialVault; it deliberately has no Debug.
#[derive(Clone, Serialize, Deserialize)]
pub struct ChatGptCredential {
    #[serde(rename = "type")]
    kind: String,
    pub client_id: String,
    pub subject: String,
    pub issuer: String,
    pub email: Option<String>,
    pub ext_agent_host_id: String,
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at_ms: i64,
    pub scopes: Vec<String>,
}
impl ChatGptCredential {
    pub fn save(&self, vault: &CredentialVault, reference: &str) -> Result<()> {
        vault.set(reference, &serde_json::to_string(self)?)
    }
    pub fn plan_usage_authorized(&self) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope == "chatgpt.tokens.use.direct")
    }
}

impl Store {
    /// Stable per application profile/host; do not copy this profile to a new host.
    pub fn chatgpt_host_id(&self) -> Result<String> {
        self.call(|connection| {
            let candidate = format!("urn:uuid:{}", uuid::Uuid::new_v4());
            connection.execute(
                "INSERT OR IGNORE INTO settings(key,value) VALUES('chatgpt_host_id',?1)",
                [candidate],
            )?;
            Ok(connection.query_row(
                "SELECT value FROM settings WHERE key='chatgpt_host_id'",
                [],
                |row| row.get(0),
            )?)
        })
    }
}

pub struct PendingChatGptSignIn {
    /// Contains an ID-token hint on returning sign-in: open in browser, never log.
    pub authorization_url: String,
    redirect_uri: String,
    host_id: String,
    state: String,
    nonce: String,
    verifier: String,
    issued_client_id: Option<String>,
    expected_subject: Option<String>,
    created: Instant,
}
fn random() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("OS randomness unavailable: {error}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn constant_equal(left: &str, right: &str) -> bool {
    let mut difference = left.len() ^ right.len();
    for (left, right) in left.bytes().zip(right.bytes()) {
        difference |= (left ^ right) as usize;
    }
    difference == 0
}
impl PendingChatGptSignIn {
    /// The host must bind the loopback listener first. This documented dynamic
    /// registration is for this local/open-source application, not hosted use.
    pub fn begin(
        host_id: &str,
        redirect_uri: &str,
        previous: Option<&ChatGptCredential>,
    ) -> Result<Self> {
        let redirect = Url::parse(redirect_uri).context("Invalid ChatGPT callback URI")?;
        ensure!(
            redirect.scheme() == "http"
                && redirect.host_str() == Some("127.0.0.1")
                && redirect.port().is_some()
                && redirect.path() == "/auth/callback"
                && redirect.query().is_none()
                && redirect.fragment().is_none()
                && redirect.username().is_empty()
                && redirect.password().is_none(),
            "ChatGPT callback must be http://127.0.0.1:PORT/auth/callback"
        );
        ensure!(
            host_id.starts_with("urn:uuid:") && uuid::Uuid::parse_str(&host_id[9..]).is_ok(),
            "ChatGPT sign-in requires a stable host UUID"
        );
        let state = random()?;
        let nonce = random()?;
        let verifier = random()?;
        let mut authorize = Url::parse(&format!("{AUTHORITY}/api/accounts/authorize"))?;
        let mut query = authorize.query_pairs_mut();
        query
            .append_pair(
                "client_id",
                previous.map_or(DYNAMIC_CLIENT, |credential| &credential.client_id),
            )
            .append_pair("ext_agent_host_id", host_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair(
                "scope",
                "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct",
            )
            .append_pair("resource", RESOURCE)
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair(
                "code_challenge",
                &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            );
        if let Some(previous) = previous {
            // This returning registration has no plan grant. The settings action
            // explicitly asks to enable it; do not force ordinary authorized logins.
            if !previous.plan_usage_authorized() {
                query.append_pair("prompt", "consent");
            }
            ensure!(
                previous.ext_agent_host_id == host_id
                    && previous.client_id != DYNAMIC_CLIENT
                    && !previous.client_id.is_empty(),
                "ChatGPT registration does not belong to this host"
            );
            if !previous.id_token.is_empty() {
                query.append_pair("id_token_hint", &previous.id_token);
            }
            if let Some(email) = &previous.email {
                query.append_pair("login_hint", email);
            }
        } else {
            query.append_pair("agent_name_hint", "Xylo");
        }
        drop(query);
        Ok(Self {
            authorization_url: authorize.into(),
            redirect_uri: redirect_uri.into(),
            host_id: host_id.into(),
            state,
            nonce,
            verifier,
            issued_client_id: previous.map(|credential| credential.client_id.clone()),
            expected_subject: previous.map(|credential| credential.subject.clone()),
            created: Instant::now(),
        })
    }
    pub(super) fn callback(&self, callback: &str) -> Result<(String, String)> {
        ensure!(
            self.created.elapsed() < Duration::from_secs(600),
            "ChatGPT sign-in attempt expired"
        );
        let callback = Url::parse(callback).context("Invalid ChatGPT sign-in callback")?;
        let mut expected = Url::parse(&self.redirect_uri)?;
        expected.set_query(None);
        let mut actual = callback.clone();
        actual.set_query(None);
        ensure!(
            actual == expected && callback.fragment().is_none(),
            "ChatGPT callback URI does not match the pending sign-in"
        );
        let mut parameters = std::collections::HashMap::new();
        for (key, value) in callback.query_pairs() {
            ensure!(
                parameters
                    .insert(key.into_owned(), value.into_owned())
                    .is_none(),
                "ChatGPT callback contains duplicate fields"
            );
        }
        ensure!(
            parameters
                .get("state")
                .is_some_and(|state| constant_equal(state, &self.state)),
            "ChatGPT sign-in state did not match"
        );
        if let Some(error) = parameters.get("error") {
            bail!(
                "ChatGPT authorization was declined ({})",
                error.chars().take(128).collect::<String>()
            )
        }
        let client_id = match &self.issued_client_id {
            Some(issued) => {
                ensure!(
                    parameters
                        .get("client_id")
                        .is_none_or(|client| client == issued),
                    "ChatGPT callback changed the selected account registration"
                );
                issued.clone()
            }
            None => parameters
                .get("client_id")
                .filter(|client| !client.is_empty() && client.as_str() != DYNAMIC_CLIENT)
                .cloned()
                .context("ChatGPT registration did not return an issued client ID")?,
        };
        let code = parameters
            .get("code")
            .filter(|code| !code.is_empty())
            .cloned()
            .context("ChatGPT callback did not include an authorization code")?;
        Ok((client_id, code))
    }
    pub(super) fn is_matching_denial(&self, callback: &str) -> bool {
        let Ok(callback) = Url::parse(callback) else {
            return false;
        };
        let Ok(mut expected) = Url::parse(&self.redirect_uri) else {
            return false;
        };
        expected.set_query(None);
        let mut actual = callback.clone();
        actual.set_query(None);
        if actual != expected || callback.fragment().is_some() {
            return false;
        }
        let mut fields = std::collections::HashMap::new();
        for (key, value) in callback.query_pairs() {
            if fields.insert(key, value).is_some() {
                return false;
            }
        }
        fields
            .get("error")
            .is_some_and(|error| error == "access_denied")
            && fields
                .get("state")
                .is_some_and(|state| constant_equal(state, &self.state))
    }
    pub async fn complete(
        self,
        callback: &str,
        cancel: &CancellationToken,
    ) -> Result<ChatGptCredential> {
        let (client_id, code) = self.callback(callback)?;
        let client = client()?;
        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", client_id.as_str()),
            ("code", code.as_str()),
            ("code_verifier", self.verifier.as_str()),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("resource", RESOURCE),
        ];
        let response = tokio::select! {_ = cancel.cancelled()=>bail!("ChatGPT sign-in cancelled"),response=client.post(format!("{AUTHORITY}/api/accounts/oauth/token")).form(&form).send()=>response?};
        let tokens = bounded_json(response, cancel).await?;
        let id_token = required(&tokens, "id_token")?;
        let jwks = fetch_jwks(&client, cancel).await?;
        let identity = validate_identity(&id_token, &client_id, Some(&self.nonce), &jwks)?;
        ensure!(
            self.expected_subject
                .as_ref()
                .is_none_or(|expected| expected == &identity.sub),
            "ChatGPT sign-in returned a different account"
        );
        credential(tokens, client_id, self.host_id, identity)
    }
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()?)
}
fn required(value: &Value, name: &str) -> Result<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .with_context(|| format!("ChatGPT token response omitted {name}"))
}
#[derive(Deserialize)]
struct Identity {
    sub: String,
    iss: String,
    nonce: Option<String>,
    email: Option<String>,
}
fn validate_identity(
    token: &str,
    client_id: &str,
    nonce: Option<&str>,
    jwks: &JwkSet,
) -> Result<Identity> {
    let header = decode_header(token).context("Invalid ChatGPT ID token header")?;
    ensure!(
        matches!(header.alg, Algorithm::RS256 | Algorithm::ES256),
        "Unsupported ChatGPT ID-token signing algorithm"
    );
    let key = jwks
        .find(
            header
                .kid
                .as_deref()
                .context("ChatGPT ID token omitted signing key ID")?,
        )
        .context("ChatGPT signing key was not found")?;
    let mut validation = Validation::new(header.alg);
    validation.set_audience(&[client_id]);
    validation.set_issuer(&[AUTHORITY]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.leeway = 30;
    let identity = decode::<Identity>(token, &DecodingKey::from_jwk(key)?, &validation)
        .context("ChatGPT ID-token signature or identity claims are invalid")?
        .claims;
    if let Some(nonce) = nonce {
        ensure!(
            identity
                .nonce
                .as_deref()
                .is_some_and(|actual| constant_equal(actual, nonce)),
            "ChatGPT ID-token nonce did not match"
        );
    }
    ensure!(
        !identity.sub.is_empty(),
        "ChatGPT ID token omitted account identity"
    );
    Ok(identity)
}
async fn openid_configuration(
    client: &reqwest::Client,
    cancel: &CancellationToken,
) -> Result<Value> {
    let response = tokio::select! {_ = cancel.cancelled()=>bail!("ChatGPT authentication cancelled"),response=client.get(format!("{AUTHORITY}/.well-known/openid-configuration")).send()=>response?};
    let configuration = bounded_json(response, cancel).await?;
    ensure!(
        configuration["issuer"] == AUTHORITY,
        "Unexpected ChatGPT authentication issuer"
    );
    Ok(configuration)
}
fn official_auth_url(value: &Value, name: &str) -> Result<Url> {
    let url = Url::parse(&required(value, name)?)?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("auth.openai.com")
            && url.username().is_empty()
            && url.password().is_none(),
        "ChatGPT authentication metadata references an untrusted endpoint"
    );
    Ok(url)
}
async fn fetch_jwks(client: &reqwest::Client, cancel: &CancellationToken) -> Result<JwkSet> {
    let configuration = openid_configuration(client, cancel).await?;
    let url = official_auth_url(&configuration, "jwks_uri")?;
    let response = tokio::select! {_ = cancel.cancelled()=>bail!("ChatGPT authentication cancelled"),response=client.get(url).send()=>response?};
    serde_json::from_value(bounded_json(response, cancel).await?)
        .context("Invalid ChatGPT signing-key set")
}
/// Upper bound on a stored access-token lifetime (30 days).
const MAX_ACCESS_TOKEN_LIFETIME_SECS: i64 = 30 * 24 * 60 * 60;

fn credential(
    tokens: Value,
    client_id: String,
    host_id: String,
    identity: Identity,
) -> Result<ChatGptCredential> {
    let scopes = required(&tokens, "scope")?
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    // A validated identity may be retained without plan consent. Inference and
    // discovery enforce the granted plan scope at the credential-use boundary.
    // Issuers may grant multi-day tokens; refresh still happens on use, so a
    // long lifetime is only capped to keep the stored expiry sane.
    let expires = tokens["expires_in"]
        .as_i64()
        .filter(|value| *value > 0)
        .context("Invalid ChatGPT access-token expiry")?
        .min(MAX_ACCESS_TOKEN_LIFETIME_SECS);
    ensure!(
        tokens["token_type"]
            .as_str()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("bearer")),
        "Unsupported ChatGPT access-token type"
    );
    Ok(ChatGptCredential {
        kind: "chatgpt".into(),
        client_id,
        subject: identity.sub,
        issuer: identity.iss,
        email: identity.email,
        ext_agent_host_id: host_id,
        id_token: required(&tokens, "id_token")?,
        access_token: required(&tokens, "access_token")?,
        refresh_token: tokens["refresh_token"].as_str().map(str::to_owned),
        expires_at_ms: xylo_core::now_ms() + expires * 1000,
        scopes,
    })
}

/// Serialize this operation per credential reference in the host. It never retries
/// inference or a tool call. The host persists the replacement atomically in vault.
pub async fn refresh_chatgpt_credential(
    current: &ChatGptCredential,
    cancel: &CancellationToken,
) -> Result<ChatGptCredential> {
    let refresh = current
        .refresh_token
        .as_deref()
        .context("Reconnect ChatGPT; this account has no renewable session")?;
    ensure!(
        current.client_id != DYNAMIC_CLIENT,
        "ChatGPT refresh requires the issued account client ID"
    );
    let client = client()?;
    let form = [
        ("grant_type", "refresh_token"),
        ("client_id", current.client_id.as_str()),
        ("refresh_token", refresh),
        ("resource", RESOURCE),
    ];
    let response = tokio::select! {_ = cancel.cancelled()=>bail!("ChatGPT token refresh cancelled"),response=client.post(format!("{AUTHORITY}/api/accounts/oauth/token")).form(&form).send()=>response?};
    let mut tokens = bounded_json(response, cancel).await?;
    if tokens.get("scope").is_none() {
        tokens["scope"] = Value::String(current.scopes.join(" "));
    }
    if tokens.get("refresh_token").is_none() {
        tokens["refresh_token"] = serde_json::to_value(&current.refresh_token)?;
    }
    let identity = if let Some(token) = tokens["id_token"].as_str() {
        validate_identity(
            token,
            &current.client_id,
            None,
            &fetch_jwks(&client, cancel).await?,
        )?
    } else {
        tokens["id_token"] = Value::String(current.id_token.clone());
        Identity {
            sub: current.subject.clone(),
            iss: current.issuer.clone(),
            nonce: None,
            email: current.email.clone(),
        }
    };
    ensure!(
        identity.sub == current.subject,
        "ChatGPT refresh returned a different account"
    );
    credential(
        tokens,
        current.client_id.clone(),
        current.ext_agent_host_id.clone(),
        identity,
    )
}

/// An empty HTTP 200 confirms revocation. On failure the host may still clear local
/// tokens, but must report that remote revocation was not confirmed.
pub async fn revoke_chatgpt_credential(
    current: &ChatGptCredential,
    cancel: &CancellationToken,
) -> Result<()> {
    let Some(refresh) = current.refresh_token.as_deref() else {
        return Ok(());
    };
    let client = client()?;
    let configuration = openid_configuration(&client, cancel).await?;
    let url = official_auth_url(&configuration, "revocation_endpoint")?;
    let response = tokio::select! {_ = cancel.cancelled()=>bail!("ChatGPT sign-out cancelled"),response=client.post(url).form(&[("token",refresh),("token_type_hint","refresh_token"),("client_id",current.client_id.as_str())]).send()=>response?};
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "ChatGPT remote revocation was not confirmed (HTTP {})",
        response.status().as_u16()
    );
    Ok(())
}

#[cfg(test)]
mod tests;
