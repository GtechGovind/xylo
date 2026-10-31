use super::{
    ChatGptCredential, PendingChatGptSignIn, ProviderCredential, RefreshedCredential,
    normalize_provider, refresh_chatgpt_credential, revoke_chatgpt_credential,
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
        let path = self.locks.join(format!(
            "{}.lock",
            hex::encode(Sha256::digest(reference.as_bytes()))
        ));
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
    fn commit_under_lease<F>(lease: File, commit: F) -> tokio::task::JoinHandle<Result<()>>
    where
        F: FnOnce() -> Result<()> + Send + 'static,
    {
        tokio::task::spawn_blocking(move || {
            let _lease = lease;
            commit()
        })
    }
    /// Persists what a refresh the authority answered left behind, and
    /// returns the renewed credential, or the error that made the refresh
    /// keep only the rotated refresh token (after that token is saved).
    async fn persist_refreshed(
        vault: CredentialVault,
        reference: String,
        refreshed: RefreshedCredential,
        lease: File,
    ) -> Result<ChatGptCredential> {
        let RefreshedCredential { credential, error } = refreshed;
        Self::save_rotated(vault, reference, credential.clone(), lease).await?;
        match error {
            Some(error) => Err(error.context(
                "ChatGPT refresh returned tokens that failed validation; the new refresh token was kept for the next attempt",
            )),
            None => Ok(credential),
        }
    }
    async fn save_rotated(
        vault: CredentialVault,
        reference: String,
        credential: ChatGptCredential,
        lease: File,
    ) -> Result<()> {
        // A dropped/timeout waiter must not release coordination while the OS
        // credential write is still running on the blocking executor.
        Self::commit_under_lease(lease, move || credential.save(&vault, &reference)).await?
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
        // As in `refresh_rejected_with`: once the token request may have
        // been sent, the commit runs on its own task and cannot be dropped.
        let (reference, cancel) = (reference.to_owned(), cancel.clone());
        let renewed = tokio::spawn(async move {
            let refreshed = refresh_chatgpt_credential(&credential, &cancel).await?;
            Self::persist_refreshed(vault, reference, refreshed, _lease).await
        })
        .await??;
        let authorized = renewed.plan_usage_authorized();
        let record = serde_json::to_string(&renewed)?;
        ensure!(
            authorized,
            "ChatGPT plan usage is not authorized; reconnect and allow plan usage"
        );
        Ok(Some(record))
    }
    /// Renew only the selected ChatGPT registration rejected by the provider.
    pub async fn refresh_rejected_provider_credential(
        &self,
        config: &ProviderConfig,
        vault: CredentialVault,
        rejected_access_token_sha256: &str,
        cancel: &CancellationToken,
    ) -> Result<String> {
        self.refresh_rejected_with(config, vault, rejected_access_token_sha256, cancel,
            |credential, cancel| async move { refresh_chatgpt_credential(&credential, &cancel).await }).await
    }
    async fn refresh_rejected_with<F, Fut>(
        &self,
        config: &ProviderConfig,
        vault: CredentialVault,
        rejected_access_token_sha256: &str,
        cancel: &CancellationToken,
        refresh: F,
    ) -> Result<String>
    where
        F: FnOnce(ChatGptCredential, CancellationToken) -> Fut,
        Fut: Future<Output = Result<RefreshedCredential>> + Send + 'static,
    {
        ensure!(!cancel.is_cancelled(), "ChatGPT token refresh cancelled");
        ensure!(
            rejected_access_token_sha256.len() == 64
                && rejected_access_token_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "Invalid rejected credential fingerprint"
        );
        let reference = config
            .credential_ref
            .as_deref()
            .context("Provider credential is not configured")?;
        let endpoint = normalize_provider(config)?;
        ensure!(
            config.family == ApiFamily::OpenAiResponses
                && endpoint.base_url.as_str().trim_end_matches('/') == "https://api.openai.com/v1",
            "ChatGPT account access requires the official public Responses API"
        );
        let _lease = self.lease(reference, cancel).await?;
        let record=tokio::select! { biased; _=cancel.cancelled()=>bail!("ChatGPT token refresh cancelled"), record=Self::load(vault.clone(),reference.into())=>record? }.context("Provider credential is not configured")?;
        let value: serde_json::Value = serde_json::from_str(&record)
            .context("The selected credential is not a ChatGPT registration")?;
        ensure!(
            value["type"] == "chatgpt",
            "The selected credential is not a ChatGPT registration"
        );
        let credential: ChatGptCredential = serde_json::from_value(value)
            .context("The selected credential is not a ChatGPT registration")?;
        ensure!(
            !credential.access_token.is_empty(),
            "Reconnect ChatGPT; this account is signed out or unavailable"
        );
        ensure!(
            credential.plan_usage_authorized(),
            "ChatGPT plan usage is not authorized; reconnect and allow plan usage"
        );
        let current_digest = hex::encode(Sha256::digest(credential.access_token.as_bytes()));
        if !current_digest.eq_ignore_ascii_case(rejected_access_token_sha256)
            && credential.expires_at_ms > now_ms() + 60_000
        {
            ensure!(!cancel.is_cancelled(), "ChatGPT token refresh cancelled");
            return Ok(record);
        }
        ensure!(
            credential
                .refresh_token
                .as_deref()
                .is_some_and(|token| !token.is_empty()),
            "Reconnect ChatGPT; this account has no renewable session"
        );
        // The refresh honours `cancel` only until it sends the token request
        // (its HTTP client bounds the rest). From then on the authority may
        // have rotated the refresh token, so the exchange, its checks and the
        // durable replacement run on their own task: neither cancellation nor
        // a dropped caller can lose the rotated token. Sign-out holds the same lease and follows
        // this commit; no cancelled request receives the renewed credential.
        let exchange = refresh(credential.clone(), cancel.clone());
        let reference = reference.to_owned();
        let commit = tokio::spawn(async move {
            let refreshed = exchange.await?;
            ensure!(
                refreshed.error.is_some()
                    || (serde_json::to_value(&refreshed.credential)?["type"] == "chatgpt"
                        && refreshed.credential.subject == credential.subject
                        && refreshed.credential.issuer == credential.issuer
                        && refreshed.credential.client_id == credential.client_id
                        && refreshed.credential.ext_agent_host_id == credential.ext_agent_host_id
                        && !refreshed.credential.access_token.is_empty()),
                "ChatGPT refresh returned a different account registration"
            );
            Self::persist_refreshed(vault, reference, refreshed, _lease).await
        });
        // `refresh` stops promptly on cancellation until it sends the
        // request; after that, waiting for the commit is what keeps a
        // cancelled caller from racing a sign-out past the rotated tokens.
        let renewed = commit.await??;
        let authorized = renewed.plan_usage_authorized();
        let usable = renewed.expires_at_ms > now_ms() + 30_000;
        let record = serde_json::to_string(&renewed)?;
        ensure!(
            authorized,
            "ChatGPT plan usage is not authorized; reconnect and allow plan usage"
        );
        ensure!(
            usable,
            "ChatGPT refreshed credential expires too soon; reconnect before retrying"
        );
        ensure!(!cancel.is_cancelled(), "ChatGPT token refresh cancelled");
        Ok(record)
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
mod tests;

#[cfg(test)]
mod account_resolution_tests;
