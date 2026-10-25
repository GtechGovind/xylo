//! Explicit image generation requests and durable outcomes. Secrets and image
//! bytes stay in native services; the product carries references and receipts.
use crate::ImageAttachment;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImageGenerationProtocol {
    OpenAiImages,
    GeminiGenerateContent,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImageGenerationProvider {
    pub name: String,
    pub protocol: ImageGenerationProtocol,
    pub endpoint: String,
    pub model: String,
    pub credential_ref: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub timeout_seconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImageGenerationRequest {
    pub id: Uuid,
    pub session_id: Uuid,
    pub provider: ImageGenerationProvider,
    pub prompt: String,
    pub size: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImageGenerationState {
    Prepared,
    Submitted,
    Succeeded,
    Failed,
    Cancelled,
    OutcomeUnknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GeneratedImage {
    pub attachment: ImageAttachment,
    pub preview_path: PathBuf,
    pub artifact_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageGenerationReceipt {
    pub id: Uuid,
    pub session_id: Uuid,
    pub state: ImageGenerationState,
    pub provider_name: String,
    pub model: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub dispatch_started: bool,
    pub provider_request_id: Option<String>,
    pub images: Vec<GeneratedImage>,
    pub message: String,
}
