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
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::BufReader;
    #[tokio::test]
    async fn unicode_byte_lengths_and_back_to_back_frames_round_trip() {
        let (mut input, output) = tokio::io::duplex(4096);
        let first = json!({"jsonrpc":"2.0","id":1,"result":"世界 🦀"});
        let second = json!({"jsonrpc":"2.0","method":"initialized","params":{}});
        let a = first.clone();
        let b = second.clone();
        let writer = tokio::spawn(async move {
            write_frame(&mut input, &a).await.unwrap();
            write_frame(&mut input, &b).await.unwrap();
        });
        let mut reader = BufReader::new(output);
        assert_eq!(read_frame(&mut reader).await.unwrap(), Some(first));
        assert_eq!(read_frame(&mut reader).await.unwrap(), Some(second));
        writer.await.unwrap();
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }
    #[tokio::test]
    async fn oversized_or_ambiguous_headers_fail_before_allocating_a_body() {
        for bytes in [
            b"Content-Length: 999999999\r\n\r\n".as_slice(),
            b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}".as_slice(),
            b"Content-Length: 2\n\n{}".as_slice(),
        ] {
            assert!(read_frame(&mut BufReader::new(bytes)).await.is_err());
        }
        let bytes = vec![b'x'; MAX_HEADER + 1];
        assert!(
            read_frame(&mut BufReader::new(bytes.as_slice()))
                .await
                .is_err()
        );
    }
}
