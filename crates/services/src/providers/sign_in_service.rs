use super::{
    ChatGptCredential, PendingChatGptSignIn, ProviderCredential, normalize_provider,
    refresh_chatgpt_credential, revoke_chatgpt_credential,
};
use crate::CredentialVault;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;
use xylo_core::{ApiFamily, ProviderConfig, now_ms};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatGptAccountSummary {
    pub credential_ref: String,
    pub subject: String,
    pub email: Option<String>,
    pub expires_at_ms: i64,
    pub plan_usage_authorized: bool,
    pub connected: bool,
}

/// One native coordinator per host. Cross-process locks also serialize rotating
/// refresh tokens with another cooperating instance using the same data directory.
#[derive(Clone)]
pub struct ChatGptSignInService {
    locks: PathBuf,
    capacity: Arc<tokio::sync::Semaphore>,
}
impl ChatGptSignInService {
    pub fn new(data_dir: &Path) -> Result<Self> {
        let locks = data_dir.join("auth-locks");
        std::fs::create_dir_all(&locks)?;
        Ok(Self {
            locks,
            capacity: Arc::new(tokio::sync::Semaphore::new(8)),
        })
    }
    async fn lease(&self, reference: &str, cancel: &CancellationToken) -> Result<File> {
        ensure!(
            !reference.is_empty() && reference.len() <= 256,
            "Invalid credential reference"
        );
        let path = self
            .locks
            .join(format!("{:x}.lock", Sha256::digest(reference.as_bytes())));
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(660);
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(file),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {}
                Err(error) => return Err(error.into()),
            }
            tokio::select! {_ = cancel.cancelled()=>bail!("ChatGPT connection cancelled"),_ = tokio::time::sleep_until(deadline)=>bail!("Another ChatGPT sign-in is still active"),_ = tokio::time::sleep(Duration::from_millis(40))=>{}}
        }
    }
    async fn load(vault: CredentialVault, reference: String) -> Result<Option<String>> {
        tokio::task::spawn_blocking(move || vault.get(&reference)).await?
    }
    async fn save(
        vault: CredentialVault,
        reference: String,
        credential: ChatGptCredential,
    ) -> Result<()> {
        tokio::task::spawn_blocking(move || credential.save(&vault, &reference)).await?
    }
    pub async fn sign_in<F, Fut>(
        &self,
        host_id: &str,
        vault: CredentialVault,
        reference: &str,
        cancel: CancellationToken,
        authorize: F,
    ) -> Result<ChatGptAccountSummary>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let _capacity = tokio::select! {_ = cancel.cancelled()=>bail!("ChatGPT sign-in cancelled"),capacity=self.capacity.acquire()=>capacity?};
        let _lease = self.lease(reference, &cancel).await?;
        let previous = Self::load(vault.clone(), reference.into())
            .await?
            .map(|record| {
                serde_json::from_str::<ChatGptCredential>(&record)
                    .context("The selected credential is not a ChatGPT registration")
            })
            .transpose()?;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let redirect = format!(
            "http://127.0.0.1:{}/auth/callback",
            listener.local_addr()?.port()
        );
        let pending = PendingChatGptSignIn::begin(host_id, &redirect, previous.as_ref())?;
        authorize(pending.authorization_url.clone()).await?;
        let callback = receive_callback(&listener, &pending, &redirect, &cancel).await?;
        let (mut socket, url) = callback;
        let result = pending.complete(&url, &cancel).await;
        match result {
            Ok(credential) => {
                let summary = summary(reference, &credential);
                Self::save(vault, reference.into(), credential).await?;
                let _ = reply(&mut socket, 200, "Connected. You can return to Xylo.").await;
                Ok(summary)
            }
            Err(error) => {
                let _ = reply(
                    &mut socket,
                    400,
                    "Sign-in could not be completed. Return to Xylo to try again.",
                )
                .await;
                Err(error)
            }
        }
    }
    pub async fn prepare_provider_credential(
        &self,
        config: &ProviderConfig,
        vault: CredentialVault,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.resolve_provider_credential(config, vault, cancel)
            .await
            .map(|_| ())
    }
    pub async fn resolve_provider_credential(
        &self,
        config: &ProviderConfig,
        vault: CredentialVault,
        cancel: &CancellationToken,
    ) -> Result<Option<String>> {
        let Some(reference) = config.credential_ref.as_deref() else {
            return Ok(None);
        };
        let _lease = self.lease(reference, cancel).await?;
        let record = Self::load(vault.clone(), reference.into())
            .await?
            .context("Provider credential is not configured")?;
        let value = serde_json::from_str::<serde_json::Value>(&record).ok();
        if value
            .as_ref()
            .and_then(|value| value.get("type"))
            .and_then(serde_json::Value::as_str)
            != Some("chatgpt")
        {
            ProviderCredential::parse(Some(record.clone()))?;
            return Ok(Some(record));
        }
        let endpoint = normalize_provider(config)?;
        ensure!(
            config.family == ApiFamily::OpenAiResponses
                && endpoint.base_url.as_str().trim_end_matches('/') == "https://api.openai.com/v1",
            "ChatGPT account access requires the official public Responses API"
        );
        let credential: ChatGptCredential = serde_json::from_str(&record)?;
        if credential.expires_at_ms > now_ms() + 60_000 {
            ensure!(
                credential.plan_usage_authorized(),
                "ChatGPT plan usage is not authorized"
            );
            return Ok(Some(record));
        }
        let renewed = refresh_chatgpt_credential(&credential, cancel).await?;
        let authorized = renewed.plan_usage_authorized();
        let record = serde_json::to_string(&renewed)?;
        Self::save(vault, reference.into(), renewed).await?;
        ensure!(
            authorized,
            "ChatGPT plan usage is not authorized; reconnect and allow plan usage"
        );
        Ok(Some(record))
    }
    pub async fn account(
        &self,
        vault: CredentialVault,
        reference: &str,
    ) -> Result<Option<ChatGptAccountSummary>> {
        let Some(record) = Self::load(vault, reference.into()).await? else {
            return Ok(None);
        };
        let credential: ChatGptCredential = serde_json::from_str(&record)
            .context("The selected credential is not a ChatGPT registration")?;
        Ok(Some(summary(reference, &credential)))
    }
    /// Clear local tokens even if remote revocation fails. A non-None warning
    /// means revocation was not confirmed and must be shown to the user.
    pub async fn sign_out(
        &self,
        vault: CredentialVault,
        reference: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<String>> {
        let _lease = self.lease(reference, cancel).await?;
        let Some(record) = Self::load(vault.clone(), reference.into()).await? else {
            return Ok(None);
        };
        let credential: ChatGptCredential = serde_json::from_str(&record)?;
        let warning=revoke_chatgpt_credential(&credential,cancel).await.err().map(|_|"Signed out locally. Remote disconnection was not confirmed; you can disconnect this app in ChatGPT Settings.".to_owned());
        // Preserve the validated account/client registration for a later sign-in,
        // while removing every token and active grant as required by SIWC.
        let mut registration = credential;
        registration.access_token.clear();
        registration.id_token.clear();
        registration.refresh_token = None;
        registration.expires_at_ms = 0;
        registration.scopes.clear();
        Self::save(vault, reference.into(), registration).await?;
        Ok(warning)
    }
}
fn summary(reference: &str, credential: &ChatGptCredential) -> ChatGptAccountSummary {
    ChatGptAccountSummary {
        credential_ref: reference.into(),
        subject: credential.subject.clone(),
        email: credential.email.clone(),
        expires_at_ms: credential.expires_at_ms,
        plan_usage_authorized: credential.plan_usage_authorized(),
        connected: !credential.access_token.is_empty() || credential.refresh_token.is_some(),
    }
}

