//! Explicit native image generation. A submitted request is never replayed
//! automatically: transport loss and cancellation keep a durable uncertain result.
mod http;
#[cfg(test)]
mod tests;

use crate::{
    attachments::AttachmentStore,
    providers::{ProviderCredential, normalize_provider},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;
use xylo_core::{
    ApiFamily, AttachmentSource, GeneratedImage, ImageGenerationProtocol, ImageGenerationProvider,
    ImageGenerationReceipt, ImageGenerationRequest, ImageGenerationState, ProviderConfig, now_ms,
};

#[derive(Clone)]
pub struct ImageGenerationService {
    root: PathBuf,
    artifacts: PathBuf,
}
#[derive(Serialize, Deserialize)]
struct Job {
    request_sha256: String,
    receipt: ImageGenerationReceipt,
}
enum Preparation {
    Ready(File, ImageGenerationReceipt),
    Existing(ImageGenerationReceipt),
}
enum ImagePayload {
    Bytes(Vec<u8>),
    Url(String),
}
struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl ImageGenerationService {
    pub fn new(data_dir: PathBuf, artifacts: PathBuf) -> Result<Self> {
        fs::create_dir_all(&data_dir)?;
        fs::create_dir_all(&artifacts)?;
        let root = data_dir.canonicalize()?.join("image-generation");
        directory(&root)?;
        let artifacts = artifacts.canonicalize()?;
        directory(&artifacts.join("generated"))?;
        Ok(Self { root, artifacts })
    }
    pub fn configured_provider(&self) -> Result<Option<ImageGenerationProvider>> {
        let path = self.root.join("provider.json");
        if !path.exists() {
            return Ok(None);
        }
        let provider: ImageGenerationProvider =
            serde_json::from_slice(&read_file(&path, 32 * 1024)?)?;
        validate_provider(&provider)?;
        Ok(Some(provider))
    }
    pub fn save_provider(&self, provider: &ImageGenerationProvider) -> Result<()> {
        validate_provider(provider)?;
        atomic_json(&self.root.join("provider.json"), provider)
    }

    /// Recent requests remain reviewable after application/worker restart. The
    /// bounded atomic index contains IDs only; prompts and credentials are absent.
    pub fn recent_receipts(&self, limit: usize) -> Result<Vec<ImageGenerationReceipt>> {
        ensure!(
            (1..=16).contains(&limit),
            "Choose at most sixteen recent image requests"
        );
        let path = self.root.join("recent.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let ids: Vec<Uuid> = serde_json::from_slice(&read_file(&path, 8192)?)?;
        ensure!(ids.len() <= 64, "Invalid image request index");
        ids.into_iter()
            .rev()
            .take(limit)
            .map(|id| {
                self.receipt(id)?
                    .context("Saved image request is missing its receipt")
            })
            .collect()
    }

    pub fn receipt(&self, id: Uuid) -> Result<Option<ImageGenerationReceipt>> {
        let path = self.job_path(id);
        if !path.exists() {
            return Ok(None);
        }
        let lease = self.try_lease(id)?;
        let mut job: Job = serde_json::from_slice(&read_file(&path, 128 * 1024)?)?;
        ensure!(
            job.receipt.id == id,
            "Saved generation receipt has a different request ID"
        );
        if matches!(
            job.receipt.state,
            ImageGenerationState::Prepared | ImageGenerationState::Submitted
        ) && lease.is_some()
        {
            job.receipt.state = if job.receipt.dispatch_started {
                ImageGenerationState::OutcomeUnknown
            } else {
                ImageGenerationState::Cancelled
            };
            job.receipt.updated_at = now_ms();
            job.receipt.message=if job.receipt.dispatch_started{"The application stopped before generation was confirmed. Check the selected provider before requesting another image."}else{"Generation stopped before dispatch. No request was sent."}.into();
            atomic_json(&path, &job)?;
        }
        self.validate_receipt_images(&job.receipt)?;
        Ok(Some(job.receipt))
    }

    pub async fn generate(
        &self,
        request: ImageGenerationRequest,
        secret: Option<String>,
        cancel: CancellationToken,
    ) -> Result<ImageGenerationReceipt> {
        let cancel = cancel.child_token();
        let _cancel_on_drop = CancelOnDrop(cancel.clone());
        let endpoint = validate_request(&request)?;
        let credential = ProviderCredential::parse(secret)?;
        ensure!(
            !matches!(
                credential,
                ProviderCredential::Aws { .. } | ProviderCredential::ChatGpt { .. }
            ),
            "This image API requires its own API key or bearer credential. ChatGPT plan credentials use the Responses API and are not supported by this operation."
        );
        ensure!(
            request.provider.credential_ref.is_none()
                || !matches!(credential, ProviderCredential::None),
            "The selected image provider credential could not be resolved from the OS credential store"
        );
        let family = if request.provider.protocol == ImageGenerationProtocol::GeminiGenerateContent
        {
            ApiFamily::Gemini
        } else {
            ApiFamily::OpenAiChat
        };
        let headers = credential.headers(&family, &request.provider.headers)?;
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(request.provider.timeout_seconds))
            .build()?;
        let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&request)?));
        let preparation = {
            let service = self.clone();
            let request = request.clone();
            let fingerprint = fingerprint.clone();
            tokio::task::spawn_blocking(move || service.prepare(&request, &fingerprint))
                .await
                .context("Image request preparation task failed")??
        };
        let (_lease, mut receipt) = match preparation {
            Preparation::Ready(lease, receipt) => (std::sync::Arc::new(lease), receipt),
            Preparation::Existing(receipt) => return Ok(receipt),
        };
        if cancel.is_cancelled() {
            receipt.state = ImageGenerationState::Cancelled;
            receipt.message = "Generation cancelled before dispatch. No request was sent.".into();
            self.persist_async(&fingerprint, &mut receipt, &_lease)
                .await?;
            return Ok(receipt);
        }
        let body = request_body(&request)?;
        receipt.state = ImageGenerationState::Submitted;
        receipt.dispatch_started = true;
        receipt.message = "Generation submitted to the selected provider.".into();
        self.persist_async(&fingerprint, &mut receipt, &_lease)
            .await?;
        if cancel.is_cancelled() {
            receipt.state = ImageGenerationState::Cancelled;
            receipt.dispatch_started = false;
            receipt.message = "Generation cancelled before dispatch. No request was sent.".into();
            self.persist_async(&fingerprint, &mut receipt, &_lease)
                .await?;
            return Ok(receipt);
        }
        let send = client
            .post(endpoint.clone())
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .json(&body)
            .send();
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(request.provider.timeout_seconds);
        let response = tokio::select! {biased;_ = cancel.cancelled()=>Err(anyhow::anyhow!("Generation cancelled after dispatch")),response=tokio::time::timeout_at(deadline,send)=>match response{Ok(response)=>response.map_err(|_|anyhow::anyhow!("Generation transport failed after dispatch")),Err(_)=>Err(anyhow::anyhow!("Generation timed out after dispatch"))}};
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                receipt.state = ImageGenerationState::OutcomeUnknown;
                receipt.message = format!(
                    "{error}. The provider may have generated or charged for an image. Check it before retrying."
                );
                self.persist_async(&fingerprint, &mut receipt, &_lease)
                    .await?;
                return Ok(receipt);
            }
        };
        receipt.provider_request_id = response
            .headers()
            .get("x-request-id")
            .or_else(|| response.headers().get("x-goog-request-id"))
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .chars()
                    .filter(|character| {
                        character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
                    })
                    .take(160)
                    .collect()
            });
        if !response.status().is_success() {
            receipt.state =
                if response.status().is_server_error() || response.status().as_u16() == 408 {
                    ImageGenerationState::OutcomeUnknown
                } else {
                    ImageGenerationState::Failed
                };
            receipt.message = format!(
                "Selected image provider returned HTTP {}. Check the chosen model, credential and API support. No automatic retry or model fallback was attempted.",
                response.status().as_u16()
            );
            if receipt.state == ImageGenerationState::OutcomeUnknown {
                receipt.message.push_str(" The outcome is uncertain and may have incurred a charge; check the provider before creating another request.");
            }
            self.persist_async(&fingerprint, &mut receipt, &_lease)
                .await?;
            return Ok(receipt);
        }
        let result = tokio::time::timeout_at(
            deadline,
            self.receive_image(response, &endpoint, &request, cancel.clone()),
        )
        .await;
        match result {
            Ok(Ok(image)) => {
                receipt.images.push(image);
                receipt.state = ImageGenerationState::Succeeded;
                receipt.message = "Generated image saved on this computer.".into();
            }
            Ok(Err(error)) if !cancel.is_cancelled() => {
                receipt.state = ImageGenerationState::Failed;
                receipt.message = format!(
                    "The provider responded, but its image could not be saved: {error}. It may have charged for this request; no retry was attempted."
                );
            }
            _ => {
                cancel.cancel();
                receipt.state = ImageGenerationState::OutcomeUnknown;
                receipt.message="Generation was cancelled or timed out after dispatch. The provider may have generated or charged for an image. Check it before trying again.".into();
            }
        }
        self.persist_async(&fingerprint, &mut receipt, &_lease)
            .await?;
        Ok(receipt)
    }

    async fn receive_image(
        &self,
        response: reqwest::Response,
        endpoint: &Url,
        request: &ImageGenerationRequest,
        cancel: CancellationToken,
    ) -> Result<GeneratedImage> {
        let bytes = http::read_bounded(response, 12 * 1024 * 1024, &cancel).await?;
        let protocol = request.provider.protocol;
        let payload = tokio::task::spawn_blocking(move || image_payload(bytes, protocol))
            .await
            .context("Image response decode task failed")??;
        let bytes = match payload {
            ImagePayload::Bytes(bytes) => bytes,
            ImagePayload::Url(url) => http::download_image(&url, endpoint, &cancel).await?,
        };
        ensure!(!cancel.is_cancelled(), "Image save cancelled");
        let artifacts = self.artifacts.clone();
        let session = request.session_id;
        let id = request.id;
        tokio::task::spawn_blocking(move || {
            let store = AttachmentStore::new(artifacts.clone())?;
            let attachment = store.import_bytes(
                session,
                "Generated image",
                &bytes,
                AttachmentSource::Generated,
                &cancel,
            )?;
            let extension = match attachment.media_type.as_str() {
                "image/png" => "png",
                "image/jpeg" => "jpg",
                "image/webp" => "webp",
                "image/gif" => "gif",
                _ => bail!("Unsupported generated image"),
            };
            let artifact_path = artifacts
                .join("generated")
                .join(format!("{id}.{extension}"));
            ensure!(!cancel.is_cancelled(), "Image save cancelled");
            let mut file = tempfile::NamedTempFile::new_in(artifact_path.parent().unwrap())?;
            file.write_all(&bytes)?;
            file.as_file().sync_all()?;
            ensure!(!cancel.is_cancelled(), "Image save cancelled");
            file.persist_noclobber(&artifact_path)
                .map_err(|error| error.error)?;
            #[cfg(unix)]
            File::open(artifact_path.parent().unwrap())?.sync_all()?;
            Ok(GeneratedImage {
                preview_path: store.preview_path(&attachment)?,
                attachment,
                artifact_path,
            })
        })
        .await
        .context("Native image import task failed")?
    }
    fn prepare(&self, request: &ImageGenerationRequest, fingerprint: &str) -> Result<Preparation> {
        let _lease = match self.try_lease(request.id)? {
            Some(lease) => lease,
            None => {
                let existing: Job = serde_json::from_slice(
                    &read_file(&self.job_path(request.id), 128 * 1024)
                        .context("This generation is already starting")?,
                )?;
                ensure!(
                    existing.request_sha256 == fingerprint,
                    "Generation ID already belongs to another request"
                );
                return Ok(Preparation::Existing(existing.receipt));
            }
        };
        if self.job_path(request.id).exists() {
            let previous: Job =
                serde_json::from_slice(&read_file(&self.job_path(request.id), 128 * 1024)?)?;
            ensure!(
                previous.request_sha256 == fingerprint,
                "Generation ID already belongs to another request"
            );
            let mut receipt = previous.receipt;
            if matches!(
                receipt.state,
                ImageGenerationState::Prepared | ImageGenerationState::Submitted
            ) {
                receipt.state = if receipt.dispatch_started {
                    ImageGenerationState::OutcomeUnknown
                } else {
                    ImageGenerationState::Cancelled
                };
                receipt.updated_at = now_ms();
                receipt.message="A prior attempt already owns this generation ID. It was not dispatched again; check its outcome before creating another request.".into();
                self.persist(fingerprint, &mut receipt)?;
            }
            self.validate_receipt_images(&receipt)?;
            return Ok(Preparation::Existing(receipt));
        }
        let mut receipt = ImageGenerationReceipt {
            id: request.id,
            session_id: request.session_id,
            state: ImageGenerationState::Prepared,
            provider_name: request.provider.name.clone(),
            model: request.provider.model.clone(),
            created_at: now_ms(),
            updated_at: now_ms(),
            dispatch_started: false,
            provider_request_id: None,
            images: Vec::new(),
            message: "Preparing the selected image request.".into(),
        };
        self.persist(fingerprint, &mut receipt)?;
        Ok(Preparation::Ready(_lease, receipt))
    }

    async fn persist_async(
        &self,
        fingerprint: &str,
        receipt: &mut ImageGenerationReceipt,
        lease: &std::sync::Arc<File>,
    ) -> Result<()> {
        let service = self.clone();
        let lease = lease.clone();
        let fingerprint = fingerprint.to_owned();
        let mut pending = receipt.clone();
        *receipt = tokio::task::spawn_blocking(move || -> Result<_> {
            // A cancelled/dropped async caller cannot release the operation's
            // file lease until this already-started durable phase completes.
            let _lease = lease;
            service.persist(&fingerprint, &mut pending)?;
            Ok(pending)
        })
        .await
        .context("Image receipt persistence task failed")??;
        Ok(())
    }

    fn persist(&self, fingerprint: &str, receipt: &mut ImageGenerationReceipt) -> Result<()> {
        receipt.updated_at = now_ms();
        atomic_json(
            &self.job_path(receipt.id),
            &Job {
                request_sha256: fingerprint.into(),
                receipt: receipt.clone(),
            },
        )?;
        let path = self.root.join("recent.json");
        let lock_path = self.root.join("recent.lock");
        if lock_path.exists() {
            ensure!(
                !fs::symlink_metadata(&lock_path)?.file_type().is_symlink(),
                "Invalid image index lock"
            );
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match fs2::FileExt::try_lock_exclusive(&lock) {
                Ok(()) => break,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
                {
                    ensure!(
                        std::time::Instant::now() < deadline,
                        "Image request history is busy; check saved request status"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
        let mut ids: Vec<Uuid> = if path.exists() {
            serde_json::from_slice(&read_file(&path, 8192)?)?
        } else {
            Vec::new()
        };
        ensure!(ids.len() <= 64, "Invalid image request index");
        if !ids.contains(&receipt.id) {
            ids.push(receipt.id);
            if ids.len() > 64 {
                ids.remove(0);
            }
            atomic_json(&path, &ids)?;
        }
        Ok(())
    }
    fn job_path(&self, id: Uuid) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }
    fn try_lease(&self, id: Uuid) -> Result<Option<File>> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join(format!("{id}.lock")))?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(file)),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
            {
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }
    fn validate_receipt_images(&self, receipt: &ImageGenerationReceipt) -> Result<()> {
        ensure!(
            receipt.images.len() <= 1,
            "Invalid saved generation receipt"
        );
        if !receipt.images.is_empty() {
            directory(&self.artifacts.join("generated"))?;
            let store = AttachmentStore::new(self.artifacts.clone())?;
            for image in &receipt.images {
                store.validate(receipt.session_id, std::slice::from_ref(&image.attachment))?;
                let preview = store.preview_path(&image.attachment)?;
                ensure!(
                    image.preview_path == preview
                        && image.artifact_path
                            == self.artifacts.join("generated").join(format!(
                                "{}.{}",
                                receipt.id,
                                match image.attachment.media_type.as_str() {
                                    "image/png" => "png",
                                    "image/jpeg" => "jpg",
                                    "image/webp" => "webp",
                                    "image/gif" => "gif",
                                    _ => bail!("Invalid generated image format"),
                                }
                            )),
                    "Invalid generated artifact path"
                );
                let bytes = read_file(&image.artifact_path, xylo_core::MAX_IMAGE_BYTES as usize)?;
                ensure!(
                    format!("{:x}", Sha256::digest(bytes)) == image.attachment.sha256,
                    "Generated artifact failed its integrity check"
                );
            }
        }
        Ok(())
    }
}

