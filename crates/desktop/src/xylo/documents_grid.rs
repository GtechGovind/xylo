//! The native table virtualizes both axes. Only bounded, revision-tagged pages
//! are retained; a missing page is never represented as an editable blank.
use super::*;
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use std::collections::VecDeque;

pub(super) const ROWS: u32 = 64;
pub(super) const COLUMNS: u32 = 16;
const MAX_PAGES: usize = 8;
const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in super::super) struct PageKey {
    pub sheet: String,
    pub row: u32,
    pub column: u32,
}
impl PageKey {
    pub fn at(sheet: String, row: u32, column: u32) -> Self {
        Self {
            sheet,
            row: row / ROWS * ROWS,
            column: column / COLUMNS * COLUMNS,
        }
    }
}
struct Page {
    cells: BTreeMap<(u32, u32), DocumentCell>,
    bytes: usize,
}
pub(super) struct DocumentGrid {
    pub owner: WeakEntity<Xylo>,
    pub epoch: u64,
    pub sheet: String,
    pub rows: u32,
    pub columns: u32,
    pages: BTreeMap<PageKey, Page>,
    order: VecDeque<PageKey>,
    pub failed: BTreeSet<PageKey>,
    pub edits: BTreeMap<(String, String), DocumentCellValue>,
}
impl DocumentGrid {
    pub fn new(
        owner: WeakEntity<Xylo>,
        epoch: u64,
        sheet: String,
        rows: u32,
        columns: u32,
    ) -> Self {
        Self {
            owner,
            epoch,
            sheet,
            rows,
            columns,
            pages: BTreeMap::new(),
            order: VecDeque::new(),
            failed: BTreeSet::new(),
            edits: BTreeMap::new(),
        }
    }
    pub fn insert(&mut self, key: PageKey, cells: Vec<DocumentCell>) {
        self.failed.remove(&key);
        let cache_key = PageKey::at(key.sheet, key.row, key.column);
        let mut page = self.pages.remove(&cache_key).unwrap_or_else(|| Page {
            cells: BTreeMap::new(),
            bytes: 0,
        });
        for cell in cells {
            page.cells.insert((cell.row, cell.column), cell);
        }
        page.bytes = page
            .cells
            .values()
            .map(|cell| {
                cell.address.len()
                    + raw(&cell.value).len()
                    + cell.reason.as_ref().map_or(0, String::len)
                    + 96
            })
            .sum();
        while page.bytes > MAX_CACHE_BYTES {
            let Some(position) = page
                .cells
                .keys()
                .find(|(row, column)| {
                    *row < key.row
                        || *row >= key.row + ROWS
                        || *column < key.column
                        || *column >= key.column + COLUMNS
                })
                .copied()
                .or_else(|| page.cells.keys().next().copied())
            else {
                break;
            };
            if let Some(cell) = page.cells.remove(&position) {
                page.bytes = page.bytes.saturating_sub(
                    cell.address.len()
                        + raw(&cell.value).len()
                        + cell.reason.as_ref().map_or(0, String::len)
                        + 96,
                );
            }
        }
        self.order.retain(|cached| cached != &cache_key);
        while self.pages.len() >= MAX_PAGES
            || (!self.pages.is_empty()
                && self.pages.values().map(|page| page.bytes).sum::<usize>() + page.bytes
                    > MAX_CACHE_BYTES)
        {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            self.pages.remove(&old);
        }
        self.order.push_back(cache_key.clone());
        self.pages.insert(cache_key, page);
    }
    pub fn cell(&self, row: u32, column: u32) -> Option<&DocumentCell> {
        self.pages
            .get(&PageKey::at(self.sheet.clone(), row, column))?
            .cells
            .get(&(row, column))
    }
    pub fn set_edits(&mut self, edits: &[DocumentEdit]) {
        self.edits = edits
            .iter()
            .filter_map(|edit| match edit {
                DocumentEdit::Cell {
                    sheet,
                    address,
                    value,
                } => Some(((sheet.clone(), address.clone()), value.clone())),
                _ => None,
            })
            .collect();
    }
}
impl TableDelegate for DocumentGrid {
    fn columns_count(&self, _: &App) -> usize {
        self.columns as usize + 1
    }
    fn rows_count(&self, _: &App) -> usize {
        self.rows as usize
    }
    fn column(&self, column: usize, _: &App) -> Column {
        Column::new(
            format!("document-column-{column}"),
            if column == 0 {
                String::new()
            } else {
                column_name(column as u32 - 1)
            },
        )
        .width(px(if column == 0 { 56. } else { 132. }))
    }
    fn render_td(
        &mut self,
        row: usize,
        column: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        if column == 0 {
            return div()
                .text_xs()
                .text_color(rgb(muted()))
                .child((row + 1).to_string())
                .into_any_element();
        }
        let row = row as u32;
        let column = column as u32 - 1;
        let key = PageKey {
            sheet: self.sheet.clone(),
            row,
            column,
        };
        let mut protected = false;
        let mut edited = false;
        let value = if let Some(cell) = self.cell(row, column) {
            protected = !cell.editable;
            if let Some(value) = self.edits.get(&(self.sheet.clone(), cell.address.clone())) {
                edited = true;
                preview(value)
            } else {
                preview(&cell.value)
            }
        } else {
            if !self.failed.contains(&key) {
                let owner = self.owner.clone();
                let epoch = self.epoch;
                cx.defer(move |cx| {
                    let _ = owner.update(cx, |view, cx| {
                        view.document_request_grid_page(epoch, key, cx)
                    });
                });
                "…".into()
            } else {
                "Unavailable".into()
            }
        };
        div()
            .size_full()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_sm()
            .when(protected, |view| view.text_color(rgb(muted())))
            .when(edited, |view| view.bg(rgb(lavender())))
            .child(value)
            .into_any_element()
    }
}
pub(super) fn column_name(mut column: u32) -> String {
    let mut name = String::new();
    loop {
        name.insert(0, (b'A' + (column % 26) as u8) as char);
        if column < 26 {
            break;
        }
        column = column / 26 - 1;
    }
    name
}
pub(super) fn address(row: u32, column: u32) -> String {
    format!("{}{}", column_name(column), row + 1)
}
pub(super) fn parse_address(value: &str) -> Option<(u32, u32)> {
    let value = value.trim().to_ascii_uppercase();
    let split = value.find(|c: char| c.is_ascii_digit())?;
    if split == 0 {
        return None;
    }
    let mut column = 0u32;
    for letter in value[..split].bytes() {
        if !letter.is_ascii_uppercase() {
            return None;
        }
        column = column
            .checked_mul(26)?
            .checked_add((letter - b'A' + 1) as u32)?;
    }
    let row = value[split..].parse::<u32>().ok()?.checked_sub(1)?;
    (column <= 16384 && row < 1_048_576).then_some((row, column - 1))
}
pub(super) fn display(value: &DocumentCellValue) -> String {
    match value {
        DocumentCellValue::Blank => String::new(),
        DocumentCellValue::Text { text } => text.clone(),
        DocumentCellValue::Number { value } | DocumentCellValue::DateTime { value } => {
            value.clone()
        }
        DocumentCellValue::Boolean { value } => if *value { "TRUE" } else { "FALSE" }.into(),
        DocumentCellValue::Formula { expression, .. } => expression.clone(),
        DocumentCellValue::Error { code } => code.clone(),
    }
}

fn preview(value: &DocumentCellValue) -> String {
    let raw = raw(value);
    let mut chars = raw.chars();
    let mut label = chars.by_ref().take(256).collect::<String>();
    if chars.next().is_some() {
        label.push('…');
    }
    label
}

fn raw(value: &DocumentCellValue) -> &str {
    match value {
        DocumentCellValue::Blank => "",
        DocumentCellValue::Text { text } => text,
        DocumentCellValue::Number { value } | DocumentCellValue::DateTime { value } => value,
        DocumentCellValue::Boolean { value } => {
            if *value {
                "TRUE"
            } else {
                "FALSE"
            }
        }
        DocumentCellValue::Formula { expression, .. } => expression,
        DocumentCellValue::Error { code } => code,
    }
}
