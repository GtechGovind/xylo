//! OAuth protected-resource and authorization-server discovery with native public-client registration.
use super::*;
use serde_json::{Value, json};

/// A vault-only OAuth record. It is intentionally not Debug.
#[derive(Clone, Serialize, Deserialize)]
pub struct ConnectorOAuthCredential {
    pub config: OAuthConfig,
    pub tokens: OAuthTokens,
}

/// Discovers RFC 9728 and RFC 8414 metadata, then registers an RFC 7591 public client.
/// The caller invokes this only after the user chooses Sign in for a saved server.
pub async fn discover_oauth(
    endpoint: &str,
    redirect_uri: &str,
    client_id: Option<&str>,
    cancel: &CancellationToken,
) -> Result<OAuthConfig> {
    let resource = validate_endpoint(endpoint)?;
    ensure!(
        resource.fragment().is_none(),
        "OAuth resource must not contain a fragment"
    );
    validate_endpoint(redirect_uri)?;
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = tokio::select! { _ = cancel.cancelled() => bail!("Sign-in cancelled"), response = http.get(resource.clone()).send() => response? };
    let challenge = response
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .and_then(resource_metadata_challenge);
    let mut metadata_url = resource.clone();
    metadata_url.set_path(&format!(
        "/.well-known/oauth-protected-resource{}",
        resource.path().trim_end_matches('/')
    ));
    metadata_url.set_query(None);
    let document = if let Some(challenge) = challenge {
        metadata(
            &http,
            http.get(discovered_endpoint(&challenge, &resource)?),
            cancel,
        )
        .await?
    } else {
        match metadata(&http, http.get(metadata_url.clone()), cancel).await {
            Ok(value) => value,
            Err(_) if !cancel.is_cancelled() => {
                metadata_url.set_path("/.well-known/oauth-protected-resource");
                metadata(&http, http.get(metadata_url), cancel).await?
            }
            Err(error) => return Err(error),
        }
    };
    let declared = validate_endpoint(
        document["resource"]
            .as_str()
            .context("Resource metadata is missing its resource identifier")?,
    )?;
    ensure!(
        declared == resource,
        "Resource metadata identifies a different MCP server"
    );
    let issuer = document["authorization_servers"]
        .as_array()
        .and_then(|servers| servers.first())
        .and_then(Value::as_str)
        .context("Resource metadata has no authorization server")?;
    let issuer = discovered_endpoint(issuer, &resource)?;
    ensure!(
        issuer.query().is_none() && issuer.fragment().is_none(),
        "Invalid authorization server issuer"
    );
    let mut discovery = issuer.clone();
    discovery.set_path(&format!(
        "/.well-known/oauth-authorization-server{}",
        issuer.path().trim_end_matches('/')
    ));
    let auth = metadata(&http, http.get(discovery), cancel).await?;
    ensure!(
        auth["issuer"]
            .as_str()
            .and_then(|value| url::Url::parse(value).ok())
            .as_ref()
            == Some(&issuer),
        "Authorization metadata issuer mismatch"
    );
    let authorization_endpoint = auth["authorization_endpoint"]
        .as_str()
        .context("Authorization endpoint missing")?
        .to_owned();
    let token_endpoint = auth["token_endpoint"]
        .as_str()
        .context("Token endpoint missing")?
        .to_owned();
    discovered_endpoint(&authorization_endpoint, &resource)?;
    discovered_endpoint(&token_endpoint, &resource)?;
    ensure!(
        auth["code_challenge_methods_supported"]
            .as_array()
            .is_some_and(|methods| methods.iter().any(|method| method == "S256")),
        "Authorization server does not advertise PKCE S256"
    );
    let client_id = match client_id.filter(|id| !id.trim().is_empty()) {
        Some(id) => id.to_owned(),
        None => {
            let endpoint = discovered_endpoint(auth["registration_endpoint"].as_str().context("This server requires a registered client ID; dynamic client registration is unavailable")?, &resource)?;
            let registration = metadata(&http, http.post(endpoint).json(&json!({"client_name":"Xylo","redirect_uris":[redirect_uri],"grant_types":["authorization_code","refresh_token"],"response_types":["code"],"token_endpoint_auth_method":"none"})), cancel).await?;
            ensure!(
                registration["token_endpoint_auth_method"]
                    .as_str()
                    .is_none_or(|method| method == "none"),
                "Server registration requires unsupported confidential-client authentication"
            );
            registration["client_id"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 4096)
                .context("Registration returned no client ID")?
                .to_owned()
        }
    };
    let scopes = document["scopes_supported"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(64)
        .map(str::to_owned)
        .collect();
    Ok(OAuthConfig {
        authorization_endpoint,
        token_endpoint,
        client_id,
        scopes,
        resource: Some(resource.to_string()),
    })
}
pub(super) fn discovered_endpoint(value: &str, resource: &url::Url) -> Result<url::Url> {
    let endpoint = validate_endpoint(value)?;
    ensure!(
        resource.scheme() != "https" || endpoint.scheme() == "https",
        "OAuth discovery cannot downgrade HTTPS"
    );
    Ok(endpoint)
}
fn resource_metadata_challenge(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let part = part.trim().strip_prefix("Bearer ").unwrap_or(part.trim());
        let (name, value) = part.split_once('=')?;
        (name.trim().eq_ignore_ascii_case("resource_metadata"))
            .then(|| value.trim().trim_matches('"').to_owned())
    })
}
async fn metadata(
    _http: &reqwest::Client,
    request: reqwest::RequestBuilder,
    cancel: &CancellationToken,
) -> Result<Value> {
    let response = tokio::select! { _ = cancel.cancelled() => bail!("Sign-in cancelled"), response = request.send() => response? };
    ensure!(
        response.status().is_success(),
        "OAuth metadata endpoint returned HTTP {}",
        response.status()
    );
    ensure!(
        response.content_length().is_none_or(|size| size <= 65536),
        "OAuth metadata exceeds size limit"
    );
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    loop {
        let chunk = tokio::select! { _ = cancel.cancelled() => bail!("Sign-in cancelled"), chunk = stream.next() => chunk };
        let Some(chunk) = chunk else { break };
        let chunk = chunk?;
        ensure!(
            body.len() + chunk.len() <= 65536,
            "OAuth metadata exceeds size limit"
        );
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).context("OAuth metadata was not valid JSON")
}

