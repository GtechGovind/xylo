//! Rich file reads retaining managed images and structured document content.
use super::{ToolBroker, code, notebook, read_state::Fingerprint};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::Path;
use xylo_core::AttachmentSource;
use xylo_services::attachments::AttachmentStore;

/// Parses an inclusive one-based PDF page range with a strict twenty-page cap.
fn pages(args: &Value) -> Result<(u64, u64)> {
    let range = args.get("pages").and_then(Value::as_str).unwrap_or("1-20");
    let (start, end) = range.split_once('-').unwrap_or((range, range));
    let start: u64 = start
        .trim()
        .parse()
        .context("pages must be a page or range such as 1-5")?;
    let end: u64 = end
        .trim()
        .parse()
        .context("pages must be a page or range such as 1-5")?;
    ensure!(
        start > 0 && end >= start && end - start < 20,
        "Read between 1 and 20 PDF pages per call"
    );
    Ok((start, end - start + 1))
}
fn pdf_args(path: &Path, args: &Value) -> Result<Value> {
    let (start, count) = pages(args)?;
    Ok(json!({"path":path,"page":start,"max_pages":count}))
}
pub(super) fn execute(broker: &ToolBroker, path: &Path, args: &Value) -> Result<Value> {
    let fingerprint = Fingerprint::read(path)?;
    let shown = code::display(&broker.request.workspace, path);
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let result = match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" => {
            let image = AttachmentStore::new(broker.artifacts.clone())?.import_file(
                broker.request.session_id,
                path.to_owned(),
                AttachmentSource::Generated,
                &broker.cancel,
            )?;
            json!({"path":shown,"images":[image],"note":"Image content is attached to this tool result"})
        }
        "ipynb" => {
            let mut result = notebook::read(path, &shown, args)?;
            let mut images = Vec::new();
            if let Some(cells) = result["cells"].as_array_mut() {
                for cell in cells {
                    for output in cell["outputs"].as_array_mut().into_iter().flatten() {
                        for media in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
                            let encoded = match output["data"].get(media) {
                                Some(Value::String(text)) => Some(text.clone()),
                                Some(Value::Array(lines)) if lines.iter().all(Value::is_string) => {
                                    Some(lines.iter().filter_map(Value::as_str).collect::<String>())
                                }
                                _ => None,
                            };
                            if let Some(encoded) = encoded {
                                use base64::Engine as _;
                                ensure!(
                                    encoded.len() <= 12 * 1024 * 1024,
                                    "Notebook image exceeds import budget"
                                );
                                ensure!(
                                    images.len() < 16,
                                    "Read fewer notebook image outputs per call"
                                );
                                let bytes =
                                    base64::engine::general_purpose::STANDARD.decode(encoded)?;
                                let attachment = AttachmentStore::new(broker.artifacts.clone())?
                                    .import_bytes(
                                        broker.request.session_id,
                                        "Notebook output",
                                        &bytes,
                                        AttachmentSource::Generated,
                                        &broker.cancel,
                                    )?;
                                output["data"][media] = json!({"image_id":attachment.id});
                                images.push(attachment);
                            }
                        }
                    }
                }
            }
            if !images.is_empty() {
                result["images"] = json!(images);
            }
            result
        }
        "pdf" => {
            let document_args = pdf_args(path, args)?;
            let mut config = broker.capability_config.clone();
            // The broker has already authorized this exact canonical input.
            // Document input checks still constrain traversal to its directory.
            if !path.starts_with(&config.workspace) && !path.starts_with(&config.artifacts) {
                config.workspace = path
                    .parent()
                    .context("PDF has no parent directory")?
                    .to_owned();
            }
            let observation = broker
                .runtime
                .block_on(xylo_capabilities::documents::execute(
                    &config,
                    "document_read",
                    &document_args,
                    broker.cancel.child_token(),
                ))?;
            serde_json::from_str(&observation.summary)?
        }
        _ => code::read(path, &shown, args, &broker.window())?,
    };
    broker.session.reads.record(
        broker.request.session_id,
        &broker.request.workspace,
        path,
        fingerprint,
    )?;
    Ok(result)
}
#[cfg(test)]
mod tests;
