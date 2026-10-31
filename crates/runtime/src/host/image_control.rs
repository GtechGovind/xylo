//! Native image operations. Provider secrets are resolved only inside the host;
//! receipts preserve uncertain outcomes rather than retrying an external write.
use super::*;
use xylo_services::image_generation::ImageGenerationService;

pub(super) enum ImageCommand {
    Bytes(Uuid, Uuid, Vec<u8>, AttachmentSource),
    Generate(Uuid, ImageGenerationRequest),
    Receipt(Uuid, Uuid),
    Provider(Uuid),
    History(Uuid),
    SaveProvider(Uuid, ImageGenerationProvider, Option<String>),
}
impl ImageCommand {
    pub(super) fn cancellable_id(&self) -> Option<Uuid> {
        match self {
            Self::Bytes(id, ..) | Self::Generate(id, ..) => Some(*id),
            _ => None,
        }
    }
    pub(super) fn refused(&self, message: &str) -> HostEvent {
        match self {
            Self::Bytes(id, session_id, ..) => HostEvent::ImageImported {
                request_id: *id,
                session_id: *session_id,
                result: Err(message.into()),
            },
            Self::Generate(id, ..) | Self::Receipt(id, ..) => HostEvent::ImageGeneration {
                request_id: *id,
                result: Err(message.into()),
            },
            Self::History(id) => HostEvent::ImageGenerationHistory {
                request_id: *id,
                result: Err(message.into()),
            },
            Self::Provider(id) | Self::SaveProvider(id, ..) => HostEvent::ImageGenerationProvider {
                request_id: *id,
                result: Err(message.into()),
            },
        }
    }
}
impl HostClient {
    pub fn import_image_bytes(
        &self,
        request_id: Uuid,
        session_id: Uuid,
        bytes: Vec<u8>,
        source: AttachmentSource,
    ) -> Result<()> {
        ensure!(
            !bytes.is_empty() && bytes.len() as u64 <= MAX_IMAGE_BYTES,
            "Image must contain at most 8 MiB"
        );
        self.enqueue(HostCommand::Image(ImageCommand::Bytes(
            request_id, session_id, bytes, source,
        )))
    }
    pub fn generate_image(&self, id: Uuid, request: ImageGenerationRequest) -> Result<()> {
        self.enqueue(HostCommand::Image(ImageCommand::Generate(id, request)))
    }
    pub fn image_generation_receipt(&self, request_id: Uuid, id: Uuid) -> Result<()> {
        self.enqueue(HostCommand::Image(ImageCommand::Receipt(request_id, id)))
    }
    pub fn image_generation_history(&self, id: Uuid) -> Result<()> {
        self.enqueue(HostCommand::Image(ImageCommand::History(id)))
    }
    pub fn image_generation_provider(&self, id: Uuid) -> Result<()> {
        self.enqueue(HostCommand::Image(ImageCommand::Provider(id)))
    }
    pub fn save_image_generation_provider(
        &self,
        id: Uuid,
        provider: ImageGenerationProvider,
    ) -> Result<()> {
        self.save_image_generation_connection(id, provider, None)
    }
    /// Validate metadata, save its secret in the OS vault, then publish the
    /// connection. The UI receives only the credential reference and receipt.
    pub fn save_image_generation_connection(
        &self,
        id: Uuid,
        provider: ImageGenerationProvider,
        secret: Option<String>,
    ) -> Result<()> {
        self.enqueue(HostCommand::Image(ImageCommand::SaveProvider(
            id, provider, secret,
        )))
    }
}
impl HostState {
    async fn image_service(&self) -> Result<ImageGenerationService> {
        let data = self.paths.data.clone();
        let artifacts = self.paths.artifacts.clone();
        tokio::task::spawn_blocking(move || ImageGenerationService::new(data, artifacts)).await?
    }
    pub(super) async fn handle_image(&self, command: ImageCommand) -> Result<()> {
        let pending_id = command.cancellable_id();
        let cancel = if let Some(id) = pending_id {
            self.capability_requests
                .lock()
                .await
                .get(&id)
                .cloned()
                .context("Image operation is no longer pending")?
        } else {
            CancellationToken::new()
        };
        let event = match command {
            ImageCommand::Bytes(request_id, session_id, bytes, source) => {
                let result = self
                    .import_clipboard_image(session_id, bytes, source, cancel)
                    .await
                    .map_err(|error| error.to_string());
                HostEvent::ImageImported {
                    request_id,
                    session_id,
                    result,
                }
            }
            ImageCommand::Generate(request_id, request) => {
                let result = async {
                    let service = self.image_service().await?;
                    let vault = self.vault()?;
                    let reference = request.provider.credential_ref.clone();
                    let secret = tokio::task::spawn_blocking(move || {
                        reference.map(|reference| vault.get(&reference)).transpose()
                    })
                    .await??
                    .flatten();
                    // The reservation accounts for bounded decoding without
                    // serializing independent HTTP waits behind a decode lock.
                    let _lease = self
                        .resources
                        .acquire(
                            TaskDemand {
                                memory_bytes: 384 * 1024 * 1024,
                                cpu_share: 1.0,
                            },
                            cancel.clone(),
                        )
                        .await?;
                    service.generate(request, secret, cancel).await
                }
                .await
                .map_err(|error: anyhow::Error| error.to_string());
                HostEvent::ImageGeneration { request_id, result }
            }
            ImageCommand::Receipt(request_id, id) => {
                let result = async {
                    let service = self.image_service().await?;
                    tokio::task::spawn_blocking(move || service.receipt(id))
                        .await??
                        .context("This image generation has no saved receipt")
                }
                .await
                .map_err(|error: anyhow::Error| error.to_string());
                HostEvent::ImageGeneration { request_id, result }
            }
            ImageCommand::History(request_id) => {
                let result = async {
                    let service = self.image_service().await?;
                    tokio::task::spawn_blocking(move || service.recent_receipts(16)).await?
                }
                .await
                .map_err(|error: anyhow::Error| error.to_string());
                HostEvent::ImageGenerationHistory { request_id, result }
            }
            ImageCommand::Provider(request_id) => {
                let result = async {
                    let service = self.image_service().await?;
                    tokio::task::spawn_blocking(move || service.configured_provider()).await?
                }
                .await
                .map_err(|error: anyhow::Error| error.to_string());
                HostEvent::ImageGenerationProvider { request_id, result }
            }
            ImageCommand::SaveProvider(request_id, provider, secret) => {
                let result = async {
                    let vault = self.vault()?;
                    let service = self.image_service().await?;
                    tokio::task::spawn_blocking(move || {
                        save_connection(service, provider, secret, |reference, secret| {
                            vault.set(reference, secret)
                        })
                    })
                    .await?
                }
                .await
                .map_err(|error: anyhow::Error| error.to_string());
                HostEvent::ImageGenerationProvider { request_id, result }
            }
        };
        if let Some(id) = pending_id {
            self.capability_requests.lock().await.remove(&id);
        }
        self.events.send(event).await?;
        Ok(())
    }
    async fn import_clipboard_image(
        &self,
        session_id: Uuid,
        bytes: Vec<u8>,
        source: AttachmentSource,
        cancel: CancellationToken,
    ) -> Result<(ImageAttachment, PathBuf)> {
        ensure!(
            !bytes.is_empty() && bytes.len() as u64 <= MAX_IMAGE_BYTES,
            "Image must contain at most 8 MiB"
        );
        let _decode = tokio::select! { _ = cancel.cancelled() => anyhow::bail!("Image import cancelled"), guard = self.image_import_gate.lock() => guard };
        let _lease = self
            .resources
            .acquire(
                TaskDemand {
                    memory_bytes: 384 * 1024 * 1024,
                    cpu_share: 1.0,
                },
                cancel.clone(),
            )
            .await?;
        let artifacts = self.paths.artifacts.clone();
        tokio::task::spawn_blocking(move || {
            let images = xylo_services::attachments::AttachmentStore::new(artifacts)?;
            let attachment = if source == AttachmentSource::Clipboard {
                images.import_clipboard(session_id, &bytes, &cancel)?
            } else {
                images.import_bytes(session_id, "Imported image", &bytes, source, &cancel)?
            };
            let preview = images.preview_path(&attachment)?;
            Ok((attachment, preview))
        })
        .await?
    }
}

fn save_connection(
    service: ImageGenerationService,
    provider: ImageGenerationProvider,
    secret: Option<String>,
    save_secret: impl FnOnce(&str, &str) -> Result<()>,
) -> Result<Option<ImageGenerationProvider>> {
    xylo_services::image_generation::validate_provider(&provider)?;
    if let Some(secret) = secret {
        ensure!(
            !secret.is_empty() && secret.len() <= 65536,
            "Enter a valid image API key"
        );
        let reference = provider
            .credential_ref
            .as_deref()
            .context("Image API key needs a credential reference")?;
        save_secret(reference, &secret)?;
    }
    service.save_provider(&provider)?;
    Ok(Some(provider))
}

#[cfg(test)]
mod connection_tests;
