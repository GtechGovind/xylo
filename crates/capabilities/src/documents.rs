//! Native document extraction and artifact export. Source references are retained
//! in observations and optional exports; office formula execution is not enabled.

mod docx;
mod editing;
mod package;
mod spreadsheet;
mod transaction;
mod xml;
use editing::read_bounded;
fn check(cancel: &CancellationToken) -> Result<()> {
    ensure!(!cancel.is_cancelled(), "Document operation cancelled");
    Ok(())
}

use crate::{CapabilityConfig, definition, input_path, observation, output_path, required_str};
use anyhow::{Context, Result, ensure};
use calamine::{Reader, open_workbook_auto};
use docx_rs::{Docx, Hyperlink, HyperlinkType, Paragraph, Run};
use pdfium_render::prelude::*;
use serde_json::{Value, json};
use std::{fs::File, io::Read, path::Path, time::Duration};
use tokio_util::sync::CancellationToken;
use xylo_core::{ToolDefinition, ToolObservation};

const MAX_INPUT: u64 = 64 * 1024 * 1024;
const MAX_ARCHIVE: u64 = 128 * 1024 * 1024;

/// Validate ZIP metadata before office libraries allocate decompressed content.
fn preflight_archive(path: &Path) -> Result<()> {
    let mut zip = zip::ZipArchive::new(File::open(path)?)?;
    ensure!(
        zip.len() <= 10_000,
        "Office archive contains too many entries"
    );
    let mut total = 0u64;
    for i in 0..zip.len() {
        let file = zip.by_index(i)?;
        total = total
            .checked_add(file.size())
            .context("Office archive size overflow")?;
        ensure!(
            file.size() <= 32 * 1024 * 1024 && total <= MAX_ARCHIVE,
            "Office archive exceeds decompressed byte budget"
        );
    }
    Ok(())
}

pub async fn execute(
    config: &CapabilityConfig,
    name: &str,
    args: &Value,
    cancel: CancellationToken,
) -> Result<ToolObservation> {
    let config = config.clone();
    let name = name.to_string();
    let args = args.clone();
    let timeout = Duration::from_secs(config.operation_timeout_seconds);
    let cancellation = cancel.clone();
    let mut work = xylo_resources::spawn_blocking(move || {
        execute_blocking(&config, &name, &args, &cancellation)
    });
    tokio::select! {
        result = &mut work => result?,
        _ = cancel.cancelled() => { cancel.cancel(); work.await? },
        _ = tokio::time::sleep(timeout) => { cancel.cancel(); work.await? }
    }
}

