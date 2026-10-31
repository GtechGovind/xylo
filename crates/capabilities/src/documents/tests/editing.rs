use super::super::*;
use super::super::{editing, transaction};
use std::{
    fs,
    io::{Read, Write},
};
use uuid::Uuid;
use xylo_core::*;
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};
fn config(temp: &tempfile::TempDir) -> CapabilityConfig {
    CapabilityConfig {
        workspace: temp.path().canonicalize().unwrap(),
        artifacts: temp.path().canonicalize().unwrap(),
        ..Default::default()
    }
}
fn zip_file(path: &Path, parts: &[(&str, &[u8])]) {
    let mut zip = ZipWriter::new(File::create(path).unwrap());
    zip.set_comment("preserve original comment").unwrap();
    for (name, bytes) in parts {
        zip.start_file(
            *name,
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
}
fn part(path: &Path, name: &str) -> Vec<u8> {
    let mut zip = ZipArchive::new(File::open(path).unwrap()).unwrap();
    let mut bytes = Vec::new();
    zip.by_name(name).unwrap().read_to_end(&mut bytes).unwrap();
    bytes
}
fn raw_part(path: &Path, name: &str) -> Vec<u8> {
    let bytes = fs::read(path).unwrap();
    let mut zip = ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
    let file = zip.by_name(name).unwrap();
    let start = file.data_start().unwrap();
    bytes[start as usize..(start + file.compressed_size()) as usize].to_vec()
}
fn open(cfg: &CapabilityConfig, path: &Path) -> DocumentSnapshot {
    editing::open(
        cfg,
        DocumentOpenRequest {
            path: path.to_string_lossy().into(),
            ..Default::default()
        },
        &CancellationToken::new(),
    )
    .unwrap()
}
fn save(
    cfg: &CapabilityConfig,
    source: DocumentSource,
    name: &str,
    edits: Vec<DocumentEdit>,
) -> DocumentSaveReceipt {
    transaction::save(
        cfg,
        DocumentSaveRequest {
            operation_id: Uuid::new_v4(),
            source,
            name: name.into(),
            edits,
        },
        &CancellationToken::new(),
    )
    .unwrap()
}
const WORD: &str = r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><w:body><w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:rPr><w:b/></w:rPr><w:t>Alpha</w:t></w:r><w:r><w:rPr><w:i/></w:rPr><w:t xml:space="preserve"> Beta</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>Cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl><w:p><w:hyperlink r:id="rLink"><w:r><w:t>Link</w:t></w:r></w:hyperlink></w:p><w:p><w:r><w:drawing><a:blip r:embed="rImage"/></w:drawing></w:r></w:p><w:p><w:r><w:fldChar w:fldCharType="begin"/><w:instrText>DATE</w:instrText></w:r></w:p></w:body></w:document>"#;
#[test]
fn docx_actual_package_edits_table_and_single_styled_run_preserving_links_styles_media_and_raw_entries()
 {
    let temp = tempfile::tempdir().unwrap();
    let cfg = config(&temp);
    let original = temp.path().join("original.docx");
    zip_file(&original,&[("word/document.xml",WORD.as_bytes()),("word/styles.xml",b"<styles><style id='Heading1'/></styles>"),("word/_rels/document.xml.rels",b"<Relationships><Relationship Id='rLink' Target='https://example.com'/></Relationships>"),("word/media/image1.png",b"actual original image bytes"),("customXml/item1.xml",b"<retained/>"),("[Content_Types].xml",b"<Types/>")]);
    let snapshot = open(&cfg, &original);
    let DocumentContent::Docx { paragraphs, .. } = &snapshot.content else {
        panic!()
    };
    assert_eq!(paragraphs.len(), 5);
    assert_eq!(paragraphs[1].text, "Cell");
    assert!(!paragraphs[3].editable && !paragraphs[4].editable);
    let original_hash = transaction::file_hash(&original).unwrap();
    let receipt = save(
        &cfg,
        snapshot.source,
        "edited",
        vec![
            DocumentEdit::Paragraph {
                index: 0,
                text: "Alpine Beta".into(),
            },
            DocumentEdit::Paragraph {
                index: 1,
                text: "New table value".into(),
            },
            DocumentEdit::Paragraph {
                index: 2,
                text: "New Link".into(),
            },
        ],
    );
    assert_eq!(receipt.state, DocumentSaveState::Committed);
    let output = receipt.result.unwrap().path;
    for name in [
        "word/styles.xml",
        "word/_rels/document.xml.rels",
        "word/media/image1.png",
        "customXml/item1.xml",
        "[Content_Types].xml",
    ] {
        assert_eq!(part(&original, name), part(&output, name));
        assert_eq!(raw_part(&original, name), raw_part(&output, name));
    }
    let xml = String::from_utf8(part(&output, "word/document.xml")).unwrap();
    assert!(
        xml.contains("<w:b/>")
            && xml.contains("<w:i/>")
            && xml.contains("w:pStyle w:val=\"Heading1\"")
    );
    assert!(xml.contains("<w:hyperlink r:id=\"rLink\">") && xml.contains("New Link"));
    assert!(xml.contains("r:embed=\"rImage\""));
    assert_eq!(transaction::file_hash(&original).unwrap(), original_hash);
    let DocumentContent::Docx { paragraphs, .. } = open(&cfg, &output).content else {
        panic!()
    };
    assert_eq!(paragraphs[0].text, "Alpine Beta");
    assert_eq!(paragraphs[1].text, "New table value");
}
#[test]
fn unsupported_docx_style_crossing_and_embedded_object_edits_fail_without_publishing() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = config(&temp);
    let source = temp.path().join("source.docx");
    zip_file(&source, &[("word/document.xml", WORD.as_bytes())]);
    let snapshot = open(&cfg, &source);
    for (index, text, name) in [
        (0, "Entire replacement", "crossing"),
        (3, "Replace drawing", "object"),
    ] {
        let receipt = save(
            &cfg,
            snapshot.source.clone(),
            name,
            vec![DocumentEdit::Paragraph {
                index,
                text: text.into(),
            }],
        );
        assert_eq!(receipt.state, DocumentSaveState::Failed);
        assert!(!temp.path().join(format!("{name}.docx")).exists());
    }
}
fn workbook(path: &Path) {
    let mut book = rust_xlsxwriter::Workbook::new();
    let style = rust_xlsxwriter::Format::new().set_bold();
    let first = book.add_worksheet();
    first.set_name("First").unwrap();
    first
        .write_string_with_format(0, 0, "Styled", &style)
        .unwrap();
    first.write_formula(0, 1, "=SUM(1,2)").unwrap();
    first.write_boolean(0, 2, false).unwrap();
    first.merge_range(3, 0, 3, 1, "Merged", &style).unwrap();
    let second = book.add_worksheet();
    second.set_name("Second").unwrap();
    second.write_number(2, 2, 7.0).unwrap();
    let third = book.add_worksheet();
    third.set_name("Untouched").unwrap();
    third
        .write_string(0, 0, "Preserve exact worksheet")
        .unwrap();
    book.save(path).unwrap();
}
#[test]
fn xlsx_paged_typed_cells_and_explicit_formula_edits_preserve_other_sheets_styles_and_precise_numbers()
 {
    let temp = tempfile::tempdir().unwrap();
    let cfg = config(&temp);
    let source = temp.path().join("source.xlsx");
    workbook(&source);
    let snapshot = open(&cfg, &source);
    let DocumentContent::Spreadsheet { cells, sheets, .. } = &snapshot.content else {
        panic!()
    };
    assert_eq!(sheets.len(), 3);
    assert!(
        matches!(&cells[1].value,DocumentCellValue::Formula{expression,..}if expression=="=SUM(1,2)")
    );
    assert!(
        cells
            .iter()
            .filter(|c| matches!(c.address.as_str(), "A4" | "B4"))
            .all(|c| !c.editable)
    );
    assert!(
        cells
            .iter()
            .any(|c| c.address == "D1" && c.value == DocumentCellValue::Blank && c.editable)
    );
    let original = transaction::file_hash(&source).unwrap();
    let receipt = save(
        &cfg,
        snapshot.source,
        "edited",
        vec![
            DocumentEdit::Cell {
                sheet: "First".into(),
                address: "A1".into(),
                value: DocumentCellValue::Text {
                    text: "Styled changed".into(),
                },
            },
            DocumentEdit::Cell {
                sheet: "First".into(),
                address: "B1".into(),
                value: DocumentCellValue::Formula {
                    expression: "=SUM(5,6)".into(),
                    cached: None,
                },
            },
            DocumentEdit::Cell {
                sheet: "First".into(),
                address: "C2".into(),
                value: DocumentCellValue::Number {
                    value: "9007199254740993".into(),
                },
            },
            DocumentEdit::Cell {
                sheet: "First".into(),
                address: "D2".into(),
                value: DocumentCellValue::Text {
                    text: "=HYPERLINK(\"https://example.com\")".into(),
                },
            },
            DocumentEdit::Cell {
                sheet: "First".into(),
                address: "S71".into(),
                value: DocumentCellValue::Boolean { value: true },
            },
            DocumentEdit::Cell {
                sheet: "Second".into(),
                address: "C3".into(),
                value: DocumentCellValue::Number {
                    value: "9.25".into(),
                },
            },
        ],
    );
    assert_eq!(
        receipt.state,
        DocumentSaveState::Committed,
        "{}",
        receipt.message
    );
    let result = receipt.result.unwrap();
    for name in [
        "xl/styles.xml",
        "xl/sharedStrings.xml",
        "xl/worksheets/sheet3.xml",
        "[Content_Types].xml",
    ] {
        assert_eq!(raw_part(&source, name), raw_part(&result.path, name));
    }
    let xml = String::from_utf8(part(&result.path, "xl/worksheets/sheet1.xml")).unwrap();
    assert!(xml.contains("9007199254740993"));
    assert!(xml.contains("<f>SUM(5,6)</f>"));
    assert!(xml.contains("HYPERLINK") && xml.contains("t=\"inlineStr\""));
    assert!(xml.contains("r=\"A1\" s=\"1\"") || xml.contains("s=\"1\""));
    assert!(
        String::from_utf8(part(&result.path, "xl/workbook.xml"))
            .unwrap()
            .contains("fullCalcOnLoad=\"1\"")
    );
    assert_eq!(transaction::file_hash(&source).unwrap(), original);
    let page = editing::open(
        &cfg,
        DocumentOpenRequest {
            path: result.path.to_string_lossy().into(),
            sheet: Some("First".into()),
            row_start: 64,
            row_count: 16,
            column_start: 16,
            column_count: 8,
            ..Default::default()
        },
        &CancellationToken::new(),
    )
    .unwrap();
    let DocumentContent::Spreadsheet { cells, .. } = page.content else {
        panic!()
    };
    assert!(
        cells
            .iter()
            .any(|c| c.address == "S71" && c.value == DocumentCellValue::Boolean { value: true })
    );
    let mut decoded = calamine::open_workbook_auto(&result.path).unwrap();
    assert_eq!(
        decoded
            .worksheet_range("Second")
            .unwrap()
            .get_value((2, 2))
            .unwrap()
            .to_string(),
        "9.25"
    );
}
#[test]
fn text_bom_revision_conflict_atomic_no_clobber_and_idempotent_receipts() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = config(&temp);
    let source = temp.path().join("source.txt");
    fs::write(&source, b"\xef\xbb\xbfOld\r\n").unwrap();
    let snapshot = open(&cfg, &source);
    let request = DocumentSaveRequest {
        operation_id: Uuid::new_v4(),
        source: snapshot.source.clone(),
        name: "copy".into(),
        edits: vec![DocumentEdit::Text {
            text: "New\r\n".into(),
        }],
    };
    let receipt = transaction::save(&cfg, request.clone(), &CancellationToken::new()).unwrap();
    assert_eq!(receipt.state, DocumentSaveState::Committed);
    assert_eq!(
        fs::read(temp.path().join("copy.txt")).unwrap(),
        b"\xef\xbb\xbfNew\r\n"
    );
    assert_eq!(
        transaction::save(&cfg, request.clone(), &CancellationToken::new()).unwrap(),
        receipt
    );
    let mut changed = request;
    changed.edits = vec![DocumentEdit::Text {
        text: "Different".into(),
    }];
    assert!(transaction::save(&cfg, changed, &CancellationToken::new()).is_err());
    fs::write(&source, "Externally changed").unwrap();
    let stale = save(
        &cfg,
        snapshot.source,
        "stale",
        vec![DocumentEdit::Text {
            text: "Overwrite old source".into(),
        }],
    );
    assert_eq!(stale.state, DocumentSaveState::Failed);
    assert!(stale.message.contains("revision"));
    assert!(!temp.path().join("stale.txt").exists());
    assert_eq!(fs::read_to_string(&source).unwrap(), "Externally changed");
    assert!(fs::read_dir(temp.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".partial")
    }));
    assert_eq!(
        transaction::status(&cfg, receipt.operation_id)
            .unwrap()
            .state,
        DocumentSaveState::Committed
    );
}
#[test]
fn cancelled_staging_and_existing_output_are_never_partially_published() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = config(&temp);
    let cancel = CancellationToken::new();
    let mut staging = transaction::Staging::new(&cfg.artifacts).unwrap();
    fs::write(&staging.path, b"complete temporary zip content").unwrap();
    cancel.cancel();
    let output = temp.path().join("never.docx");
    assert!(staging.commit(&output, &cancel).is_err());
    drop(staging);
    assert!(!output.exists());
    fs::write(&output, b"original output").unwrap();
    let mut staging = transaction::Staging::new(&cfg.artifacts).unwrap();
    fs::write(&staging.path, b"different copy").unwrap();
    assert!(staging.commit(&output, &CancellationToken::new()).is_err());
    assert_eq!(fs::read(output).unwrap(), b"original output");
}
#[tokio::test]
async fn typed_page_budget_is_rejected_without_truncation_and_admission_cancellation_is_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let mut cfg = config(&temp);
    let source = temp.path().join("long.txt");
    fs::write(&source, "x".repeat(2000)).unwrap();
    cfg.max_output_bytes = 1024;
    assert!(
        execute(
            &cfg,
            "document_open",
            &json!({"path":source}),
            CancellationToken::new()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("response budget")
    );
    let lock = transaction::admission(&cfg, &CancellationToken::new()).unwrap();
    let cancel = CancellationToken::new();
    let args = json!({"path":source});
    let operation = execute(&cfg, "document_open", &args, cancel.clone());
    let result = tokio::join!(operation, async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();
    });
    assert!(result.0.unwrap_err().to_string().contains("cancelled"));
    drop(lock);
}

