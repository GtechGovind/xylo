//! User-initiated MCP browser sign-in with a bounded, one-use loopback callback.
use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

impl HostState {
    pub(super) async fn connector_sign_in(
        &self,
        request_id: Uuid,
        id: Uuid,
        client_id: Option<String>,
    ) -> Result<()> {
        let cancel = CancellationToken::new();
        self.capability_requests
            .lock()
            .await
            .insert(request_id, cancel.clone());
        let result = self
            .connector_oauth(request_id, id, client_id.as_deref(), &cancel)
            .await
            .map_err(|error| error.to_string());
        self.capability_requests.lock().await.remove(&request_id);
        self.events
            .send(HostEvent::ConnectorMutation { request_id, result })
            .await?;
        Ok(())
    }
    async fn connector_oauth(
        &self,
        request_id: Uuid,
        id: Uuid,
        client_id: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<ConnectorMutationReceipt> {
        let connectors = self.connectors()?;
        let _guard = connectors.configuration_guard(id).await?;
        let store = self.store.clone();
        let mut config = tokio::task::spawn_blocking(move || store.connectors())
            .await??
            .into_iter()
            .find(|config| config.id == id)
            .context("Connection no longer exists")?;
        ensure!(
            matches!(
                config.transport.as_str(),
                "http" | "streamable_http" | "sse"
            ),
            "Browser sign-in requires a remote MCP connection"
        );
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let redirect = format!(
            "http://127.0.0.1:{}/callback",
            listener.local_addr()?.port()
        );
        let oauth =
            xylo_services::discover_oauth(&config.endpoint, &redirect, client_id, cancel).await?;
        let pending = xylo_services::PendingOAuth::begin(oauth.clone(), &redirect)?;
        self.events
            .send(HostEvent::ConnectorAuthorization {
                request_id,
                url: pending.authorization_url.clone(),
            })
            .await?;
        let callback = async {
            let (socket, peer) = listener.accept().await?;
            ensure!(peer.ip().is_loopback(), "Sign-in callback is not local");
            let (read, mut write) = socket.into_split();
            let mut read = tokio::io::BufReader::new(read).take(8192);
            let mut line = String::new();
            read.read_line(&mut line).await?;
            let target = line
                .strip_prefix("GET ")
                .and_then(|line| line.split_once(' '))
                .map(|(path, _)| path)
                .context("Invalid sign-in callback")?;
            let callback = url::Url::parse(&format!("http://127.0.0.1{target}"))?;
            ensure!(
                callback.path() == "/callback",
                "Unexpected sign-in callback path"
            );
            let pairs = callback
                .query_pairs()
                .collect::<std::collections::HashMap<_, _>>();
            ensure!(
                !pairs.contains_key("error"),
                "Sign-in was declined by the authorization server"
            );
            let code = pairs.get("code").context("Sign-in callback has no code")?;
            let state = pairs
                .get("state")
                .context("Sign-in callback has no state")?;
            let tokens = pending.complete(code, state, cancel).await?;
            let body = "Signed in. You can close this window and return to Xylo.";
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = write.write_all(reply.as_bytes()).await;
            Ok::<_, anyhow::Error>(tokens)
        };
        let tokens = tokio::select! { _ = cancel.cancelled() => anyhow::bail!("Sign-in cancelled"), result = tokio::time::timeout(Duration::from_secs(600), callback) => result.context("Sign-in timed out")?? };
        let reference = format!("connector-oauth-{id}-{}", Uuid::new_v4());
        let record = serde_json::to_string(&xylo_services::ConnectorOAuthCredential {
            config: oauth,
            tokens,
        })?;
        let vault = self.vault()?;
        let key = reference.clone();
        tokio::task::spawn_blocking(move || vault.set(&key, &record)).await??;
        config.credential_ref = Some(reference);
        let store = self.store.clone();
        let saved = config.clone();
        tokio::task::spawn_blocking(move || store.save_connector(&saved)).await??;
        connectors.disconnect(id).await?;
        Ok(ConnectorMutationReceipt {
            saved: Some(config),
            removed: None,
        })
    }
}