fn execute_blocking(
    config: &CapabilityConfig,
    name: &str,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<ToolObservation> {
    ensure!(!cancel.is_cancelled(), "Document operation cancelled");
    let _admission = if matches!(name, "document_save_copy" | "document_save_status") {
        None
    } else {
        Some(transaction::admission(config, cancel)?)
    };
    match name {
        "document_open" | "document_save_copy" | "document_save_status" => {
            editing::execute(config, name, args, cancel)
        }
        "document_read" | "document_pdf_preview" => {
            let path = input_path(config, required_str(args, "path")?)?;
            ensure!(
                std::fs::metadata(&path)?.len() <= MAX_INPUT,
                "Document exceeds 64MiB input budget"
            );
            let extension = path
                .extension()
                .and_then(|v| v.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            match extension.as_str() {
                "pdf" => pdf(config, &path, name == "document_pdf_preview", args, cancel),
                "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" => {
                    ensure!(name == "document_read", "Preview only supports PDF");
                    if extension != "xls" {
                        preflight_archive(&path)?;
                    }
                    let mut workbook = open_workbook_auto(&path)?;
                    let sheets = workbook.sheet_names();
                    let sheet = args
                        .get("sheet")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| sheets.first().cloned())
                        .context("Workbook has no sheets")?;
                    let range = workbook.worksheet_range(&sheet)?;
                    let limit = args
                        .get("max_rows")
                        .and_then(Value::as_u64)
                        .unwrap_or(200)
                        .clamp(1, 2000) as usize;
                    let rows: Vec<Vec<String>> = range
                        .rows()
                        .take(limit)
                        .map(|r| r.iter().take(128).map(ToString::to_string).collect())
                        .collect();
                    Ok(observation(
                        json!({"source":path,"sheet":sheet,"sheets":sheets,"rows":rows,
                        "total_rows":range.height(),"total_columns":range.width(),"truncated":range.height()>limit || range.width()>128}),
                        vec![],
                    ))
                }
                "docx" => {
                    ensure!(name == "document_read", "Preview only supports PDF");
                    preflight_archive(&path)?;
                    let mut archive = zip::ZipArchive::new(File::open(&path)?)?;
                    let mut xml = String::new();
                    archive
                        .by_name("word/document.xml")?
                        .take(8 * 1024 * 1024)
                        .read_to_string(&mut xml)?;
                    let mut reader = quick_xml::Reader::from_str(&xml);
                    let mut text = String::new();
                    loop {
                        match reader.read_event()? {
                            quick_xml::events::Event::Text(t) => {
                                text.push_str(&quick_xml::escape::unescape(&t.decode()?)?)
                            }
                            quick_xml::events::Event::GeneralRef(reference) => {
                                text.push_str(&quick_xml::escape::unescape(&format!(
                                    "&{};",
                                    reference.decode()?
                                ))?);
                            }
                            quick_xml::events::Event::End(e) if e.name().as_ref() == b"w:p" => {
                                text.push('\n')
                            }
                            quick_xml::events::Event::Eof => break,
                            _ => (),
                        }
                        ensure!(!cancel.is_cancelled(), "Document operation cancelled");
                    }
                    Ok(observation(json!({"source":path,"text":text}), vec![]))
                }
                "txt" | "md" | "csv" | "json" | "html" | "xml" | "log" | "yaml" | "yml" => {
                    ensure!(name == "document_read", "Preview only supports PDF");
                    let mut bytes = Vec::new();
                    File::open(&path)?
                        .take(config.max_output_bytes as u64 + 1)
                        .read_to_end(&mut bytes)?;
                    let truncated = bytes.len() > config.max_output_bytes;
                    bytes.truncate(config.max_output_bytes);
                    Ok(observation(
                        json!({"source":path,"text":String::from_utf8_lossy(&bytes),"truncated":truncated}),
                        vec![],
                    ))
                }
                _ => anyhow::bail!("Unsupported document type: {extension}"),
            }
        }
        "document_export_xlsx" => {
            let path = output_path(config, required_str(args, "name")?, "xlsx")?;
            let rows = args
                .get("rows")
                .and_then(Value::as_array)
                .context("rows must be an array of arrays")?;
            ensure!(rows.len() <= 10_000, "Export exceeds 10000 rows");
            let mut book = rust_xlsxwriter::Workbook::new();
            let sheet = book.add_worksheet();
            sheet.set_name(
                args.get("sheet")
                    .and_then(Value::as_str)
                    .unwrap_or("Sheet1"),
            )?;
            for (r, row) in rows.iter().enumerate() {
                ensure!(!cancel.is_cancelled(), "Document operation cancelled");
                let columns = row.as_array().context("Each row must be an array")?;
                ensure!(columns.len() <= 128, "Export exceeds 128 columns");
                for (c, cell) in columns.iter().enumerate() {
                    match cell {
                        Value::Number(n) => {
                            sheet.write_number(
                                r as u32,
                                c as u16,
                                n.as_f64().context("Number outside spreadsheet range")?,
                            )?;
                        }
                        Value::Null => (),
                        other => {
                            let value = other
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| other.to_string());
                            ensure!(value.chars().count() <= 32_767, "Spreadsheet cell too long");
                            sheet.write_string(r as u32, c as u16, value)?;
                        }
                    }
                }
            }
            let mut staging = transaction::Staging::new(&config.artifacts)?;
            book.save_to_writer(File::create(&staging.path)?)?;
            staging.commit(&path, cancel)?;
            Ok(observation(
                json!({"path":path,"rows":rows.len(),"formulas":"written_as_text"}),
                vec![path],
            ))
        }
        "document_export_docx" => {
            let path = output_path(config, required_str(args, "name")?, "docx")?;
            let paragraphs = args
                .get("paragraphs")
                .and_then(Value::as_array)
                .context("paragraphs must be an array")?;
            ensure!(
                paragraphs.len() <= 10_000 && args.to_string().len() <= 8 * 1024 * 1024,
                "Document export exceeds content budget"
            );
            let mut doc = Docx::new();
            for paragraph in paragraphs {
                ensure!(!cancel.is_cancelled(), "Document operation cancelled");
                doc = doc.add_paragraph(Paragraph::new().add_run(
                    Run::new().add_text(paragraph.as_str().context("Paragraph must be text")?),
                ));
            }
            if let Some(sources) = args.get("sources").and_then(Value::as_array) {
                ensure!(sources.len() <= 100, "Too many source references");
                for source in sources {
                    let url = url::Url::parse(required_str(source, "url")?)?;
                    ensure!(
                        matches!(url.scheme(), "http" | "https"),
                        "Source URL must be HTTP(S)"
                    );
                    let title = required_str(source, "title")?;
                    doc = doc.add_paragraph(
                        Paragraph::new().add_hyperlink(
                            Hyperlink::new(url.as_str(), HyperlinkType::External)
                                .add_run(Run::new().add_text(title)),
                        ),
                    );
                }
            }
            let mut staging = transaction::Staging::new(&config.artifacts)?;
            doc.build().pack(File::create(&staging.path)?)?;
            staging.commit(&path, cancel)?;
            Ok(observation(
                json!({"path":path,"paragraphs":paragraphs.len()}),
                vec![path],
            ))
        }
        _ => anyhow::bail!("Unknown document tool: {name}"),
    }
}

