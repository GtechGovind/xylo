//! Server notifications and explicit, declineable elicitation routed to the active chat.
use super::*;
use rmcp::{
    ClientHandler,
    model::{ClientConfig, ElicitRequestParams, ElicitResult, ElicitationAction, ErrorData},
    service::{NotificationContext, RequestContext},
};
use xylo_core::{EngineEvent, McpFormAction, McpFormRequest};

#[derive(Clone)]
pub(super) struct Route {
    pub events: tokio::sync::mpsc::Sender<EngineEvent>,
    pub cancel: CancellationToken,
}
type PendingForms = HashMap<Uuid, (Value, tokio::sync::oneshot::Sender<ElicitResult>)>;
#[derive(Default)]
pub(super) struct Forms {
    pending: std::sync::Mutex<PendingForms>,
}
impl Forms {
    pub fn answer(&self, id: Uuid, action: McpFormAction, content: Option<Value>) -> Result<()> {
        ensure!(
            content.as_ref().is_none_or(
                |content| serde_json::to_vec(content).is_ok_and(|bytes| bytes.len() <= 32768)
            ),
            "MCP form response is too large"
        );
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| anyhow!("MCP form is unavailable"))?;
        if action == McpFormAction::Accept {
            let (schema, _) = pending.get(&id).context("MCP form is no longer waiting")?;
            xylo_core::validate_mcp_form(schema, content.as_ref().unwrap_or(&Value::Null))
                .map_err(anyhow::Error::msg)?;
        }
        let (_, sender) = pending
            .remove(&id)
            .context("MCP form is no longer waiting")?;
        drop(pending);
        let result = match action {
            McpFormAction::Accept => ElicitResult::new(ElicitationAction::Accept)
                .with_content(content.unwrap_or_else(|| serde_json::json!({}))),
            McpFormAction::Decline => ElicitResult::new(ElicitationAction::Decline),
            McpFormAction::Cancel => ElicitResult::new(ElicitationAction::Cancel),
        };
        sender
            .send(result)
            .map_err(|_| anyhow!("MCP form request ended"))
    }
}
#[derive(Clone)]
pub(super) struct Handler {
    pub store: Store,
    pub config: ConnectorConfig,
    pub fingerprint: String,
    pub route: Arc<std::sync::RwLock<Option<Route>>>,
    pub forms: Arc<Forms>,
    pub revision: Arc<AtomicU64>,
    pub catalog_failure: Arc<AtomicBool>,
    pub refreshing: Arc<AtomicBool>,
    pub notifications: Arc<AtomicU64>,
}
impl ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        let mut info = ClientConfig::default();
        info.capabilities.elicitation = Some(
            rmcp::model::ElicitationCapability::new()
                .with_form(rmcp::model::FormElicitationCapability::new()),
        );
        info
    }
    async fn on_tool_list_changed(&self, context: NotificationContext<RoleClient>) {
        // Revoke authority before scheduling network work, even if the server's
        // next tools/list errors or never replies. Persistence also fences restart.
        let invalidated = invalidate_catalog(&self.store, self.config.id);
        self.revision.fetch_add(1, Ordering::AcqRel);
        self.notifications.fetch_add(1, Ordering::AcqRel);
        if invalidated.is_err() {
            self.refresh_failed();
            // No call may rely on a transition that could not be persisted.
            self.catalog_failure.store(true, Ordering::Release);
            return;
        }
        if self.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let handler = self.clone();
        tokio::spawn(async move {
            loop {
                let observed = handler.notifications.load(Ordering::Acquire);
                let list = async {
                    let generation = catalog_generation(&handler.store, handler.config.id)?;
                    let mut cursor = None;
                    let mut seen = std::collections::HashSet::new();
                    let mut tools = Vec::new();
                    loop {
                        let page = context
                            .peer
                            .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                            .await?;
                        for tool in page.tools {
                            tools.push(ToolDefinition {
                                name: tool.name.to_string(),
                                description: tool
                                    .description
                                    .as_deref()
                                    .unwrap_or_default()
                                    .to_owned(),
                                parameters: serde_json::to_value(&tool.input_schema)?,
                                writes: tool_writes(tool.annotations.as_ref()),
                            });
                        }
                        ensure!(
                            tools.len() <= 512
                                && serde_json::to_vec(&tools)?.len() <= 2 * 1024 * 1024,
                            "Connector catalog exceeds discovery budget"
                        );
                        cursor = page.next_cursor;
                        if let Some(cursor) = &cursor {
                            ensure!(
                                seen.insert(cursor.clone()) && seen.len() <= 128,
                                "MCP pagination repeats"
                            );
                        } else {
                            break;
                        }
                    }
                    Ok::<_, anyhow::Error>((tools, generation))
                };
                let mut saved_latest = false;
                if let Ok(Ok((tools, generation))) =
                    tokio::time::timeout(DISCOVERY_TIMEOUT, list).await
                {
                    let store = handler.store.clone();
                    let id = handler.config.id;
                    let fingerprint = handler.fingerprint.clone();
                    if let Ok(json) = serde_json::to_string(&tools) {
                        let saved = tokio::task::spawn_blocking(move || {
                            save_catalog_generation(&store, id, fingerprint, json, generation)
                        })
                        .await;
                        if matches!(saved, Ok(Ok(()))) {
                            handler.revision.fetch_add(1, Ordering::AcqRel);
                            saved_latest = true;
                        }
                    }
                }
                if !saved_latest && handler.notifications.load(Ordering::Acquire) == observed {
                    handler.refresh_failed();
                }
                handler.refreshing.store(false, Ordering::Release);
                if handler.notifications.load(Ordering::Acquire) == observed
                    || handler.refreshing.swap(true, Ordering::AcqRel)
                {
                    break;
                }
            }
        });
    }
    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        let decline = || ElicitResult::new(ElicitationAction::Decline);
        let ElicitRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } = request
        else {
            return Ok(decline());
        };
        let Some(route) = self.route.read().ok().and_then(|route| route.clone()) else {
            return Ok(decline());
        };
        let Ok(schema) = serde_json::to_value(requested_schema) else {
            return Ok(decline());
        };
        if message.len() > 8192
            || !serde_json::to_vec(&schema).is_ok_and(|bytes| bytes.len() <= 32768)
        {
            return Ok(decline());
        }
        let Some(properties) = schema["properties"]
            .as_object()
            .filter(|properties| properties.len() <= 20)
        else {
            return Ok(decline());
        };
        if properties.iter().any(|(name, property)| {
            let name = name.to_ascii_lowercase();
            ["password", "secret", "token", "credential"]
                .iter()
                .any(|word| name.contains(word))
                || !matches!(
                    property["type"].as_str(),
                    Some("string" | "number" | "integer" | "boolean")
                )
        }) {
            return Ok(decline());
        }
        let id = Uuid::new_v4();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        if let Ok(mut pending) = self.forms.pending.lock() {
            if pending.len() >= 8 {
                return Ok(decline());
            }
            pending.insert(id, (schema.clone(), sender));
        } else {
            return Ok(decline());
        }
        let request = McpFormRequest {
            id,
            server_id: self.config.id,
            server_name: self.config.name.clone(),
            message,
            schema,
        };
        let answer = async {
            route
                .events
                .send(EngineEvent::McpElicitation { request })
                .await
                .ok()?;
            receiver.await.ok()
        };
        let response = tokio::select! {
            _ = route.cancel.cancelled() => None,
            _ = context.ct.cancelled() => None,
            answer = tokio::time::timeout(Duration::from_secs(300), answer) => answer.ok().flatten(),
        };
        if let Ok(mut pending) = self.forms.pending.lock() {
            pending.remove(&id);
        }
        let _ = route
            .events
            .send(EngineEvent::McpElicitationClosed { request_id: id })
            .await;
        Ok(response.unwrap_or_else(|| ElicitResult::new(ElicitationAction::Cancel)))
    }
}

impl Handler {
    /// At most one bounded, content-free notice per failed coalesced refresh.
    fn refresh_failed(&self) {
        if let Some(route) = self.route.read().ok().and_then(|route| route.clone()) {
            let _ = route.events.try_send(EngineEvent::Status { text: "An MCP server changed its tools, but refresh failed. Its tools are unavailable; rediscover them in Connections or with connector_tools.".into() });
        }
    }
}
