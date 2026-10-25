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
    ensure!(
        scopes
            .iter()
            .any(|scope| scope == "chatgpt.tokens.use.direct"),
        "ChatGPT plan usage was not authorized; reconnect and allow plan usage"
    );
    let expires = tokens["expires_in"]
        .as_i64()
        .filter(|value| *value > 0 && *value <= 86400)
        .context("Invalid ChatGPT access-token expiry")?;
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
        expires_at_ms: workbench_core::now_ms() + expires * 1000,
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
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;

    fn jwks() -> JwkSet {
        // Public modulus of the generated NONPRODUCTION fixture private key.
        let modulus = "A823867816F5C1917F9D3863DD8EFD2500340B75C33534788CAC01D8DDD725C4B5A8188D6DEB9AF12E89D22E523EEE9A723B8FDEC57B3F1B2C202291616B3049CCFEC9BED3B3CB9D3BC37E75D2C6595FD714C4DB055F955B3F5C126C7EEE7B3ED025F89315D9F84D5AE9177877DE96B8E5E2FF09C3EEE1D29B55956EC5F5B685BA1324DB5EAA6A956BD0D9EDFBFF8DC171C4D138FEF97B3BF2B57948CEBF4D0A69849C37CCA30D1B56FEF7FFD51DE1A23D89E274BA2B23560F239FA3E42148FAE10B8E8CF26967AAD6B1AA6887D183F06B00EAF7EF71C61E454F48B4C06BF29B9759A5C6F7C18022BD813C9C05D5330CF892305B1324B116E56CD0D1E09425A5";
        let bytes = (0..modulus.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&modulus[index..index + 2], 16).unwrap())
            .collect::<Vec<_>>();
        serde_json::from_value(json!({"keys":[{"kty":"RSA","kid":"workbench-test-only","use":"sig","alg":"RS256","n":URL_SAFE_NO_PAD.encode(bytes),"e":"AQAB"}]})).unwrap()
    }
    fn signed(claims: &Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("workbench-test-only".into());
        encode(
            &header,
            claims,
            &EncodingKey::from_rsa_pem(include_bytes!("fixtures/chatgpt-test-key.pem")).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn signed_identity_requires_published_key_issuer_audience_expiry_and_nonce() {
        let claims = json!({"iss":AUTHORITY,"sub":"fixture-account","aud":"oaiapp_fixture","exp":workbench_core::now_ms()/1000+3600,"nonce":"attempt-nonce","email":"fixture@example.invalid"});
        let identity = validate_identity(
            &signed(&claims),
            "oaiapp_fixture",
            Some("attempt-nonce"),
            &jwks(),
        )
        .unwrap();
        assert_eq!(identity.sub, "fixture-account");
        assert!(
            validate_identity(
                &signed(&claims),
                "different-client",
                Some("attempt-nonce"),
                &jwks()
            )
            .is_err()
        );
        assert!(
            validate_identity(
                &signed(&claims),
                "oaiapp_fixture",
                Some("different-nonce"),
                &jwks()
            )
            .is_err()
        );
        for (key, value) in [
            ("iss", json!("https://untrusted.example")),
            ("exp", json!(1)),
        ] {
            let mut invalid = claims.clone();
            invalid[key] = value;
            assert!(
                validate_identity(
                    &signed(&invalid),
                    "oaiapp_fixture",
                    Some("attempt-nonce"),
                    &jwks()
                )
                .is_err()
            );
        }
        let unsigned = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(b"not-an-openai-key"),
        )
        .unwrap();
        assert!(
            validate_identity(&unsigned, "oaiapp_fixture", Some("attempt-nonce"), &jwks()).is_err()
        );
    }

    #[test]
    fn dynamic_registration_binds_state_callback_and_issued_client_id() {
        let host = format!("urn:uuid:{}", uuid::Uuid::new_v4());
        let pending =
            PendingChatGptSignIn::begin(&host, "http://127.0.0.1:44321/auth/callback", None)
                .unwrap();
        let url = Url::parse(&pending.authorization_url).unwrap();
        let fields = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(fields["client_id"], DYNAMIC_CLIENT);
        assert_eq!(fields["agent_name_hint"], "Xylo");
        assert_eq!(fields["ext_agent_host_id"], host);
        assert_eq!(
            fields["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(pending.verifier.as_bytes()))
        );
        let callback = format!(
            "{}?code=fixture-code&state={}&client_id=oaiapp_issued",
            pending.redirect_uri, pending.state
        );
        assert_eq!(
            pending.callback(&callback).unwrap(),
            ("oaiapp_issued".into(), "fixture-code".into())
        );
        assert!(
            pending
                .callback(&callback.replace(&pending.state, "wrong-state"))
                .is_err()
        );
        assert!(
            pending
                .callback(&callback.replace("oaiapp_issued", DYNAMIC_CLIENT))
                .is_err()
        );
        assert!(
            pending
                .callback(&callback.replace("127.0.0.1", "localhost"))
                .is_err()
        );
        assert!(
            pending
                .callback(&format!("{callback}&state=duplicate"))
                .is_err()
        );
        assert!(
            PendingChatGptSignIn::begin(&host, "http://localhost:44321/auth/callback", None)
                .is_err()
        );
    }

    #[test]
    fn validated_identity_does_not_imply_plan_permission() {
        let identity = Identity {
            sub: "account".into(),
            iss: AUTHORITY.into(),
            nonce: None,
            email: None,
        };
        let tokens = json!({"id_token":"fixture-id","access_token":"fixture-access","token_type":"Bearer","scope":"openid profile email","expires_in":3600});
        assert!(credential(tokens, "oaiapp_issued".into(), "host".into(), identity).is_err());
    }
}