fn pdf(
    config: &CapabilityConfig,
    path: &Path,
    preview: bool,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<ToolObservation> {
    let library = config
        .pdfium_library
        .as_ref()
        .context("Configure a local PDFium dynamic library for PDF extraction/previews")?;
    ensure!(
        library.is_absolute() && library.is_file(),
        "Configured PDFium library is missing"
    );
    // pdfium-render0.9 owns process-global bindings. Bind once, then reuse
    // those bindings for subsequent documents/previews in this helper.
    static LIBRARY: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);
    let canonical = std::fs::canonicalize(library)?;
    let mut loaded = LIBRARY
        .lock()
        .map_err(|_| anyhow::anyhow!("PDFium initialization state unavailable"))?;
    let pdfium = if let Some(selected) = loaded.as_ref() {
        ensure!(
            *selected == canonical,
            "PDFium library changed; restart the native helper before using the new library"
        );
        Pdfium::default()
    } else {
        let pdfium = Pdfium::new(Pdfium::bind_to_library(&canonical)?);
        *loaded = Some(canonical);
        pdfium
    };
    drop(loaded);
    let document = pdfium.load_pdf_from_file(path, args.get("password").and_then(Value::as_str))?;
    let index = args.get("page").and_then(Value::as_u64).unwrap_or(1);
    ensure!(
        index > 0 && index <= document.pages().len() as u64,
        "PDF page outside document range"
    );
    if preview {
        let page = document.pages().get((index - 1) as i32)?;
        let width = args
            .get("width")
            .and_then(Value::as_u64)
            .unwrap_or(1280)
            .clamp(64, 2048) as i32;
        let output = output_path(
            config,
            &format!("pdf-{}-p{index}", uuid::Uuid::new_v4()),
            "png",
        )?;
        page.render_with_config(
            &PdfRenderConfig::new()
                .set_target_width(width)
                .set_maximum_height(2048),
        )?
        .as_image()?
        .save(&output)?;
        Ok(observation(
            json!({"source":path,"page":index,"page_count":document.pages().len(),"preview":output}),
            vec![output],
        ))
    } else {
        let count = args
            .get("max_pages")
            .and_then(Value::as_u64)
            .unwrap_or(10)
            .clamp(1, 20);
        let mut pages = Vec::new();
        for i in (index - 1)..(index - 1 + count).min(document.pages().len() as u64) {
            ensure!(!cancel.is_cancelled(), "Document operation cancelled");
            let page = document.pages().get(i as i32)?;
            let text = page.text()?.all();
            pages.push(json!({"page":i+1,"text":text.chars().take(200_000).collect::<String>(),"truncated":text.chars().count()>200_000}));
        }
        Ok(observation(
            json!({"source":path,"page_count":document.pages().len(),"pages":pages}),
            vec![],
        ))
    }
}

fn save_schema() -> Value {
    let value = json!({"oneOf":[
        {"type":"object","properties":{"type":{"const":"blank"}},"required":["type"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"text"},"text":{"type":"string","maxLength":32767}},"required":["type","text"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"number"},"value":{"type":"string","description":"Finite decimal lexical value, preserved without f64 rounding"}},"required":["type","value"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"boolean"},"value":{"type":"boolean"}},"required":["type","value"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"formula"},"expression":{"type":"string","pattern":"^=","maxLength":8193},"cached":{"type":["string","null"]}},"required":["type","expression"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"error"},"code":{"type":"string"}},"required":["type","code"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"date_time"},"value":{"type":"string"}},"required":["type","value"],"additionalProperties":false}
    ]});
    json!({"operation_id":{"type":"string","format":"uuid"},"name":{"type":"string","maxLength":180},
    "source":{"type":"object","properties":{"path":{"type":"string"},"revision":{"type":"string","pattern":"^[0-9a-f]{64}$"},"bytes":{"type":"integer","minimum":0},"kind":{"type":"string","enum":["text","docx","spreadsheet"]}},"required":["path","revision","bytes","kind"],"additionalProperties":false},
    "edits":{"type":"array","minItems":1,"maxItems":4096,"items":{"oneOf":[
        {"type":"object","properties":{"kind":{"const":"text"},"text":{"type":"string","maxLength":524288}},"required":["kind","text"],"additionalProperties":false},
        {"type":"object","properties":{"kind":{"const":"paragraph"},"index":{"type":"integer","minimum":0},"text":{"type":"string","maxLength":65536}},"required":["kind","index","text"],"additionalProperties":false},
        {"type":"object","properties":{"kind":{"const":"cell"},"sheet":{"type":"string"},"address":{"type":"string","pattern":"^[A-Z]{1,3}[1-9][0-9]{0,6}$"},"value":value},"required":["kind","sheet","address","value"],"additionalProperties":false}
    ]}}})
}

pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        definition(
            "document_open",
            "Open a revision-bound editable UTF-8, DOCX paragraph page or XLSX typed cell page without flattening Office objects",
            json!({"path":{"type":"string"},"expected_revision":{"type":"string"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":200},"sheet":{"type":"string"},"row_start":{"type":"integer","minimum":0},"row_count":{"type":"integer","minimum":1,"maximum":200},"column_start":{"type":"integer","minimum":0},"column_count":{"type":"integer","minimum":1,"maximum":64}}),
            &["path"],
            false,
        ),
        definition(
            "document_save_copy",
            "Save explicit typed edits to a revision-checked copy, preserving unrelated Office parts. Never reuse an operation_id for changed arguments; reconcile uncertain results with document_save_status.",
            save_schema(),
            &["operation_id", "source", "name", "edits"],
            true,
        ),
        definition(
            "document_save_status",
            "Reconcile a document copy operation by its durable operation UUID without repeating the write",
            json!({"operation_id":{"type":"string","format":"uuid"}}),
            &["operation_id"],
            false,
        ),
        definition(
            "document_read",
            "Extract PDF text with PDFium, office spreadsheet values with calamine, DOCX text or bounded UTF-8 text. Reports exact source path and page/sheet.",
            json!({"path":{"type":"string"},"page":{"type":"integer","minimum":1},"max_pages":{"type":"integer","minimum":1,"maximum":20},"max_rows":{"type":"integer","minimum":1,"maximum":2000},"sheet":{"type":"string"},"password":{"type":"string"}}),
            &["path"],
            false,
        ),
        definition(
            "document_pdf_preview",
            "Render one PDF page to a bounded PNG using locally provisioned PDFium.",
            json!({"path":{"type":"string"},"page":{"type":"integer","minimum":1},"width":{"type":"integer","minimum":64,"maximum":2048},"password":{"type":"string"}}),
            &["path"],
            false,
        ),
        definition(
            "document_export_xlsx",
            "Create an XLSX artifact using native rust_xlsxwriter. Text remains text, including strings beginning with '='.",
            json!({"name":{"type":"string"},"sheet":{"type":"string"},"rows":{"type":"array","items":{"type":"array"}}}),
            &["name", "rows"],
            true,
        ),
        definition(
            "document_export_docx",
            "Create an editable DOCX artifact with paragraphs and optional clickable source references.",
            json!({"name":{"type":"string"},"paragraphs":{"type":"array","items":{"type":"string"}},"sources":{"type":"array","items":{"type":"object","properties":{"title":{"type":"string"},"url":{"type":"string"}},"required":["title","url"]}}}),
            &["name", "paragraphs"],
            true,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(root: &Path) -> CapabilityConfig {
        CapabilityConfig {
            workspace: root.canonicalize().unwrap(),
            artifacts: root.canonicalize().unwrap(),
            ..Default::default()
        }
    }
    #[tokio::test]
    async fn office_exports_round_trip_text_and_numbers_offline() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = config(temp.path());
        execute(
            &cfg,
            "document_export_xlsx",
            &json!({"name":"sample","rows":[["=1+1",42], ["Station", "A"]]}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let result = execute(
            &cfg,
            "document_read",
            &json!({"path":"sample.xlsx"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(result.summary.contains("=1+1") && result.summary.contains("42"));
        execute(&cfg, "document_export_docx", &json!({"name":"report","paragraphs":["A & B <C>"],"sources":[{"title":"Source","url":"https://example.com"}]}), CancellationToken::new()).await.unwrap();
        let result = execute(
            &cfg,
            "document_read",
            &json!({"path":"report.docx"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(result.summary.contains("A & B <C>"));
        assert!(
            execute(
                &cfg,
                "document_export_docx",
                &json!({"name":"report","paragraphs":["overwrite"]}),
                CancellationToken::new()
            )
            .await
            .is_err()
        );
    }
}

#[cfg(test)]
mod editing_tests;
