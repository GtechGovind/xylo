//! Deferred MCP resources and prompts with bounded pagination and durable prompt discovery.
use super::*;
use rmcp::model::{GetPromptRequestParams, ReadResourceRequestParams};

impl ConnectorBroker {
    /// Reads resource or prompt metadata/content after the caller's normal permission decision.
    pub async fn metadata(
        &self,
        config: &ConnectorConfig,
        method: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let _permit = tokio::select! { _ = cancel.cancelled() => bail!("Connector request cancelled"), permit = self.pool.capacity.acquire() => permit? };
        let slot = self.session(config).await?;
        let mut session = tokio::select! { _ = cancel.cancelled() => bail!("Connector request cancelled"), session = slot.lock() => session };
        self.ensure_connected(&mut session, config, cancel, self.local_scope.as_ref())
            .await?;
        let operation = async {
            let client = session.client.as_ref().context("MCP client unavailable")?;
            match method {
                "resources/list" | "prompts/list" => {
                    let mut items = Vec::new();
                    let mut cursors = std::collections::HashSet::new();
                    let mut cursor = None;
                    loop {
                        let params = Some(PaginatedRequestParams::default().with_cursor(cursor));
                        let (page, next) = if method == "resources/list" {
                            let page = client.list_resources(params).await?;
                            (
                                page.resources
                                    .into_iter()
                                    .map(serde_json::to_value)
                                    .collect::<Result<Vec<_>, _>>()?,
                                page.next_cursor,
                            )
                        } else {
                            let page = client.list_prompts(params).await?;
                            (
                                page.prompts
                                    .into_iter()
                                    .map(serde_json::to_value)
                                    .collect::<Result<Vec<_>, _>>()?,
                                page.next_cursor,
                            )
                        };
                        items.extend(page);
                        ensure!(
                            items.len() <= 512
                                && serde_json::to_vec(&items)?.len() <= 2 * 1024 * 1024,
                            "MCP metadata exceeds discovery budget"
                        );
                        cursor = next;
                        if let Some(cursor) = &cursor {
                            ensure!(
                                cursors.insert(cursor.clone()) && cursors.len() <= 128,
                                "MCP pagination repeats or exceeds its limit"
                            );
                        } else {
                            break;
                        }
                    }
                    Ok(Value::Array(items))
                }
                "resources/read" => Ok(serde_json::to_value(
                    client
                        .read_resource(ReadResourceRequestParams::new(
                            arguments["uri"].as_str().context("Resource URI required")?,
                        ))
                        .await?,
                )?),
                "prompts/get" => {
                    let name = arguments["name"].as_str().context("Prompt name required")?;
                    let mut params = GetPromptRequestParams::new(name);
                    if let Some(args) = arguments["arguments"].as_object() {
                        params = params.with_arguments(args.clone());
                    }
                    Ok(serde_json::to_value(client.get_prompt(params).await?)?)
                }
                _ => bail!("Unsupported MCP metadata operation"),
            }
        };
        let result: Result<Value> = tokio::select! { _ = cancel.cancelled() => Err(anyhow!("Connector request cancelled")), result = tokio::time::timeout(CALL_TIMEOUT, operation) => result.context("Connector metadata request timed out").and_then(|result| result) };
        if result.is_err() {
            let _ = session.close().await;
        }
        let result = result?;
        ensure!(
            serde_json::to_vec(&result)?.len() <= HTTP_BODY_LIMIT,
            "MCP content exceeds response budget"
        );
        session.touched = Instant::now();
        if method == "prompts/list" {
            let store = self.pool.store.clone();
            let id = config.id.to_string();
            let fingerprint = Self::fingerprint(config, self.local_scope.as_ref())?;
            let encoded = serde_json::to_string(&result)?;
            tokio::task::spawn_blocking(move || store.call(move |conn| { conn.execute("INSERT INTO connector_prompts(id,fingerprint,json) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET fingerprint=excluded.fingerprint,json=excluded.json", rusqlite::params![id, fingerprint, encoded])?; Ok(()) })).await??;
        }
        Ok(result)
    }
    /// Lists only previously discovered prompt commands; this never starts a server.
    pub fn cached_prompts(&self, config: &ConnectorConfig) -> Result<Value> {
        use rusqlite::OptionalExtension;
        let id = config.id.to_string();
        let fingerprint = Self::fingerprint(config, self.local_scope.as_ref())?;
        self.pool.store.call(move |conn| {
            let json: Option<String> = conn
                .query_row(
                    "SELECT json FROM connector_prompts WHERE id=?1 AND fingerprint=?2",
                    rusqlite::params![id, fingerprint],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(json
                .map(|json| serde_json::from_str(&json))
                .transpose()?
                .unwrap_or_else(|| Value::Array(Vec::new())))
        })
    }
}
