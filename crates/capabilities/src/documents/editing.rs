//! Native parse/edit/copy transaction. All APIs here run on blocking workers.
use super::{check, docx, package::Package, spreadsheet, transaction};
use crate::{CapabilityConfig, input_path, observation};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Read,
    path::Path,
};
use tokio_util::sync::CancellationToken;
use xylo_core::*;
pub fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
pub fn read_bounded(
    reader: &mut impl Read,
    max: u64,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        check(cancel)?;
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        ensure!(
            bytes.len() as u64 + count as u64 <= max,
            "Document exceeds its byte budget"
        );
        bytes.extend_from_slice(&buffer[..count]);
    }
    Ok(bytes)
}
fn kind(path: &Path) -> Result<DocumentKind> {
    Ok(
        match path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "docx" => DocumentKind::Docx,
            "xlsx" => DocumentKind::Spreadsheet,
            "txt" | "md" | "csv" | "json" | "html" | "xml" | "log" | "yaml" | "yml" | "rs"
            | "toml" => DocumentKind::Text,
            _ => bail!(
                "Native copy editing supports DOCX, XLSX and UTF-8 text. Other Office formats remain extract-only."
            ),
        },
    )
}
fn source(
    config: &CapabilityConfig,
    path: &str,
    cancel: &CancellationToken,
) -> Result<(DocumentSource, Vec<u8>)> {
    let path = input_path(config, path)?;
    ensure!(
        fs::metadata(&path)?.len() <= 64 * 1024 * 1024,
        "Document exceeds 64 MiB input budget"
    );
    let bytes = read_bounded(&mut File::open(&path)?, 64 * 1024 * 1024, cancel)?;
    let source = DocumentSource {
        kind: kind(&path)?,
        revision: digest(&bytes),
        bytes: bytes.len() as u64,
        path,
    };
    Ok((source, bytes))
}
pub fn verify(
    config: &CapabilityConfig,
    source: &DocumentSource,
    cancel: &CancellationToken,
) -> Result<()> {
    let path = input_path(
        config,
        source
            .path
            .to_str()
            .context("Document path must be UTF-8")?,
    )?;
    ensure!(
        path == source.path,
        "Source path changed; reopen the document"
    );
    let mut file = File::open(path)?;
    ensure!(
        file.metadata()?.len() == source.bytes,
        "Source revision changed; reopen before saving a copy"
    );
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut count = 0u64;
    loop {
        check(cancel)?;
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        ensure!(count <= 64 * 1024 * 1024, "Source grew beyond input budget");
        hash.update(&buffer[..n]);
    }
    ensure!(
        count == source.bytes && hex::encode(hash.finalize()) == source.revision,
        "Source revision changed; reopen before saving a copy"
    );
    Ok(())
}
pub fn open(
    config: &CapabilityConfig,
    args: DocumentOpenRequest,
    cancel: &CancellationToken,
) -> Result<DocumentSnapshot> {
    ensure!(
        !args.path.is_empty() && (1..=200).contains(&args.limit),
        "Choose a document and a page of 1..200 paragraphs"
    );
    let (source, bytes) = source(config, &args.path, cancel)?;
    if let Some(revision) = &args.expected_revision {
        ensure!(
            *revision == source.revision,
            "Source revision changed; reopen the document before paging"
        );
    }
    let content = match source.kind {
        DocumentKind::Text => {
            ensure!(
                bytes.len() <= 512 * 1024,
                "Native text editor supports whole documents up to 512 KiB; use the source editor for larger files"
            );
            let text =
                std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes))
                    .context("Native text editing requires UTF-8")?;
            ensure!(!text.contains('\0'), "Binary/NUL text is not editable");
            DocumentContent::Text { text: text.into() }
        }
        DocumentKind::Docx => docx::open(
            &mut Package::new(bytes, cancel)?,
            &args,
            config.max_output_bytes,
            cancel,
        )?,
        DocumentKind::Spreadsheet => spreadsheet::open(
            &mut Package::new(bytes, cancel)?,
            &args,
            &source.revision,
            config.max_output_bytes.min(8 * 1024 * 1024),
            cancel,
        )?,
    };
    Ok(DocumentSnapshot { source, content })
}
pub fn build(
    config: &CapabilityConfig,
    request: &DocumentSaveRequest,
    path: &Path,
    cancel: &CancellationToken,
) -> Result<()> {
    let (actual, bytes) = source(
        config,
        request
            .source
            .path
            .to_str()
            .context("Document path must be UTF-8")?,
        cancel,
    )?;
    ensure!(
        actual == request.source,
        "Source revision changed; reopen before saving a copy"
    );
    ensure!(
        !request.edits.is_empty()
            && request.edits.len() <= 4096
            && serde_json::to_vec(&request.edits)?.len() <= 1024 * 1024,
        "Save requires 1..4096 coalesced edits within 1 MiB"
    );
    match request.source.kind {
        DocumentKind::Text => {
            ensure!(
                request.edits.len() == 1,
                "Text edits must be coalesced into one full document"
            );
            let DocumentEdit::Text { text } = &request.edits[0] else {
                bail!("Text document requires a text edit")
            };
            ensure!(
                text.len() <= 512 * 1024 && !text.contains('\0'),
                "Edited UTF-8 text exceeds native editor limits"
            );
            let mut value = Vec::new();
            if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
                value.extend([0xef, 0xbb, 0xbf]);
            }
            value.extend(text.as_bytes());
            fs::write(path, value)?;
        }
        DocumentKind::Docx => {
            ensure!(
                request.edits.len() <= 512,
                "At most 512 paragraph edits per copy"
            );
            let mut package = Package::new(bytes, cancel)?;
            let replacements = docx::edit(&mut package, &request.edits, cancel)?;
            package.rewrite(path, &replacements, cancel)?;
        }
        DocumentKind::Spreadsheet => {
            let mut package = Package::new(bytes, cancel)?;
            let replacements = spreadsheet::edit(&mut package, &request.edits, cancel)?;
            package.rewrite(path, &replacements, cancel)?;
        }
    }
    check(cancel)?;
    ensure!(
        fs::metadata(path)?.len() <= 128 * 1024 * 1024,
        "Edited document exceeds output budget"
    );
    Ok(())
}
pub fn execute(
    config: &CapabilityConfig,
    name: &str,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<ToolObservation> {
    ensure!(
        serde_json::to_vec(args)?.len() <= 2 * 1024 * 1024,
        "Document request exceeds 2 MiB"
    );
    let value = match name {
        "document_open" => {
            serde_json::to_value(open(config, serde_json::from_value(args.clone())?, cancel)?)?
        }
        "document_save_copy" => serde_json::to_value(transaction::save(
            config,
            serde_json::from_value(args.clone())?,
            cancel,
        )?)?,
        "document_save_status" => serde_json::to_value(transaction::status(
            config,
            serde_json::from_value(
                args.get("operation_id")
                    .context("Missing operation_id")?
                    .clone(),
            )?,
        )?)?,
        _ => bail!("Unknown native document operation"),
    };
    let artifacts = if name == "document_save_copy"
        && value.get("state").and_then(Value::as_str) == Some("committed")
    {
        value
            .get("result")
            .and_then(|v| v.get("path"))
            .and_then(Value::as_str)
            .map(std::path::PathBuf::from)
            .into_iter()
            .collect()
    } else {
        Vec::new()
    };
    let result = observation(value, artifacts);
    ensure!(
        result.summary.len() <= config.max_output_bytes,
        "Document page exceeds response budget; reduce row/column count or paragraph limit. Editable content was not truncated."
    );
    Ok(result)
}
