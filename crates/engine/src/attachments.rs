//! Hydrates managed images for one provider request. Checkpoints and events
//! only ever carry the managed references in [`Message::attachments`].
use crate::model::{
    EventStream, InlineImage, Message, ModelError, ModelProvider, ModelRequest, Role,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_core::TurnRequest;
use xylo_services::attachments::AttachmentStore;

/// Images one request may carry, by count and encoded size.
const MAX_REQUEST_IMAGES: usize = 16;
const MAX_REQUEST_IMAGE_BYTES: usize = 16 * 1024 * 1024;

/// The durable user message for a turn, with its managed image references.
pub(crate) fn user_message(request: &TurnRequest) -> Message {
    let mut message = Message::user(&request.prompt);
    message.attachments = request.attachments.clone();
    message
}

pub(crate) struct AttachmentProvider {
    inner: Arc<dyn ModelProvider>,
    store: AttachmentStore,
    session: Uuid,
}

impl AttachmentProvider {
    pub(crate) fn new(
        inner: Arc<dyn ModelProvider>,
        store: AttachmentStore,
        session: Uuid,
    ) -> Self {
        Self {
            inner,
            store,
            session,
        }
    }
}

/// Reads every referenced image of `messages` into [`Message::images`].
fn hydrate(
    store: &AttachmentStore,
    session: Uuid,
    messages: &mut [Message],
    cancel: &CancellationToken,
) -> Result<(), ModelError> {
    let mut encoded_size = 0usize;
    let mut count = 0;
    for message in messages.iter_mut().filter(|m| !m.attachments.is_empty()) {
        if message.role != Role::User {
            return Err(ModelError::InvalidRequest(
                "Only user messages may contain images".into(),
            ));
        }
        for attachment in &message.attachments {
            if cancel.is_cancelled() {
                return Err(ModelError::Interrupted);
            }
            count += 1;
            if count > MAX_REQUEST_IMAGES {
                return Err(ModelError::InvalidRequest("This conversation exceeds the 16-image provider budget. Start a new chat to attach more images.".into()));
            }
            let image = store
                .read_provider_image(session, attachment)
                .map_err(|error| ModelError::InvalidRequest(error.to_string()))?;
            encoded_size = encoded_size.saturating_add(image.bytes.len().div_ceil(3) * 4);
            if encoded_size > MAX_REQUEST_IMAGE_BYTES {
                return Err(ModelError::InvalidRequest("This conversation exceeds the 16 MiB image transmission budget. Start a new chat or attach smaller images.".into()));
            }
            message.images.push(InlineImage {
                media_type: image.media_type,
                data: STANDARD.encode(image.bytes),
            });
        }
    }
    Ok(())
}

impl ModelProvider for AttachmentProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> EventStream {
        let inner = self.inner.clone();
        let store = self.store.clone();
        let session = self.session;
        Box::pin(async_stream::try_stream! {
            let mut request = request;
            if request.messages.iter().any(|m| !m.attachments.is_empty()) {
                let hydrate_cancel = cancel.clone();
                request = tokio::task::spawn_blocking(move || {
                    hydrate(&store, session, &mut request.messages, &hydrate_cancel).map(|()| request)
                })
                .await
                .map_err(|error| ModelError::Io(format!("Image preparation failed: {error}")))??;
            }
            let mut stream = inner.stream(request, cancel);
            while let Some(event) = stream.next().await {
                yield event?;
            }
        })
    }
}