fn image_payload(bytes: Vec<u8>, protocol: ImageGenerationProtocol) -> Result<ImagePayload> {
    let value: Value = serde_json::from_slice(&bytes).context("Provider returned invalid JSON")?;
    Ok(match protocol {
        ImageGenerationProtocol::OpenAiImages => {
            let data = value
                .get("data")
                .and_then(Value::as_array)
                .context("Provider returned no image data")?;
            ensure!(
                data.len() == 1,
                "Provider must return exactly one requested image"
            );
            if let Some(data) = data[0].get("b64_json").and_then(Value::as_str) {
                ImagePayload::Bytes(http::decode_image(data)?)
            } else {
                let url = data[0]
                    .get("url")
                    .and_then(Value::as_str)
                    .context("Provider returned no image bytes or URL")?;
                if url.starts_with("data:") {
                    ImagePayload::Bytes(http::decode_data_image(url)?)
                } else {
                    ensure!(
                        url.len() <= 16 * 1024,
                        "Provider returned an oversized image URL"
                    );
                    ImagePayload::Url(url.to_owned())
                }
            }
        }
        ImageGenerationProtocol::GeminiGenerateContent => {
            let candidates = value
                .get("candidates")
                .and_then(Value::as_array)
                .context("Provider returned no image candidate")?;
            ensure!(
                candidates.len() == 1,
                "Provider must return one image candidate"
            );
            let finish = candidates[0]
                .get("finishReason")
                .and_then(Value::as_str)
                .unwrap_or("STOP");
            ensure!(
                finish == "STOP",
                "Provider did not complete image generation"
            );
            let parts = candidates[0]
                .pointer("/content/parts")
                .and_then(Value::as_array)
                .context("Provider returned no image parts")?;
            let images = parts
                .iter()
                .filter_map(|part| part.get("inlineData").or_else(|| part.get("inline_data")))
                .collect::<Vec<_>>();
            ensure!(
                images.len() == 1,
                "Provider returned no image or more than one image"
            );
            let mime = images[0]
                .get("mimeType")
                .or_else(|| images[0].get("mime_type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            ensure!(
                matches!(
                    mime,
                    "image/png" | "image/jpeg" | "image/webp" | "image/gif"
                ),
                "Unsupported generated image format"
            );
            ImagePayload::Bytes(http::decode_image(
                images[0]
                    .get("data")
                    .and_then(Value::as_str)
                    .context("Provider returned empty image bytes")?,
            )?)
        }
    })
}

pub fn validate_provider(provider: &ImageGenerationProvider) -> Result<Url> {
    ensure!(
        !provider.name.trim().is_empty() && provider.name.len() <= 160,
        "Name the selected image provider"
    );
    ensure!(
        !provider.model.trim().is_empty() && provider.model.len() <= 256,
        "Choose an image-generation model explicitly"
    );
    ensure!(
        provider.timeout_seconds > 0 && provider.timeout_seconds <= 600,
        "Image generation timeout must be between 1 and 600 seconds"
    );
    ensure!(
        provider.endpoint.len() <= 8192 && provider.headers.len() <= 16,
        "Image provider configuration is too large"
    );
    for (name, value) in &provider.headers {
        let name = name.to_ascii_lowercase();
        ensure!(
            name.len() <= 128
                && value.len() <= 4096
                && !name.contains("key")
                && !name.contains("token")
                && !name.contains("secret")
                && !name.contains("auth")
                && !name.contains("cookie")
                && !name.contains("password")
                && !name.contains("credential")
                && !name.contains("session"),
            "Store image provider secrets in the OS credential store, not custom headers"
        );
    }
    if let Some(reference) = &provider.credential_ref {
        ensure!(
            !reference.is_empty() && reference.len() <= 256,
            "Invalid image credential reference"
        );
    }
    let config = ProviderConfig {
        family: if provider.protocol == ImageGenerationProtocol::GeminiGenerateContent {
            ApiFamily::Gemini
        } else {
            ApiFamily::OpenAiChat
        },
        endpoint: provider.endpoint.clone(),
        model: provider.model.clone(),
        ..Default::default()
    };
    ensure!(
        serde_json::to_vec(provider)?.len() <= 32 * 1024,
        "Image provider configuration exceeds 32 KiB"
    );
    ProviderCredential::None.headers(&config.family, &provider.headers)?;
    let normalized = normalize_provider(&config)?;
    let mut endpoint = normalized.inference_url;
    if provider.protocol == ImageGenerationProtocol::OpenAiImages {
        let path = endpoint
            .path()
            .trim_end_matches("/chat/completions")
            .trim_end_matches("/images/generations")
            .to_owned();
        endpoint.set_path(&format!("{path}/images/generations"));
    }
    Ok(endpoint)
}
fn validate_request(request: &ImageGenerationRequest) -> Result<Url> {
    ensure!(
        !request.id.is_nil() && !request.session_id.is_nil(),
        "Generation requires valid request and conversation IDs"
    );
    ensure!(
        !request.prompt.trim().is_empty() && request.prompt.len() <= 16 * 1024,
        "Image prompt must contain at most 16 KiB of text"
    );
    ensure!(
        matches!(
            request.size.as_str(),
            "auto" | "256x256" | "512x512" | "1024x1024" | "1024x1536" | "1536x1024"
        ),
        "Choose a supported explicit image size"
    );
    if request.provider.protocol == ImageGenerationProtocol::GeminiGenerateContent {
        ensure!(
            !matches!(request.size.as_str(), "256x256" | "512x512"),
            "The native Gemini operation supports automatic or 1K images; choose a 1K size explicitly"
        );
    }
    validate_provider(&request.provider)
}
fn request_body(request: &ImageGenerationRequest) -> Result<Value> {
    Ok(match request.provider.protocol {
        ImageGenerationProtocol::OpenAiImages => {
            json!({"model":request.provider.model,"prompt":request.prompt,"size":request.size,"n":1})
        }
        ImageGenerationProtocol::GeminiGenerateContent => {
            let mut body = json!({"contents":[{"role":"user","parts":[{"text":request.prompt}]}],"generationConfig":{"responseModalities":["TEXT","IMAGE"]}});
            if request.size != "auto" {
                body["generationConfig"]["imageConfig"] = json!({"aspectRatio":match request.size.as_str(){"1024x1536"=>"2:3","1536x1024"=>"3:2",_=>"1:1"},"imageSize":"1K"});
            }
            body
        }
    })
}
fn directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    ensure!(
        fs::symlink_metadata(path)?.is_dir()
            && !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "Managed generation directory must be a regular directory"
    );
    Ok(())
}
fn read_file(path: &Path, max: usize) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= max as u64,
        "Managed generation file is invalid or oversized"
    );
    let mut bytes = Vec::new();
    File::open(path)?
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= max, "Managed generation file is oversized");
    Ok(bytes)
}
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= 128 * 1024,
        "Generation record exceeds its limit"
    );
    let parent = path.parent().context("Missing generation directory")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
