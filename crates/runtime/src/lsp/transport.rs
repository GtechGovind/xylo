//! Bounded LSP content-length framing. Payload sizes count UTF-8 bytes, not
//! characters; an invalid header is rejected before a body allocation occurs.

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(super) const MAX_FRAME: usize = 8 * 1024 * 1024;
const MAX_HEADER: usize = 8192;

pub(super) async fn read_frame(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Option<Value>> {
    let mut length = None;
    let mut header_bytes = 0;
    loop {
        let mut line = Vec::new();
        let read = (&mut *reader)
            .take((MAX_HEADER + 1 - header_bytes) as u64)
            .read_until(b'\n', &mut line)
            .await?;
        if read == 0 {
            ensure!(header_bytes == 0, "Language server closed inside a header");
            return Ok(None);
        }
        header_bytes += read;
        ensure!(
            header_bytes <= MAX_HEADER,
            "Language-server header exceeds its size limit"
        );
        ensure!(
            line.ends_with(b"\r\n"),
            "Language-server header requires CRLF framing"
        );
        if line == b"\r\n" {
            break;
        }
        let line = std::str::from_utf8(&line[..line.len() - 2])
            .context("Invalid language-server header")?;
        let (name, value) = line
            .split_once(':')
            .context("Malformed language-server header")?;
        if name.eq_ignore_ascii_case("Content-Length") {
            ensure!(length.is_none(), "Duplicate language-server content length");
            let value = value
                .trim()
                .parse::<usize>()
                .context("Invalid language-server content length")?;
            ensure!(
                value > 0 && value <= MAX_FRAME,
                "Language-server response exceeds its size limit"
            );
            length = Some(value);
        }
        if name.eq_ignore_ascii_case("Content-Type") {
            let value = value.to_ascii_lowercase();
            if let Some((_, encoding)) = value.split_once("charset=") {
                ensure!(
                    matches!(encoding.trim(), "utf-8" | "utf8"),
                    "Unsupported language-server encoding"
                );
            }
        }
    }
    let length = length.context("Language-server response has no content length")?;
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .await
        .context("Language server closed inside a response")?;
    let value: Value = serde_json::from_slice(&bytes).context("Invalid language-server JSON")?;
    ensure!(
        value.is_object() && value.get("jsonrpc").and_then(Value::as_str) == Some("2.0"),
        "Invalid language-server JSON-RPC message"
    );
    Ok(Some(value))
}

pub(super) async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    message: &Value,
) -> Result<()> {
    let bytes = serde_json::to_vec(message)?;
    if bytes.len() > MAX_FRAME {
        bail!("Code intelligence request exceeds its size limit");
    }
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())
        .await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