/// Browsers open speculative connections that may never send a request, and
/// other local software can connect too. Each connection is read on its own,
/// so an idle or broken one is dropped without delaying or ending sign-in.
async fn receive_callback(
    listener: &tokio::net::TcpListener,
    pending: &PendingChatGptSignIn,
    redirect: &str,
    cancel: &CancellationToken,
) -> Result<(tokio::net::TcpStream, String)> {
    use futures_util::{StreamExt, stream::FuturesUnordered};
    const OPEN_CONNECTIONS: usize = 16;
    const INVALID_REQUESTS: usize = 32;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    let mut reads = FuturesUnordered::new();
    let mut invalid = 0;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => bail!("ChatGPT sign-in cancelled"),
            _ = tokio::time::sleep_until(deadline) => bail!("ChatGPT sign-in timed out"),
            accepted = listener.accept() => {
                // A failed accept affects one connection, not the listener.
                let Ok((socket, peer)) = accepted else { continue };
                if peer.ip().is_loopback() && reads.len() < OPEN_CONNECTIONS {
                    reads.push(request_line(socket));
                }
            }
            Some(read) = reads.next() => {
                // Idle, closed or unreadable connections carry no callback.
                let Ok((mut socket, line)) = read else { continue };
                let callback = line.and_then(|line| callback_request_url(&line, redirect));
                let Some(callback) = callback else {
                    // Do not log request targets: they can contain authorization codes.
                    tracing::debug!("Ignored unrelated ChatGPT callback HTTP request");
                    let _ = reply(
                        &mut socket,
                        404,
                        "This is Xylo's sign-in callback. Return to Xylo and start sign-in again.",
                    )
                    .await;
                    invalid += 1;
                    ensure!(invalid < INVALID_REQUESTS, "Too many invalid ChatGPT callbacks");
                    continue;
                };
                match pending.callback(&callback) {
                    Ok(_) => return Ok((socket, callback)),
                    Err(error) => {
                        let _ = reply(&mut socket, 400, "Invalid or declined sign-in callback.").await;
                        if pending.is_matching_denial(&callback) {
                            return Err(error);
                        }
                        invalid += 1;
                        ensure!(invalid < INVALID_REQUESTS, "Too many invalid ChatGPT callbacks");
                    }
                }
            }
        }
    }
}
/// The HTTP request line of one connection; `None` when it is oversized or
/// not UTF-8. An idle or closed connection is an error for that connection only.
async fn request_line(
    socket: tokio::net::TcpStream,
) -> Result<(tokio::net::TcpStream, Option<String>)> {
    let mut input = BufReader::new(socket);
    let mut bytes = Vec::new();
    let length = tokio::time::timeout(
        Duration::from_secs(10),
        (&mut input)
            .take(16 * 1024 + 1)
            .read_until(b'\n', &mut bytes),
    )
    .await
    .context("ChatGPT callback read timed out")??;
    ensure!(length > 0, "ChatGPT callback connection closed");
    let socket = input.into_inner();
    if length > 16 * 1024 {
        return Ok((socket, None));
    }
    Ok((socket, String::from_utf8(bytes).ok()))
}
/// Accept HTTP origin-form and absolute-form targets without relaxing the
/// registered redirect origin/path. OAuth state and code validation follows.
fn callback_request_url(line: &str, redirect: &str) -> Option<String> {
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 3 || fields[0] != "GET" || !matches!(fields[2], "HTTP/1.0" | "HTTP/1.1") {
        return None;
    }
    let expected = url::Url::parse(redirect).ok()?;
    let target = fields[1];
    let callback = if target.starts_with('/') && !target.starts_with("//") {
        url::Url::parse(&format!(
            "{}{target}",
            expected.origin().ascii_serialization()
        ))
        .ok()?
    } else {
        url::Url::parse(target).ok()?
    };
    if callback.origin() != expected.origin()
        || callback.path() != expected.path()
        || !callback.username().is_empty()
        || callback.password().is_some()
        || callback.fragment().is_some()
        || callback.query().is_none()
    {
        return None;
    }
    Some(callback.into())
}

