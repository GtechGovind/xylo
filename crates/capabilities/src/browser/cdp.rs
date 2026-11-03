//! Serialized CDP for the bundled runtime's authenticated loopback socket.
//! No public endpoint or token-in-URL configuration is exposed.
use anyhow::{Context, Result, ensure};
use async_tungstenite::{
    WebSocketStream,
    tokio::ConnectStream,
    tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig},
};
use futures_util::StreamExt;
use serde_json::{Value, json};

pub(super) struct Cdp {
    socket: WebSocketStream<ConnectStream>,
    next_id: u64,
    session: Option<String>,
}

impl Cdp {
    pub(super) async fn connect(endpoint: &str, token: &str) -> Result<Self> {
        let mut request = endpoint.into_client_request()?;
        let mut authorization = format!("Bearer {token}")
            .parse::<async_tungstenite::tungstenite::http::HeaderValue>()?;
        authorization.set_sensitive(true);
        request.headers_mut().insert("Authorization", authorization);
        let config = WebSocketConfig::default()
            .max_message_size(Some(16 * 1024 * 1024))
            .max_frame_size(Some(16 * 1024 * 1024));
        // Errors must not format the request: it contains authentication.
        let (socket, _) =
            async_tungstenite::tokio::connect_async_with_config(request, Some(config))
                .await
                .map_err(|_| {
                    anyhow::anyhow!("Could not authenticate the bundled Obscura CDP connection")
                })?;
        Ok(Self {
            socket,
            next_id: 0,
            session: None,
        })
    }

    pub(super) async fn initialize(&mut self) -> Result<()> {
        let target = self
            .call("Target.createTarget", json!({"url":"about:blank"}))
            .await?;
        let target = target["targetId"]
            .as_str()
            .context("Obscura omitted targetId")?;
        let attached = self
            .call(
                "Target.attachToTarget",
                json!({
                    "targetId":target, "flatten":true
                }),
            )
            .await?;
        self.session = Some(
            attached["sessionId"]
                .as_str()
                .context("Obscura omitted sessionId")?
                .to_owned(),
        );
        self.call(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width":1280, "height":900, "deviceScaleFactor":1, "mobile":false
            }),
        )
        .await?;
        Ok(())
    }

    pub(super) async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("CDP request ID exhausted")?;
        let id = self.next_id;
        let mut request = json!({"id":id,"method":method,"params":params});
        if let Some(session) = &self.session {
            request["sessionId"] = json!(session);
        }
        self.socket
            .send(Message::Text(request.to_string().into()))
            .await?;
        loop {
            let message = self.socket.next().await.context("Obscura disconnected")??;
            match message {
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(&text)?;
                    if value["id"].as_u64() != Some(id) {
                        continue;
                    }
                    ensure!(
                        value.get("error").is_none(),
                        "Obscura {method} failed: {}",
                        value["error"]
                    );
                    return value
                        .get("result")
                        .cloned()
                        .context("Obscura omitted CDP result");
                }
                Message::Ping(bytes) => {
                    self.socket.send(Message::Pong(bytes)).await?;
                }
                Message::Close(_) => anyhow::bail!("Obscura closed its CDP connection"),
                _ => {}
            }
        }
    }

    pub(super) async fn evaluate(&mut self, expression: String) -> Result<Value> {
        let result = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression":expression, "returnByValue":true, "timeout":10000
                }),
            )
            .await?;
        ensure!(
            result.get("exceptionDetails").is_none(),
            "Browser script failed: {}",
            result["exceptionDetails"]
        );
        result["result"]
            .get("value")
            .cloned()
            .context("Browser script did not return a value")
    }
}