#[test]
fn abandoned_copy_receipts_reconcile_output_and_live_lease_prevents_false_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = config(&temp);
    let source = temp.path().join("original.txt");
    fs::write(&source, "Original").unwrap();
    let snapshot = open(&cfg, &source);
    let receipt = save(
        &cfg,
        snapshot.source,
        "copy",
        vec![DocumentEdit::Text {
            text: "Saved content".into(),
        }],
    );
    let path = cfg
        .artifacts
        .join(".document-operations")
        .join(format!("{}.json", receipt.operation_id));
    let mut job: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    job["receipt"]["state"] = json!("prepared");
    fs::write(&path, serde_json::to_vec(&job).unwrap()).unwrap();
    let lease = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&lease).unwrap();
    assert_eq!(
        transaction::status(&cfg, receipt.operation_id)
            .unwrap()
            .state,
        DocumentSaveState::Prepared
    );
    drop(lease);
    assert_eq!(
        transaction::status(&cfg, receipt.operation_id)
            .unwrap()
            .state,
        DocumentSaveState::Committed
    );
    let missing = Uuid::new_v4();
    job["receipt"]["operation_id"] = json!(missing);
    job["receipt"]["result"]["path"] = json!(cfg.artifacts.join("unpublished.txt"));
    let missing_path = path.with_file_name(format!("{missing}.json"));
    fs::write(&missing_path, serde_json::to_vec(&job).unwrap()).unwrap();
    assert_eq!(
        transaction::status(&cfg, missing).unwrap().state,
        DocumentSaveState::Cancelled
    );
    let uncertain = Uuid::new_v4();
    job["receipt"]["operation_id"] = json!(uncertain);
    job["receipt"]["result"]["path"] = json!(receipt.result.unwrap().path);
    job["receipt"]["result"]["revision"] = json!("0".repeat(64));
    fs::write(
        path.with_file_name(format!("{uncertain}.json")),
        serde_json::to_vec(&job).unwrap(),
    )
    .unwrap();
    assert_eq!(
        transaction::status(&cfg, uncertain).unwrap().state,
        DocumentSaveState::OutcomeUnknown
    );
    assert_eq!(
        transaction::status(&cfg, Uuid::new_v4()).unwrap().state,
        DocumentSaveState::Cancelled
    );
}
#[test]
fn signed_office_and_ancestor_bound_content_are_explicitly_protected() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = config(&temp);
    let signed = temp.path().join("signed.docx");
    zip_file(
        &signed,
        &[
            ("word/document.xml", WORD.as_bytes()),
            ("_xmlsignatures/origin.sigs", b""),
        ],
    );
    assert!(
        editing::open(
            &cfg,
            DocumentOpenRequest {
                path: signed.to_string_lossy().into(),
                ..Default::default()
            },
            &CancellationToken::new()
        )
        .unwrap_err()
        .to_string()
        .contains("signed")
    );
    zip_file(&signed,&[("word/document.xml",WORD.as_bytes()),("_rels/.rels",br#"<Relationships><Relationship Id="s" Type="http://schemas.openxmlformats.org/package/2006/relationships/digital-signature&#47;origin" Target="signature.xml"/></Relationships>"#)]);
    assert!(
        editing::open(
            &cfg,
            DocumentOpenRequest {
                path: signed.to_string_lossy().into(),
                ..Default::default()
            },
            &CancellationToken::new()
        )
        .unwrap_err()
        .to_string()
        .contains("signed")
    );
    let bound=WORD.replace("</w:body>","<w:sdt><w:sdtContent><w:p><w:r><w:t>Bound control</w:t></w:r></w:p></w:sdtContent></w:sdt></w:body>");
    let source = temp.path().join("bound.docx");
    zip_file(&source, &[("word/document.xml", bound.as_bytes())]);
    let snapshot = open(&cfg, &source);
    let DocumentContent::Docx { paragraphs, .. } = &snapshot.content else {
        panic!()
    };
    assert!(!paragraphs[5].editable);
    assert!(
        paragraphs[5]
            .reason
            .as_deref()
            .unwrap()
            .contains("content control")
    );
    let receipt = save(
        &cfg,
        snapshot.source,
        "no-bound-edit",
        vec![DocumentEdit::Paragraph {
            index: 5,
            text: "Changed binding".into(),
        }],
    );
    assert_eq!(receipt.state, DocumentSaveState::Failed);
    assert!(!temp.path().join("no-bound-edit.docx").exists());
}
#[tokio::test]
async fn full_length_cell_remains_editable_through_focused_page_after_explicit_page_budget_error() {
    let temp = tempfile::tempdir().unwrap();
    let mut cfg = config(&temp);
    cfg.max_output_bytes = 64 * 1024;
    let source = temp.path().join("long.xlsx");
    let mut book = rust_xlsxwriter::Workbook::new();
    book.add_worksheet()
        .write_string(0, 0, "x".repeat(32767))
        .unwrap();
    book.save(&source).unwrap();
    assert!(
        execute(
            &cfg,
            "document_open",
            &json!({"path":source}),
            CancellationToken::new()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("response budget")
    );
    let observed = execute(
        &cfg,
        "document_open",
        &json!({"path":source,"row_count":1,"column_count":1}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let snapshot: DocumentSnapshot = serde_json::from_str(&observed.summary).unwrap();
    let DocumentContent::Spreadsheet {
        cells,
        row_count,
        column_count,
        ..
    } = snapshot.content
    else {
        panic!()
    };
    assert_eq!((row_count, column_count), (1, 1));
    assert!(cells[0].editable);
    assert!(matches!(&cells[0].value,DocumentCellValue::Text{text}if text.len()==32767));
}
#[test]
fn source_revision_is_checked_again_after_copy_materialization_before_publication() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = config(&temp);
    let source = temp.path().join("source.txt");
    fs::write(&source, "first").unwrap();
    let snapshot = open(&cfg, &source);
    let request = DocumentSaveRequest {
        operation_id: Uuid::new_v4(),
        source: snapshot.source,
        name: "copy".into(),
        edits: vec![DocumentEdit::Text {
            text: "edited".into(),
        }],
    };
    let staging = transaction::Staging::new(&cfg.artifacts).unwrap();
    editing::build(&cfg, &request, &staging.path, &CancellationToken::new()).unwrap();
    fs::write(&source, "other").unwrap();
    assert!(
        editing::verify(&cfg, &request.source, &CancellationToken::new())
            .unwrap_err()
            .to_string()
            .contains("revision")
    );
    drop(staging);
    assert!(!cfg.artifacts.join("copy.txt").exists());
    assert_eq!(fs::read_to_string(source).unwrap(), "other");
}

#[test]
fn status_before_receipt_observes_live_start_or_tombstones_id_so_delayed_save_cannot_publish() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = config(&temp);
    let source = temp.path().join("source.txt");
    fs::write(&source, "Original").unwrap();
    let snapshot = open(&cfg, &source);
    let id = Uuid::new_v4();
    let operations = cfg.artifacts.join(".document-operations");
    fs::create_dir_all(&operations).unwrap();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(operations.join(format!("{id}.lock")))
        .unwrap();
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    assert_eq!(
        transaction::status(&cfg, id).unwrap().state,
        DocumentSaveState::Prepared
    );
    assert!(!operations.join(format!("{id}.json")).exists());
    drop(lock);
    assert_eq!(
        transaction::status(&cfg, id).unwrap().state,
        DocumentSaveState::Cancelled
    );
    let request = DocumentSaveRequest {
        operation_id: id,
        source: snapshot.source,
        name: "Delayed copy.txt".into(),
        edits: vec![DocumentEdit::Text {
            text: "Must not publish".into(),
        }],
    };
    assert_eq!(
        transaction::save(&cfg, request.clone(), &CancellationToken::new())
            .unwrap()
            .state,
        DocumentSaveState::Cancelled
    );
    assert!(!cfg.artifacts.join("Delayed copy.txt").exists());
    let mut changed = request;
    changed.name = "Changed identity.txt".into();
    assert!(transaction::save(&cfg, changed, &CancellationToken::new()).is_err());
}
#[test]
fn friendly_unicode_copy_filenames_are_portable_and_never_escape_or_clobber() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = config(&temp);
    let source = temp.path().join("source.txt");
    fs::write(&source, "Original").unwrap();
    let snapshot = open(&cfg, &source);
    let receipt = save(
        &cfg,
        snapshot.source.clone(),
        "Notes copy नमस्ते.txt",
        vec![DocumentEdit::Text {
            text: "Saved".into(),
        }],
    );
    assert_eq!(receipt.state, DocumentSaveState::Committed);
    assert!(cfg.artifacts.join("Notes copy नमस्ते.txt").is_file());
    for name in [
        "../escape",
        "folder/copy.txt",
        "folder\\copy.txt",
        "CON.txt",
        "name.docx",
        "name.txt ",
        "name?.txt",
    ] {
        assert!(
            transaction::save(
                &cfg,
                DocumentSaveRequest {
                    operation_id: Uuid::new_v4(),
                    source: snapshot.source.clone(),
                    name: name.into(),
                    edits: vec![DocumentEdit::Text { text: "No".into() }]
                },
                &CancellationToken::new()
            )
            .is_err(),
            "{name}"
        );
    }
}
#[test]
fn small_sheet_pages_ignore_an_unrelated_sheet_beyond_native_materialization_limit() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = config(&temp);
    let source = temp.path().join("large-other.xlsx");
    let mut large = String::from(
        "<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><dimension ref=\"A1:A100001\"/><sheetData>",
    );
    for row in 1..=100001 {
        large.push_str(&format!(
            "<row r=\"{row}\"><c r=\"A{row}\"><v>1</v></c></row>"
        ));
    }
    large.push_str("</sheetData></worksheet>");
    zip_file(&source,&[("xl/workbook.xml",br#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Small" r:id="s"/><sheet name="Large" r:id="l"/></sheets></workbook>"#),("xl/_rels/workbook.xml.rels",br#"<Relationships><Relationship Id="s" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/small.xml"/><Relationship Id="l" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/large.xml"/></Relationships>"#),("xl/worksheets/small.xml",br#"<worksheet><dimension ref="A1"/><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Small page</t></is></c></row></sheetData></worksheet>"#),("xl/worksheets/large.xml",large.as_bytes())]);
    for _ in 0..2 {
        let snapshot = editing::open(
            &cfg,
            DocumentOpenRequest {
                path: source.to_string_lossy().into(),
                sheet: Some("Small".into()),
                row_count: 1,
                column_count: 1,
                ..Default::default()
            },
            &CancellationToken::new(),
        )
        .unwrap();
        let DocumentContent::Spreadsheet { cells, sheets, .. } = snapshot.content else {
            panic!()
        };
        assert_eq!(sheets[1].rows, 100001);
        assert_eq!(
            cells[0].value,
            DocumentCellValue::Text {
                text: "Small page".into()
            }
        );
    }
}
