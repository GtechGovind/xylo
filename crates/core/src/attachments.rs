//! Durable image references. Bytes and native paths never cross chat protocols.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_ATTACHMENTS_PER_TURN: usize = 4;
pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentSource {
    Picker,
    Drop,
    PastedFile,
    Clipboard,
    Generated,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImageAttachment {
    pub id: Uuid,
    pub session_id: Uuid,
    pub name: String,
    pub media_type: String,
    pub sha256: String,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    pub source: AttachmentSource,
    pub imported_at: i64,
}
