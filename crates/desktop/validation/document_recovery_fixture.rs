//! Real document package, durable draft and Host; no model, account or picker.
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use uuid::Uuid;
use workbench_core::{AppSettings, DocumentCellValue, DocumentEdit, DocumentKind, DocumentSource};
use workbench_runtime::{
    AppPaths, DocumentDraftKey, DocumentDraftState, DocumentDraftWrite,
    host::{HostClient, HostEvent},
};
use workbench_services::Store;
#[derive(Default)]
pub struct Delivery {
    pub drop_relink: AtomicBool,
    pub dropped: AtomicBool,
}
pub struct Profile {
    pub directory: tempfile::TempDir,
    pub paths: AppPaths,
    pub store: Store,
    pub workspace: PathBuf,
    pub original: PathBuf,
    pub moved: PathBuf,
    pub key: DocumentDraftKey,
    pub state: DocumentDraftState,
}
fn package(path: &Path, entries: &[(&str, &str)]) {
    use std::io::Write;
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, value) in entries {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(value.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}
pub fn seeded(kind: DocumentKind) -> Profile {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let workspace = root.join("project");
    std::fs::create_dir(&workspace).unwrap();
    let extension = match kind {
        DocumentKind::Text => "txt",
        DocumentKind::Docx => "docx",
        DocumentKind::Spreadsheet => "xlsx",
    };
    let original = workspace.join(format!("Original.{extension}"));
    let moved = workspace.join(format!("Moved original.{extension}"));
    match kind {
        DocumentKind::Text => std::fs::write(&original, "Original full document").unwrap(),
        DocumentKind::Docx => package(
            &original,
            &[
                (
                    "[Content_Types].xml",
                    r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#,
                ),
                (
                    "word/document.xml",
                    r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Original paragraph</w:t></w:r></w:p></w:body></w:document>"#,
                ),
                (
                    "custom/keep.xml",
                    "<keep>Unchanged objects and metadata</keep>",
                ),
            ],
        ),
        DocumentKind::Spreadsheet => package(
            &original,
            &[
                (
                    "[Content_Types].xml",
                    r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#,
                ),
                (
                    "xl/workbook.xml",
                    r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/><sheet name="Other" sheetId="2" r:id="rId2"/></sheets></workbook>"#,
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#,
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Original cell</t></is></c><c r="B1"><v>2</v></c></row></sheetData></worksheet>"#,
                ),
                (
                    "xl/worksheets/sheet2.xml",
                    r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Preserve this other sheet</t></is></c></row></sheetData></worksheet>"#,
                ),
                (
                    "custom/keep.xml",
                    "<keep>Unchanged workbook metadata</keep>",
                ),
            ],
        ),
    }
    let bytes = std::fs::read(&original).unwrap();
    let paths = AppPaths::at(root.join("profile")).unwrap();
    let store = Store::open(&paths.database).unwrap();
    store
        .save_settings(&AppSettings {
            last_workspace: Some(workspace.clone()),
            ..Default::default()
        })
        .unwrap();
    let key = DocumentDraftKey {
        workspace: workspace.clone(),
        source: original.clone(),
    };
    let edits = match kind {
        DocumentKind::Text => vec![DocumentEdit::Text {
            text: "Recovered full document α".into(),
        }],
        DocumentKind::Docx => vec![DocumentEdit::Paragraph {
            index: 0,
            text: "Recovered paragraph α".into(),
        }],
        DocumentKind::Spreadsheet => vec![
            DocumentEdit::Cell {
                sheet: "Data".into(),
                address: "A1".into(),
                value: DocumentCellValue::Text {
                    text: "Recovered cell α".into(),
                },
            },
            DocumentEdit::Cell {
                sheet: "Data".into(),
                address: "B1".into(),
                value: DocumentCellValue::Formula {
                    expression: "=2+3".into(),
                    cached: None,
                },
            },
        ],
    };
    let state = DocumentDraftState {
        source: DocumentSource {
            path: original.clone(),
            revision: format!("{:x}", Sha256::digest(&bytes)),
            bytes: bytes.len() as u64,
            kind,
        },
        edits,
        export_name: format!("Recovered copy.{extension}"),
        sheet: if kind == DocumentKind::Spreadsheet {
            Some("Data".into())
        } else {
            None
        },
        selected: if kind == DocumentKind::Spreadsheet {
            Some("A1".into())
        } else {
            None
        },
        pending_copy: None,
        pending_relink: None,
    };
    store
        .save_document_draft(DocumentDraftWrite {
            key: key.clone(),
            expected_revision: 0,
            mutation_id: Uuid::new_v4(),
            state: Some(state.clone()),
        })
        .unwrap();
    std::fs::rename(&original, &moved).unwrap();
    Profile {
        directory,
        paths,
        store,
        workspace,
        original,
        moved,
        key,
        state,
    }
}
pub fn host(
    profile: &Profile,
    delivery: Arc<Delivery>,
) -> (HostClient, async_channel::Receiver<HostEvent>) {
    let (host, incoming) = HostClient::spawn_at(profile.paths.clone()).unwrap();
    let (sender, receiver) = async_channel::bounded(256);
    std::thread::spawn(move || {
        while let Ok(event) = incoming.recv_blocking() {
            if matches!(event, HostEvent::DocumentDraftRelinked { .. })
                && delivery.drop_relink.swap(false, Ordering::SeqCst)
            {
                delivery.dropped.store(true, Ordering::SeqCst);
                continue;
            }
            let stopped = matches!(event, HostEvent::Stopped);
            if sender.send_blocking(event).is_err() || stopped {
                break;
            }
        }
    });
    (host, receiver)
}