async fn reply(socket: &mut tokio::net::TcpStream, status: u16, text: &str) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        match status {
            200 => "OK",
            404 => "Not Found",
            _ => "Bad Request",
        },
        text.len()
    );
    tokio::time::timeout(
        Duration::from_secs(3),
        socket.write_all(response.as_bytes()),
    )
    .await??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn callback_routes_accept_browser_and_proxy_forms_but_not_other_origins() {
        let redirect = "http://127.0.0.1:60649/auth/callback";
        let query =
            "code=fixture-code&scope=openid+email&state=fixture-state&client_id=oaiapp_fixture";
        for target in [
            format!("/auth/callback?{query}"),
            format!("{redirect}?{query}"),
        ] {
            assert_eq!(
                callback_request_url(&format!("GET {target} HTTP/1.1\r\n"), redirect),
                Some(format!("{redirect}?{query}"))
            );
        }
        for target in [
            "http://evil.example/auth/callback?code=x",
            "http://127.0.0.1:60650/auth/callback?code=x",
            "//evil.example/auth/callback?code=x",
            "/favicon.ico",
            "/auth/callback?code=x#fragment",
            "/callback?code=x",
        ] {
            assert!(callback_request_url(&format!("GET {target} HTTP/1.1"), redirect).is_none());
        }
        assert!(callback_request_url("POST /auth/callback?code=x HTTP/1.1", redirect).is_none());
    }

    #[tokio::test]
    async fn browser_callback_reaches_exchange_after_unrelated_requests() {
        for absolute in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let redirect = format!("http://{address}/auth/callback");
            let pending = PendingChatGptSignIn::begin(
                &format!("urn:uuid:{}", uuid::Uuid::new_v4()),
                &redirect,
                None,
            )
            .unwrap();
            let authorization = url::Url::parse(&pending.authorization_url).unwrap();
            let state = authorization
                .query_pairs()
                .find(|(key, _)| key == "state")
                .unwrap()
                .1
                .into_owned();
            let query = format!(
                "code=fixture-code&scope=openid+email&state={state}&client_id=oaiapp_fixture"
            );
            let expected = format!("{redirect}?{query}");
            let task = tokio::spawn(async move {
                receive_callback(&listener, &pending, &redirect, &CancellationToken::new()).await
            });
            let mut noise = tokio::net::TcpStream::connect(address).await.unwrap();
            noise
                .write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .await
                .unwrap();
            let mut response = [0; 512];
            assert!(noise.read(&mut response).await.unwrap() > 0);
            let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
            let target = if absolute {
                expected.clone()
            } else {
                format!("/auth/callback?{query}")
            };
            socket
                .write_all(format!("GET {target} HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let (_, received) = tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(received, expected);
        }
    }

    #[tokio::test]
    async fn idle_or_closed_preconnects_do_not_block_or_end_sign_in() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let redirect = format!("http://{address}/auth/callback");
        let pending = PendingChatGptSignIn::begin(
            &format!("urn:uuid:{}", uuid::Uuid::new_v4()),
            &redirect,
            None,
        )
        .unwrap();
        let state = url::Url::parse(&pending.authorization_url)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned();
        let task = tokio::spawn(async move {
            receive_callback(&listener, &pending, &redirect, &CancellationToken::new()).await
        });
        // Browser speculative connections: one stays open silently, one closes.
        let _idle = tokio::net::TcpStream::connect(address).await.unwrap();
        drop(tokio::net::TcpStream::connect(address).await.unwrap());
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket
            .write_all(
                format!("GET /auth/callback?code=fixture-code&scope=openid+email&state={state}&client_id=oaiapp_fixture HTTP/1.1\r\nHost: {address}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        // Well before the idle connection's 10 s read deadline.
        let (_, received) = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("an idle connection must not delay the real callback")
            .unwrap()
            .unwrap();
        assert!(received.contains("code=fixture-code"));
    }

    #[tokio::test]
    async fn stale_or_duplicate_state_denials_do_not_abort_the_pending_sign_in() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirect = format!(
            "http://127.0.0.1:{}/auth/callback",
            listener.local_addr().unwrap().port()
        );
        let pending = PendingChatGptSignIn::begin(
            &format!("urn:uuid:{}", uuid::Uuid::new_v4()),
            &redirect,
            None,
        )
        .unwrap();
        let authorization = url::Url::parse(&pending.authorization_url).unwrap();
        let state = authorization
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            receive_callback(&listener, &pending, &redirect, &CancellationToken::new()).await
        });
        for query in [
            "state=wrong&error=access_denied".into(),
            format!("state=wrong&state={state}&error=access_denied"),
        ] {
            let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            socket
                .write_all(
                    format!("GET /auth/callback?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            let mut response = String::new();
            socket.read_to_string(&mut response).await.unwrap();
            assert!(response.starts_with("HTTP/1.1 400"));
            assert!(!task.is_finished());
        }
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        socket.write_all(format!("GET /auth/callback?state={state}&error=access_denied HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes()).await.unwrap();
        let error = task.await.unwrap().err().unwrap();
        assert!(error.to_string().contains("declined"));
    }
    #[tokio::test]
    async fn native_sign_in_binds_callback_before_authorizing_and_cancellation_closes_it() {
        let directory = tempfile::tempdir().unwrap();
        let service = ChatGptSignInService::new(directory.path()).unwrap();
        let vault = CredentialVault::from_test_store(
            "chatgpt-tests",
            keyring_core::mock::Store::new().unwrap(),
        );
        let cancel = CancellationToken::new();
        let run_cancel = cancel.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            service
                .sign_in(
                    &format!("urn:uuid:{}", uuid::Uuid::new_v4()),
                    vault,
                    "fixture-registration",
                    run_cancel,
                    |url| async move {
                        send.send(url).unwrap();
                        Ok(())
                    },
                )
                .await
        });
        let authorization = url::Url::parse(&receive.await.unwrap()).unwrap();
        let redirect = authorization
            .query_pairs()
            .find(|(name, _)| name == "redirect_uri")
            .unwrap()
            .1
            .into_owned();
        let callback = url::Url::parse(&redirect).unwrap();
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", callback.port().unwrap()))
            .await
            .unwrap();
        socket
            .write_all(
                b"GET /auth/callback?state=wrong&code=fake HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            )
            .await
            .unwrap();
        let mut reply = String::new();
        tokio::time::timeout(Duration::from_secs(2), socket.read_to_string(&mut reply))
            .await
            .unwrap()
            .unwrap();
        assert!(reply.starts_with("HTTP/1.1 400"));
        cancel.cancel();
        assert!(task.await.unwrap().is_err());
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", callback.port().unwrap()))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn rotating_token_reference_lease_is_exclusive_and_cancellable() {
        let directory = tempfile::tempdir().unwrap();
        let first = ChatGptSignInService::new(directory.path()).unwrap();
        let second = ChatGptSignInService::new(directory.path()).unwrap();
        let lease = first
            .lease("one-account", &CancellationToken::new())
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let waiting_cancel = cancel.clone();
        let waiting =
            tokio::spawn(async move { second.lease("one-account", &waiting_cancel).await });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        cancel.cancel();
        assert!(waiting.await.unwrap().is_err());
        drop(lease);
        assert!(
            first
                .lease("one-account", &CancellationToken::new())
                .await
                .is_ok()
        );
    }
}

#[cfg(test)]
mod account_resolution_tests {
    use super::*;
    use serde_json::json;

    // These fixtures represent an already validated registration in an isolated
    // keyring. Signature/nonce/audience verification is tested in chatgpt.rs.
    fn fixture(
        scopes: &str,
        expires: i64,
    ) -> (
        tempfile::TempDir,
        ChatGptSignInService,
        CredentialVault,
        ProviderConfig,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let service = ChatGptSignInService::new(directory.path()).unwrap();
        let vault = CredentialVault::from_test_store(
            "chatgpt-resolution-fixture",
            keyring_core::mock::Store::new().unwrap(),
        );
        let record = json!({"type":"chatgpt","client_id":"oaiapp_fixture","subject":"fixture-account","issuer":"https://auth.openai.com","email":"fixture@example.invalid","ext_agent_host_id":"urn:uuid:00000000-0000-4000-8000-000000000000","id_token":"fixture-validated-id","access_token":"fixture-access","refresh_token":null,"expires_at_ms":expires,"scopes":scopes.split_whitespace().collect::<Vec<_>>()});
        vault.set("selected-account", &record.to_string()).unwrap();
        let config = ProviderConfig {
            family: ApiFamily::OpenAiResponses,
            endpoint: "https://api.openai.com/v1".into(),
            credential_ref: Some("selected-account".into()),
            ..Default::default()
        };
        (directory, service, vault, config)
    }

    #[tokio::test]
    async fn selected_authorized_registration_resolves_the_same_bearer_for_catalog_and_inference() {
        let (_directory, service, vault, config) = fixture(
            "openid profile email chatgpt.tokens.use.direct",
            now_ms() + 3_600_000,
        );
        let original = vault.get("selected-account").unwrap().unwrap();
        service
            .prepare_provider_credential(&config, vault.clone(), &CancellationToken::new())
            .await
            .unwrap();
        let resolved = service
            .resolve_provider_credential(&config, vault.clone(), &CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved, original);
        let parsed = ProviderCredential::parse(Some(resolved)).unwrap();
        assert!(matches!(parsed, ProviderCredential::ChatGpt { .. }));
        assert_eq!(
            parsed.headers(&config.family, &config.headers).unwrap()["authorization"],
            "Bearer fixture-access"
        );
    }

    #[tokio::test]
    async fn identity_only_account_remains_visible_but_cannot_authorize_inference() {
        let (_directory, service, vault, config) =
            fixture("openid profile email", now_ms() + 3_600_000);
        let account = service
            .account(vault.clone(), "selected-account")
            .await
            .unwrap()
            .unwrap();
        assert!(account.connected);
        assert!(!account.plan_usage_authorized);
        let error = service
            .resolve_provider_credential(&config, vault.clone(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("plan usage is not authorized"));
        assert!(vault.get("selected-account").unwrap().is_some());
    }

    #[tokio::test]
    async fn expired_nonrenewable_registration_fails_before_any_inference_and_explains_reconnect() {
        let (_directory, service, vault, config) = fixture("chatgpt.tokens.use.direct", 0);
        let error = service
            .resolve_provider_credential(&config, vault, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Reconnect ChatGPT"));
        assert!(!error.to_string().contains("fixture-access"));
    }

    #[tokio::test]
    async fn account_registration_never_resolves_for_a_custom_inference_origin_or_chat_protocol() {
        let (_directory, service, vault, mut config) =
            fixture("chatgpt.tokens.use.direct", now_ms() + 3_600_000);
        config.endpoint = "https://custom.example/v1".into();
        assert!(
            service
                .resolve_provider_credential(&config, vault.clone(), &CancellationToken::new())
                .await
                .unwrap_err()
                .to_string()
                .contains("official public Responses API")
        );
        config.endpoint = "https://api.openai.com/v1".into();
        config.family = ApiFamily::OpenAiChat;
        assert!(
            service
                .resolve_provider_credential(&config, vault, &CancellationToken::new())
                .await
                .is_err()
        );
    }
}