/// Resolves an OAuth record in the vault, refreshing it before a new request when needed.
/// Raw bearer credentials remain supported and are never parsed or logged as errors.
pub async fn connector_access_token(
    vault: &CredentialVault,
    reference: &str,
    cancel: &CancellationToken,
) -> Result<String> {
    let vault_copy = vault.clone();
    let key = reference.to_owned();
    let stored = tokio::task::spawn_blocking(move || vault_copy.get(&key))
        .await??
        .context("Connector credential is missing")?;
    let Ok(mut credential) = serde_json::from_str::<ConnectorOAuthCredential>(&stored) else {
        return Ok(stored);
    };
    if credential.tokens.is_expired() {
        let gate = refresh_gate(reference)?;
        let _guard = tokio::select! { _ = cancel.cancelled() => bail!("Sign-in refresh cancelled"), guard = gate.lock() => guard };
        let vault_copy = vault.clone();
        let key = reference.to_owned();
        let stored = tokio::task::spawn_blocking(move || vault_copy.get(&key))
            .await??
            .context("Connector credential is missing")?;
        credential = serde_json::from_str(&stored)?;
        if credential.tokens.is_expired() {
            let refresh = credential
                .tokens
                .refresh_token
                .as_deref()
                .context("Sign in again; server supplied no refresh token")?;
            credential.tokens = exchange(
                &credential.config,
                &[("grant_type", "refresh_token"), ("refresh_token", refresh)],
                credential.tokens.refresh_token.clone(),
                cancel,
            )
            .await?;
            let json = serde_json::to_string(&credential)?;
            let vault = vault.clone();
            let key = reference.to_owned();
            tokio::task::spawn_blocking(move || vault.set(&key, &json)).await??;
        }
    }
    Ok(credential.tokens.access_token)
}
