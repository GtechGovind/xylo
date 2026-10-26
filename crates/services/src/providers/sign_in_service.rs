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
use workbench_core::{ApiFamily, ProviderConfig, now_ms};

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
        let record = serde_json::to_string(&renewed)?;
        Self::save(vault, reference.into(), renewed).await?;
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

async fn receive_callback(
    listener: &tokio::net::TcpListener,
    pending: &PendingChatGptSignIn,
    redirect: &str,
    cancel: &CancellationToken,
) -> Result<(tokio::net::TcpStream, String)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    for _ in 0..32 {
        let (socket, peer) = tokio::select! {_ = cancel.cancelled()=>bail!("ChatGPT sign-in cancelled"),_ = tokio::time::sleep_until(deadline)=>bail!("ChatGPT sign-in timed out"),accepted=listener.accept()=>accepted?};
        ensure!(peer.ip().is_loopback(), "ChatGPT callback must be local");
        let mut input = BufReader::new(socket);
        let mut bytes = Vec::new();
        let mut limited = (&mut input).take(16 * 1024 + 1);
        let length = tokio::select! {_ = cancel.cancelled()=>bail!("ChatGPT sign-in cancelled"),read=tokio::time::timeout(Duration::from_secs(10),limited.read_until(b'\n',&mut bytes))=>read.context("ChatGPT callback read timed out")??};
        let mut socket = input.into_inner();
        if length > 16 * 1024 {
            reply(&mut socket, 400, "Invalid callback.").await?;
            continue;
        }
        let Ok(line) = String::from_utf8(bytes) else {
            let _ = reply(&mut socket, 400, "Invalid callback.").await;
            continue;
        };
        let Some(callback) = callback_request_url(&line, redirect) else {
            // Do not log request targets: they can contain authorization codes.
            tracing::debug!("Ignored unrelated ChatGPT callback HTTP request");
            let _ = reply(
                &mut socket,
                404,
                "This is Xylo's sign-in callback. Return to Xylo and start sign-in again.",
            )
            .await;
            continue;
        };
        match pending.callback(&callback) {
            Ok(_) => return Ok((socket, callback)),
            Err(error) => {
                reply(&mut socket, 400, "Invalid or declined sign-in callback.").await?;
                if pending.is_matching_denial(&callback) {
                    return Err(error);
                }
            }
        }
    }
    bail!("Too many invalid ChatGPT callbacks")
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
