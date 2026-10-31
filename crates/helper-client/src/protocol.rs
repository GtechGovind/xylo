//! Private, versioned JSONL protocol. Frames exclude the trailing newline from
//! their byte budget. Stdio transport is never exposed as a network listener.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const QUEUE_CAPACITY: usize = 64;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub protocol_version: u32,
    pub id: Uuid,
    pub command: Command,
}

impl Request {
    pub fn new(command: Command) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            id: Uuid::new_v4(),
            command,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Execute { name: String, args: Value },
    Cancel { request_id: Uuid },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub protocol_version: u32,
    pub id: Uuid,
    pub outcome: Outcome,
    #[serde(default)]
    pub background_jobs: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    Completed { value: Value },
    Failed { code: String, message: String },
}

impl Response {
    pub fn completed(id: Uuid, value: Value) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            id,
            outcome: Outcome::Completed { value },
            background_jobs: 0,
        }
    }
    pub fn failed(id: Uuid, code: &str, message: impl Into<String>) -> Self {
        // Do not let an unbounded diagnostic become a second oversized frame.
        let message = message.into().chars().take(16 * 1024).collect();
        Self {
            protocol_version: PROTOCOL_VERSION,
            id,
            outcome: Outcome::Failed {
                code: code.into(),
                message,
            },
            background_jobs: 0,
        }
    }
}

/// Bounded even if a peer sends a newline-free stream. A partial final frame is
/// rejected instead of being interpreted after an interrupted write.
pub async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            ensure!(frame.is_empty(), "Helper protocol ended inside a frame");
            return Ok(None);
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.unwrap_or(available.len());
        ensure!(
            frame.len().saturating_add(count) <= MAX_FRAME_BYTES,
            "Helper frame exceeds 8MiB budget"
        );
        frame.extend_from_slice(&available[..count]);
        reader.consume(count + usize::from(newline.is_some()));
        if newline.is_some() {
            ensure!(!frame.is_empty(), "Empty helper protocol frame");
            return Ok(Some(frame));
        }
    }
}

pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>> {
    let mut frame = serde_json::to_vec(message)?;
    ensure!(
        frame.len() <= MAX_FRAME_BYTES,
        "Helper frame exceeds 8MiB budget"
    );
    frame.push(b'\n');
    Ok(frame)
}

pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    message: &T,
) -> Result<()> {
    writer.write_all(&encode(message)?).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
