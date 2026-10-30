//! Hydrate managed images for one provider request, never for checkpoints/events.
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{StreamExt, stream::BoxStream};
use hermes_core::{
    AgentError, LlmProvider, LlmResponse, Message, MessageRole, StreamChunk, ToolSchema,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_core::TurnRequest;
use xylo_services::attachments::AttachmentStore;

pub(crate) const IMAGE_MARKER: &str = "xylo_image_attachment";
const MAX_REQUEST_IMAGE_BYTES: usize = 16 * 1024 * 1024;

pub(crate) fn user_message(request: &TurnRequest) -> Result<Message, AgentError> {
    let mut message = Message::user(&request.prompt);
    if !request.attachments.is_empty() {
        message.anthropic_content_blocks = Some(
            request
                .attachments
                .iter()
                .map(|image| json!({"type":IMAGE_MARKER,"item":image}))
                .collect(),
        );
    }
    Ok(message)
}

pub(crate) struct AttachmentProvider {
    inner: Arc<dyn LlmProvider>,
    store: AttachmentStore,
    session: Uuid,
    cancel: CancellationToken,
}
impl AttachmentProvider {
    pub(crate) fn new(
        inner: Arc<dyn LlmProvider>,
        store: AttachmentStore,
        session: Uuid,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            inner,
            store,
            session,
            cancel,
        }
    }
    async fn prepare(&self, messages: &[Message]) -> Result<Vec<Message>, AgentError> {
        let messages = messages.to_vec();
        let store = self.store.clone();
        let session = self.session;
        let cancel = self.cancel.clone();
        tokio::task::spawn_blocking(move||{
            let mut messages=messages;let mut encoded_size=0usize;let mut count=0;
            for message in &mut messages {
                if cancel.is_cancelled(){return Err(AgentError::Config("Image request cancelled".into()));}
                let images=message.anthropic_content_blocks.as_ref().into_iter().flatten().filter(|part|part["type"]==IMAGE_MARKER).collect::<Vec<_>>();
                if images.is_empty(){continue;}
                if message.role!=MessageRole::User{return Err(AgentError::Config("Only user messages may contain images".into()));}
                let mut parts=vec![json!({"type":"text","text":message.content.as_deref().unwrap_or("")})];
                for part in images {
                    count+=1;
                    if count>16{return Err(AgentError::Config("This conversation exceeds the 16-image provider budget. Start a new chat to attach more images.".into()));}
                    let image=serde_json::from_value(part["item"].clone()).map_err(|_|AgentError::Config("Invalid managed image reference".into()))?;
                    let image=store.read_provider_image(session,&image).map_err(|error|AgentError::Config(error.to_string()))?;
                    encoded_size=encoded_size.saturating_add(image.bytes.len().div_ceil(3)*4);
                    if encoded_size>MAX_REQUEST_IMAGE_BYTES{return Err(AgentError::Config("This conversation exceeds the 16 MiB image transmission budget. Start a new chat or attach smaller images.".into()));}
                    parts.push(json!({"type":"image_url","image_url":{"url":format!("data:{};base64,{}",image.media_type,STANDARD.encode(image.bytes))}}));
                }
                message.content=Some(format!("__hermes_acp_parts_json__:{}",Value::Array(parts)));
            }
            Ok(messages)
        }).await.map_err(|error|AgentError::Config(format!("Image preparation failed: {error}")))?
    }
}
#[async_trait]
impl LlmProvider for AttachmentProvider {
    async fn chat_completion(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra: Option<&Value>,
    ) -> Result<LlmResponse, AgentError> {
        let messages = self.prepare(messages).await?;
        self.inner
            .chat_completion(&messages, tools, max_tokens, temperature, model, extra)
            .await
    }
    fn chat_completion_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra: Option<&Value>,
    ) -> BoxStream<'static, Result<StreamChunk, AgentError>> {
        let provider = Self::new(
            self.inner.clone(),
            self.store.clone(),
            self.session,
            self.cancel.clone(),
        );
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        let model = model.map(str::to_owned);
        let extra = extra.cloned();
        Box::pin(async_stream::try_stream! {
            let messages=provider.prepare(&messages).await?;
            let mut stream=provider.inner.chat_completion_stream(&messages,&tools,max_tokens,temperature,model.as_deref(),extra.as_ref());
            while let Some(chunk)=stream.next().await{yield chunk?;}
        })
    }
}
