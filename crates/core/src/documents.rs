//! Native document snapshots and conservative, revision-checked copy edits.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    Text,
    Docx,
    Spreadsheet,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentSource {
    pub path: PathBuf,
    pub revision: String,
    pub bytes: u64,
    pub kind: DocumentKind,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentSnapshot {
    pub source: DocumentSource,
    pub content: DocumentContent,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DocumentContent {
    Text {
        text: String,
    },
    Docx {
        paragraphs: Vec<DocumentParagraph>,
        total_paragraphs: u32,
        offset: u32,
        has_more: bool,
    },
    Spreadsheet {
        sheets: Vec<DocumentSheet>,
        selected_sheet: String,
        row_start: u32,
        row_count: u32,
        column_start: u32,
        column_count: u32,
        cells: Vec<DocumentCell>,
        total_rows: u32,
        total_columns: u32,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentParagraph {
    pub index: u32,
    pub text: String,
    pub editable: bool,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentSheet {
    pub name: String,
    pub rows: u32,
    pub columns: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentCell {
    pub address: String,
    pub row: u32,
    pub column: u32,
    pub value: DocumentCellValue,
    pub editable: bool,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DocumentCellValue {
    Blank,
    Text {
        text: String,
    },
    Number {
        value: String,
    },
    Boolean {
        value: bool,
    },
    Formula {
        expression: String,
        cached: Option<String>,
    },
    Error {
        code: String,
    },
    DateTime {
        value: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DocumentEdit {
    Text {
        text: String,
    },
    Paragraph {
        index: u32,
        text: String,
    },
    Cell {
        sheet: String,
        address: String,
        value: DocumentCellValue,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DocumentOpenRequest {
    pub path: String,
    pub expected_revision: Option<String>,
    pub offset: u32,
    pub limit: u32,
    pub sheet: Option<String>,
    pub row_start: u32,
    pub row_count: u32,
    pub column_start: u32,
    pub column_count: u32,
}
impl Default for DocumentOpenRequest {
    fn default() -> Self {
        Self {
            path: String::new(),
            expected_revision: None,
            offset: 0,
            limit: 100,
            sheet: None,
            row_start: 0,
            row_count: 64,
            column_start: 0,
            column_count: 16,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentSaveRequest {
    pub operation_id: Uuid,
    pub source: DocumentSource,
    pub name: String,
    pub edits: Vec<DocumentEdit>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentSaveResult {
    pub path: PathBuf,
    pub source: DocumentSource,
    pub revision: String,
    pub applied_edits: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentSaveState {
    Prepared,
    Committed,
    Cancelled,
    Failed,
    OutcomeUnknown,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentSaveReceipt {
    pub operation_id: Uuid,
    pub state: DocumentSaveState,
    pub result: Option<DocumentSaveResult>,
    pub message: String,
}
