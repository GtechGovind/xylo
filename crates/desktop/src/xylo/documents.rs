//! Imported documents retain their source revision and structured edits. Office
//! parsing and writing run in the supervised helper, never the rendering thread.
use super::*;
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use serde_json::{Value, json};
use xylo_runtime::{DocumentCopyIntent, DocumentDraftState};
#[path = "documents_copy.rs"]
mod copy;
#[path = "documents_grid.rs"]
mod grid;
#[path = "documents_missing.rs"]
mod missing;
#[path = "documents_drafts.rs"]
mod recovery;
#[path = "documents_view.rs"]
mod view;
use grid::{COLUMNS, DocumentGrid, PageKey, ROWS, address, display, parse_address};

#[derive(Clone)]
enum ReadKind {
    Initial,
    Locate,
    Create,
    Grid(PageKey),
    Paragraphs,
}
struct Read {
    epoch: u64,
    kind: ReadKind,
    args: Value,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ValueKind {
    Auto,
    Text,
    Number,
    Boolean,
    Formula,
    DateTime,
    Blank,
}
impl ValueKind {
    const ALL: [Self; 6] = [
        Self::Auto,
        Self::Text,
        Self::Number,
        Self::Boolean,
        Self::Formula,
        Self::Blank,
    ];
    fn label(self) -> &'static str {
        match self {
            Self::Auto => "Automatic",
            Self::Text => "Text",
            Self::Number => "Number",
            Self::Boolean => "True / false",
            Self::Formula => "Formula",
            Self::DateTime => "Date / time",
            Self::Blank => "Blank",
        }
    }
}
pub(super) struct DocumentUi {
    pub text: Entity<TextareaState>,
    pub value: Entity<InputState>,
    pub coordinate: Entity<InputState>,
    pub name: Entity<InputState>,
    subscriptions: Vec<Subscription>,
    grid_subscription: Option<Subscription>,
    grid: Option<Entity<TableState<DocumentGrid>>>,
    pub source: Option<DocumentSource>,
    snapshot: Option<DocumentSnapshot>,
    pub edits: Vec<DocumentEdit>,
    pub generation: u64,
    pub pending_copy: Option<DocumentCopyIntent>,
    copy_request: Option<Uuid>,
    copy_needs_dispatch: bool,
    reads: BTreeMap<Uuid, Read>,
    epoch: u64,
    selected: Option<(u32, u32)>,
    paragraph: Option<u32>,
    selected_sheet: Option<String>,
    value_kind: ValueKind,
    restoring: bool,
    recovery_applied: bool,
    loading_paused: bool,
    page_rows: u32,
    page_columns: u32,
    paragraph_limit: u32,
    source_conflict: bool,
    source_unavailable: bool,
    pending_relink: Option<Box<xylo_runtime::DocumentDraftRelinkRequest>>,
    relink_request: Option<(Uuid, xylo_runtime::DocumentDraftRelinkRequest)>,
    relink_needs_dispatch: bool,
    relink_watchdog: Option<Task<()>>,
    recovered_selection: usize,
    notice: String,
    output: Option<PathBuf>,
    pub drafts: recovery::DocumentDraftUi,
}
impl DocumentUi {
    pub fn new(window: &mut Window, cx: &mut Context<Xylo>) -> Self {
        Self {
            text: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(12)
                    .placeholder("Document text")
            }),
            value: cx.new(|cx| InputState::new(window, cx).placeholder("Cell value")),
            coordinate: cx.new(|cx| InputState::new(window, cx).placeholder("A1")),
            name: cx.new(|cx| InputState::new(window, cx).placeholder("Copy name")),
            subscriptions: Vec::new(),
            grid_subscription: None,
            grid: None,
            source: None,
            snapshot: None,
            edits: Vec::new(),
            generation: 0,
            pending_copy: None,
            copy_request: None,
            copy_needs_dispatch: false,
            reads: BTreeMap::new(),
            epoch: 0,
            selected: None,
            paragraph: None,
            selected_sheet: None,
            value_kind: ValueKind::Auto,
            restoring: false,
            recovery_applied: false,
            loading_paused: false,
            page_rows: ROWS,
            page_columns: COLUMNS,
            paragraph_limit: 100,
            source_conflict: false,
            source_unavailable: false,
            pending_relink: None,
            relink_request: None,
            relink_needs_dispatch: false,
            relink_watchdog: None,
            recovered_selection: 0,
            notice: String::new(),
            output: None,
            drafts: Default::default(),
        }
    }
    fn ready(&self) -> bool {
        self.source.is_some()
            && self.snapshot.is_some()
            && !self.source_conflict
            && !self.source_unavailable
            && self.pending_relink.is_none()
            && self.drafts.loaded()
    }
    pub fn busy(&self) -> bool {
        !self.reads.is_empty()
            || self.copy_request.is_some()
            || self.pending_copy.is_some()
            || self.relink_request.is_some()
            || self.pending_relink.is_some()
    }
}
impl Xylo {
    pub(super) fn document_owns_request(&self, id: Uuid) -> bool {
        self.documents.reads.contains_key(&id) || self.documents.copy_request == Some(id)
    }
    fn document_create(&mut self, spreadsheet: bool, cx: &mut Context<Self>) {
        if !self.workspace_ready || !self.document_before_workspace_switch(cx) {
            return;
        }
        let id = Uuid::new_v4();
        // Each explicit click chooses a new source name; existing artifacts are
        // create-new only, so this action cannot overwrite a user's document.
        let name = format!(
            "{}-{}",
            if spreadsheet {
                "Spreadsheet"
            } else {
                "Document"
            },
            &id.to_string()[..8]
        );
        let (tool, args) = if spreadsheet {
            (
                "document_export_xlsx",
                json!({"name":name,"rows":[[null]],"sheet":"Sheet1"}),
            )
        } else {
            (
                "document_export_docx",
                json!({"name":name,"paragraphs":[""]}),
            )
        };
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.capability(id, tool.into(), args));
        if result.is_ok() {
            self.documents.reads.insert(
                id,
                Read {
                    epoch: self.documents.epoch,
                    kind: ReadKind::Create,
                    args: Value::Null,
                },
            );
            self.documents.notice = "Creating document…".into();
        }
        self.result(result, cx);
    }
    pub(super) fn subscribe_documents(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.documents.subscriptions = vec![
            cx.subscribe(&self.documents.text, |view, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    view.document_text_changed(cx);
                }
            }),
            cx.subscribe(&self.documents.value, |view, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    view.document_value_changed(cx);
                }
            }),
            cx.subscribe(&self.documents.name, |view, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    view.document_draft_changed(cx);
                }
            }),
            cx.subscribe_in(
                &self.documents.coordinate,
                window,
                |view, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        let value = view.documents.coordinate.read(cx).value().to_string();
                        if let Some((row, column)) = parse_address(&value) {
                            view.document_select_cell(row, column, window, cx);
                        } else {
                            view.documents.notice = "Enter a cell address such as A1.".into();
                            cx.notify();
                        }
                    }
                },
            ),
        ];
    }
    pub(super) fn document_open(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.workspace_ready || self.workspace.is_none() {
            self.documents.notice = "Open a project before opening a document.".into();
            cx.notify();
            return;
        }
        if !self.document_before_workspace_switch(cx) {
            return;
        }
        self.document_cancel_reads(cx);
        self.documents.epoch = self.documents.epoch.saturating_add(1);
        self.documents.source = None;
        self.documents.snapshot = None;
        self.documents.edits.clear();
        self.documents.grid = None;
        self.documents.grid_subscription = None;
        self.documents.paragraph = None;
        self.documents.selected = None;
        self.documents.selected_sheet = None;
        self.documents.pending_copy = None;
        self.documents.copy_request = None;
        self.documents.output = None;
        self.documents.source_conflict = false;
        self.documents.source_unavailable = false;
        self.documents.pending_relink = None;
        self.documents.relink_request = None;
        self.documents.relink_watchdog = None;
        self.documents.relink_needs_dispatch = false;
        self.documents.recovered_selection = 0;
        self.documents.recovery_applied = false;
        self.documents.notice = "Opening document…".into();
        self.documents.loading_paused = false;
        self.documents.page_rows = ROWS;
        self.documents.page_columns = COLUMNS;
        self.documents.paragraph_limit = 100;
        self.documents.restoring = true;
        self.documents
            .text
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.documents.name.update(cx, |input, cx| {
            input.set_value(default_name(&path), window, cx)
        });
        self.documents.restoring = false;
        self.documents.drafts.clear_active();
        self.document_read(ReadKind::Initial, json!({"path":path}), cx);
        self.page = Page::Artifacts;
        cx.notify();
    }
    fn document_read(&mut self, kind: ReadKind, args: Value, cx: &mut Context<Self>) {
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.capability(id, "document_open".into(), args.clone()));
        if result.is_ok() {
            self.documents.reads.insert(
                id,
                Read {
                    epoch: self.documents.epoch,
                    kind,
                    args,
                },
            );
        } else {
            self.documents.notice =
                "This document could not be opened. Retry when the application is ready.".into();
        }
        self.result(result, cx);
    }
    pub(super) fn document_cancel_reads(&mut self, cx: &mut Context<Self>) {
        for id in self.documents.reads.keys() {
            if let Some(host) = &self.host {
                let _ = host.cancel_capability(*id);
            }
        }
        self.documents.reads.clear();
        self.documents.loading_paused = true;
        self.documents.notice = "Loading cancelled. Your edits are still saved here.".into();
        cx.notify();
    }
    pub(super) fn document_request_grid_page(
        &mut self,
        epoch: u64,
        key: PageKey,
        cx: &mut Context<Self>,
    ) {
        if self.documents.loading_paused
            || epoch != self.documents.epoch
            || self.documents.reads.len() >= 8
            || self.documents.source_conflict
        {
            return;
        }
        let Some(grid) = &self.documents.grid else {
            return;
        };
        if grid.read(cx).delegate().cell(key.row, key.column).is_some() {
            return;
        }
        let key = PageKey {
            sheet: key.sheet,
            row: key.row / self.documents.page_rows * self.documents.page_rows,
            column: key.column / self.documents.page_columns * self.documents.page_columns,
        };
        if grid.read(cx).delegate().failed.contains(&key)
            || self
                .documents
                .reads
                .values()
                .any(|read| matches!(&read.kind,ReadKind::Grid(pending) if pending==&key))
        {
            return;
        }
        let Some(source) = &self.documents.source else {
            return;
        };
        let args = json!({"path":source.path,"expected_revision":source.revision,"sheet":key.sheet,"row_start":key.row,"row_count":self.documents.page_rows,"column_start":key.column,"column_count":self.documents.page_columns});
        self.document_read(ReadKind::Grid(key), args, cx);
    }
    fn document_read_paragraphs(&mut self, offset: u32, cx: &mut Context<Self>) {
        let Some(source) = &self.documents.source else {
            return;
        };
        if !self.documents.reads.is_empty() {
            return;
        }
        self.document_read(ReadKind::Paragraphs,json!({"path":source.path,"expected_revision":source.revision,"offset":offset,"limit":self.documents.paragraph_limit}),cx);
    }
    pub(super) fn document_capability_result(
        &mut self,
        id: Uuid,
        _name: &str,
        result: Result<Value, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let read = self.documents.reads.remove(&id);
        let is_copy = self.documents.copy_request == Some(id);
        if read.is_none() && !is_copy {
            return false;
        }
        if let Some(read) = &read
            && read.epoch != self.documents.epoch
        {
            return true;
        }
        if is_copy {
            self.documents.copy_request = None;
        }
        let parsed = result.and_then(|value| match value.get("summary").and_then(Value::as_str) {
            Some(summary) => serde_json::from_str(summary)
                .map_err(|_| "The document response could not be read.".into()),
            None => Ok(value),
        });
        match parsed {
            Ok(value) if is_copy => match serde_json::from_value::<DocumentSaveReceipt>(value) {
                Ok(receipt) => self.document_copy_receipt(receipt, window, cx),
                Err(_) => {
                    self.documents.notice =
                        "The copy's result is uncertain. Check its status before saving again."
                            .into();
                }
            },
            Ok(value) if matches!(read.as_ref().map(|read| &read.kind), Some(ReadKind::Create)) => {
                if let Some(path) = value.get("path").and_then(Value::as_str) {
                    self.document_open(PathBuf::from(path), window, cx);
                } else {
                    self.documents.notice = "The new document could not be opened.".into();
                }
            }
            Ok(value) => match serde_json::from_value::<DocumentSnapshot>(value) {
                Ok(snapshot)
                    if matches!(read.as_ref().map(|read| &read.kind), Some(ReadKind::Locate)) =>
                {
                    self.document_found_original(snapshot, window, cx)
                }
                Ok(snapshot) => self.document_snapshot(snapshot, read.unwrap().kind, window, cx),
                Err(_) => {
                    self.documents.notice = "This document could not be displayed safely.".into()
                }
            },
            Err(error) => {
                if matches!(
                    read.as_ref().map(|read| &read.kind),
                    Some(ReadKind::Initial)
                ) {
                    self.documents.source_unavailable = true;
                }
                if error.contains("Document page exceeds response budget")
                    && let Some(read) = read.as_ref()
                    && !matches!(read.kind, ReadKind::Create)
                {
                    let mut args = read.args.clone();
                    let rows = args["row_count"].as_u64().unwrap_or(64);
                    let columns = args["column_count"].as_u64().unwrap_or(16);
                    let limit = args["limit"].as_u64().unwrap_or(100);
                    if rows > 1 || columns > 1 || limit > 1 {
                        let smaller = if rows > 4 || columns > 4 || limit > 4 {
                            4
                        } else {
                            1
                        };
                        self.documents.page_rows = smaller;
                        self.documents.page_columns = smaller;
                        self.documents.paragraph_limit = smaller;
                        args["row_count"] = json!(smaller);
                        args["column_count"] = json!(smaller);
                        args["limit"] = json!(smaller);
                        self.document_read(read.kind.clone(), args, cx);
                        self.documents.notice =
                            "Loading a smaller section of this document…".into();
                        return true;
                    }
                }
                if is_copy && error.contains("No saved document operation exists for this ID; no copy was dispatched under it") {
                    self.documents.pending_copy=None;self.document_draft_changed(cx);self.document_persist_active(cx);
                    self.documents.notice="No copy was created. Your edits remain saved; choose Save a copy when ready.".into();
                    cx.notify();return true;
                }
                if let Some(Read {
                    kind: ReadKind::Grid(key),
                    ..
                }) = read.as_ref()
                    && let Some(grid) = &self.documents.grid
                {
                    grid.update(cx, |grid, cx| {
                        grid.delegate_mut().failed.insert(key.clone());
                        cx.notify();
                    });
                }
                self.documents.notice = if matches!(
                    read.as_ref().map(|read| &read.kind),
                    Some(ReadKind::Locate)
                ) {
                    "That file differs from the saved original or cannot be opened. Your recovered edits stay saved; choose the unchanged original.".into()
                } else if matches!(
                    read.as_ref().map(|read| &read.kind),
                    Some(ReadKind::Initial)
                ) && self.documents.source.is_some()
                {
                    "The original could not be opened. Your recovered edits stay saved; find the moved original or inspect the retained changes.".into()
                } else if is_copy {
                    format!(
                        "The copy's result could not be confirmed. Check its status before saving again. {error}"
                    )
                } else {
                    error
                };
            }
        }
        self.advance_document_drafts_flush(window, cx);
        cx.notify();
        true
    }
    fn document_snapshot(
        &mut self,
        snapshot: DocumentSnapshot,
        kind: ReadKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(kind, ReadKind::Initial)
            && self
                .documents
                .source
                .as_ref()
                .is_none_or(|source| source != &snapshot.source)
        {
            self.documents.notice="The source changed. Your edits are preserved; reopen it before making another copy.".into();
            self.documents.source_conflict = true;
            return;
        }
        if matches!(kind, ReadKind::Initial) {
            if !self.documents.drafts.is_active_path(&snapshot.source.path) {
                self.document_draft_activate(snapshot.source.path.clone(), cx);
            }
            if self.documents.recovery_applied
                && self
                    .documents
                    .source
                    .as_ref()
                    .is_some_and(|source| source != &snapshot.source)
            {
                self.documents.source_conflict = true;
            }
            if !self.documents.recovery_applied && !self.documents.source_conflict {
                self.documents.source = Some(snapshot.source.clone());
            }
            self.documents.source_unavailable = false;
            self.documents.snapshot = Some(snapshot.clone());
            self.document_apply_recovery(window, cx);
        }
        if self.documents.source_conflict {
            return;
        }
        match &snapshot.content {
            DocumentContent::Spreadsheet {
                selected_sheet,
                total_rows,
                total_columns,
                cells,
                row_start,
                column_start,
                ..
            } => {
                if self
                    .documents
                    .selected_sheet
                    .as_ref()
                    .is_some_and(|sheet| sheet != selected_sheet)
                    && !matches!(kind, ReadKind::Initial)
                {
                    return;
                }
                if matches!(kind, ReadKind::Initial)
                    && self
                        .documents
                        .selected_sheet
                        .as_ref()
                        .is_some_and(|sheet| sheet != selected_sheet)
                {
                    let sheet = self.documents.selected_sheet.clone();
                    self.document_read(ReadKind::Initial,json!({"path":snapshot.source.path,"expected_revision":snapshot.source.revision,"sheet":sheet,"row_count":self.documents.page_rows,"column_count":self.documents.page_columns}),cx);
                    return;
                }
                self.documents.selected_sheet = Some(selected_sheet.clone());
                if self.documents.grid.is_none() || matches!(kind, ReadKind::Initial) {
                    let delegate = DocumentGrid::new(
                        cx.entity().downgrade(),
                        self.documents.epoch,
                        selected_sheet.clone(),
                        total_rows.saturating_add(64).min(1_048_576),
                        total_columns.saturating_add(16).min(16_384),
                    );
                    let grid =
                        cx.new(|cx| TableState::new(delegate, window, cx).cell_selectable(true));
                    self.documents.grid_subscription = Some(cx.subscribe_in(
                        &grid,
                        window,
                        |view, _, event: &TableEvent, window, cx| match event {
                            TableEvent::SelectCell(row, column)
                            | TableEvent::DoubleClickedCell(row, column)
                                if *column > 0 =>
                            {
                                view.document_select_cell(
                                    *row as u32,
                                    *column as u32 - 1,
                                    window,
                                    cx,
                                );
                                if matches!(event, TableEvent::DoubleClickedCell(..))
                                    && view.document_selected_editable(cx)
                                {
                                    view.documents
                                        .value
                                        .update(cx, |input, cx| input.focus(window, cx));
                                }
                            }
                            _ => {}
                        },
                    ));
                    self.documents.grid = Some(grid);
                }
                let key = PageKey {
                    sheet: selected_sheet.clone(),
                    row: *row_start,
                    column: *column_start,
                };
                let edits = self.documents.edits.clone();
                if let Some(grid) = &self.documents.grid {
                    grid.update(cx, |grid, cx| {
                        grid.delegate_mut().insert(key, cells.clone());
                        grid.delegate_mut().set_edits(&edits);
                        cx.notify();
                    });
                }
                let (row, column) = self.documents.selected.unwrap_or((0, 0));
                self.document_select_cell(row, column, window, cx);
            }
            DocumentContent::Text { text } => {
                let text = self
                    .documents
                    .edits
                    .iter()
                    .find_map(|edit| {
                        if let DocumentEdit::Text { text } = edit {
                            Some(text.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| text.clone());
                self.documents.restoring = true;
                self.documents
                    .text
                    .update(cx, |input, cx| input.set_value(text, window, cx));
                self.documents.restoring = false;
            }
            DocumentContent::Docx { paragraphs, .. } => {
                self.documents.snapshot = Some(snapshot.clone());
                let selected = self
                    .documents
                    .paragraph
                    .filter(|index| paragraphs.iter().any(|paragraph| paragraph.index == *index))
                    .or_else(|| paragraphs.first().map(|paragraph| paragraph.index));
                if let Some(index) = selected {
                    self.document_select_paragraph(index, window, cx);
                }
            }
        }
        if self.documents.notice == "Opening document…" {
            self.documents.notice =
                "Your source stays unchanged. Save a copy when you are ready.".into();
        }
        self.document_draft_changed(cx);
    }
    fn document_selected_cell(&self, cx: &App) -> Option<DocumentCell> {
        let (row, column) = self.documents.selected?;
        self.documents
            .grid
            .as_ref()?
            .read(cx)
            .delegate()
            .cell(row, column)
            .cloned()
    }
    fn document_selected_editable(&self, cx: &App) -> bool {
        self.documents.ready()
            && self
                .document_selected_cell(cx)
                .is_some_and(|cell| cell.editable)
    }
    fn document_select_cell(
        &mut self,
        row: u32,
        column: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(grid) = self.documents.grid.clone() else {
            return;
        };
        if row >= grid.read(cx).delegate().rows || column >= grid.read(cx).delegate().columns {
            self.documents.notice = "That cell is outside this worksheet.".into();
            cx.notify();
            return;
        }
        self.documents.selected = Some((row, column));
        self.document_show_cell(row, column, window, cx);
        if grid.read(cx).selected_cell() != Some((row as usize, column as usize + 1)) {
            grid.update(cx, |grid, cx| {
                grid.set_selected_cell(row as usize, column as usize + 1, cx)
            });
        }
        self.document_draft_changed(cx);
    }
    fn document_show_cell(
        &mut self,
        row: u32,
        column: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.documents.selected = Some((row, column));
        self.documents.restoring = true;
        Self::set_input(&self.documents.coordinate, address(row, column), window, cx);
        let cell = self.document_selected_cell(cx);
        let value = cell.as_ref().map(|cell| {
            self.documents
                .edits
                .iter()
                .find_map(|edit| match edit {
                    DocumentEdit::Cell {
                        sheet,
                        address,
                        value,
                    } if Some(sheet) == self.documents.selected_sheet.as_ref()
                        && address == &cell.address =>
                    {
                        Some(value)
                    }
                    _ => None,
                })
                .unwrap_or(&cell.value)
        });
        self.documents.value_kind = value.map_or(ValueKind::Auto, kind_of);
        Self::set_input(
            &self.documents.value,
            value.map(display).unwrap_or_default(),
            window,
            cx,
        );
        self.documents.restoring = false;
        if cell.is_none()
            && let Some(sheet) = self.documents.selected_sheet.clone()
        {
            self.document_request_grid_page(
                self.documents.epoch,
                PageKey { sheet, row, column },
                cx,
            );
        }
    }
    fn document_select_sheet(
        &mut self,
        sheet: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.documents.selected_sheet.as_ref() == Some(&sheet) {
            return;
        }
        let Some(snapshot) = &self.documents.snapshot else {
            return;
        };
        let DocumentContent::Spreadsheet { sheets, .. } = &snapshot.content else {
            return;
        };
        let Some(metadata) = sheets
            .iter()
            .find(|metadata| metadata.name == sheet)
            .cloned()
        else {
            return;
        };
        self.document_cancel_reads(cx);
        self.documents.epoch = self.documents.epoch.saturating_add(1);
        self.documents.loading_paused = false;
        self.documents.selected_sheet = Some(sheet.clone());
        self.documents.selected = Some((0, 0));
        let delegate = DocumentGrid::new(
            cx.entity().downgrade(),
            self.documents.epoch,
            sheet.clone(),
            metadata.rows.saturating_add(64).min(1_048_576),
            metadata.columns.saturating_add(16).min(16_384),
        );
        if let Some(grid) = &self.documents.grid {
            grid.update(cx, |grid, cx| {
                *grid.delegate_mut() = delegate;
                grid.refresh(cx);
                grid.set_selected_cell(0, 1, cx);
            });
        }
        self.document_show_cell(0, 0, window, cx);
        self.document_draft_changed(cx);
    }
    fn document_select_paragraph(
        &mut self,
        index: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(DocumentSnapshot {
            content: DocumentContent::Docx { paragraphs, .. },
            ..
        }) = &self.documents.snapshot
        else {
            return;
        };
        let Some(paragraph) = paragraphs.iter().find(|paragraph| paragraph.index == index) else {
            return;
        };
        let text = self
            .documents
            .edits
            .iter()
            .find_map(|edit| match edit {
                DocumentEdit::Paragraph {
                    index: edited,
                    text,
                } if *edited == index => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_else(|| paragraph.text.clone());
        self.documents.paragraph = Some(index);
        self.documents.restoring = true;
        self.documents
            .text
            .update(cx, |input, cx| input.set_value(text, window, cx));
        self.documents.restoring = false;
        cx.notify();
    }
    fn document_text_changed(&mut self, cx: &mut Context<Self>) {
        if self.documents.restoring || !self.documents.ready() {
            return;
        }
        let text = self.documents.text.read(cx).value().to_string();
        let Some(snapshot) = &self.documents.snapshot else {
            return;
        };
        let edit = match &snapshot.content {
            DocumentContent::Text { text: original } => {
                self.documents
                    .edits
                    .retain(|edit| !matches!(edit, DocumentEdit::Text { .. }));
                (original != &text).then_some(DocumentEdit::Text { text })
            }
            DocumentContent::Docx { paragraphs, .. } => {
                let Some(index) = self.documents.paragraph else {
                    return;
                };
                let Some(original) = paragraphs
                    .iter()
                    .find(|paragraph| paragraph.index == index && paragraph.editable)
                else {
                    return;
                };
                self.documents.edits.retain(|edit|!matches!(edit,DocumentEdit::Paragraph{index:edited,..} if *edited==index));
                (original.text != text).then_some(DocumentEdit::Paragraph { index, text })
            }
            _ => return,
        };
        if let Some(edit) = edit {
            self.documents.edits.push(edit);
        }
        self.document_draft_changed(cx);
    }
    fn document_set_kind(&mut self, kind: ValueKind, window: &mut Window, cx: &mut Context<Self>) {
        if !self.document_selected_editable(cx) {
            return;
        }
        self.documents.value_kind = kind;
        if kind == ValueKind::Blank {
            Self::set_input(&self.documents.value, String::new(), window, cx);
        }
        if kind == ValueKind::Boolean {
            cx.notify();
            return;
        }
        self.document_value_changed(cx);
    }
    fn document_boolean(&mut self, value: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.documents.value_kind = ValueKind::Boolean;
        Self::set_input(
            &self.documents.value,
            if value { "true" } else { "false" },
            window,
            cx,
        );
        self.document_value_changed(cx);
    }
    fn document_value_changed(&mut self, cx: &mut Context<Self>) {
        if self.documents.restoring || !self.document_selected_editable(cx) {
            return;
        }
        let Some(cell) = self.document_selected_cell(cx) else {
            return;
        };
        let Some(sheet) = self.documents.selected_sheet.clone() else {
            return;
        };
        let text = self.documents.value.read(cx).value().to_string();
        let value = parse_value(self.documents.value_kind, &text);
        self.documents.edits.retain(|edit|!matches!(edit,DocumentEdit::Cell{sheet:edited,address,..} if edited==&sheet && address==&cell.address));
        if cell.value != value {
            self.documents.edits.push(DocumentEdit::Cell {
                sheet,
                address: cell.address,
                value,
            });
        }
        let edits = self.documents.edits.clone();
        if let Some(grid) = &self.documents.grid {
            grid.update(cx, |grid, cx| {
                grid.delegate_mut().set_edits(&edits);
                cx.notify();
            });
        }
        self.document_draft_changed(cx);
    }
}
fn default_name(path: &Path) -> String {
    format!(
        "{} copy{}",
        path.file_stem().unwrap_or_default().to_string_lossy(),
        path.extension()
            .map(|extension| format!(".{}", extension.to_string_lossy()))
            .unwrap_or_default()
    )
}
fn kind_of(value: &DocumentCellValue) -> ValueKind {
    match value {
        DocumentCellValue::Blank => ValueKind::Auto,
        DocumentCellValue::Text { .. } | DocumentCellValue::Error { .. } => ValueKind::Text,
        DocumentCellValue::Number { .. } => ValueKind::Number,
        DocumentCellValue::Boolean { .. } => ValueKind::Boolean,
        DocumentCellValue::Formula { .. } => ValueKind::Formula,
        DocumentCellValue::DateTime { .. } => ValueKind::DateTime,
    }
}
fn parse_value(kind: ValueKind, text: &str) -> DocumentCellValue {
    match kind {
        ValueKind::Blank => DocumentCellValue::Blank,
        ValueKind::Formula => DocumentCellValue::Formula {
            expression: text.into(),
            cached: None,
        },
        ValueKind::Number => DocumentCellValue::Number { value: text.into() },
        ValueKind::DateTime => DocumentCellValue::DateTime { value: text.into() },
        ValueKind::Boolean
            if text.eq_ignore_ascii_case("true") || text.eq_ignore_ascii_case("false") =>
        {
            DocumentCellValue::Boolean {
                value: text.eq_ignore_ascii_case("true"),
            }
        }
        ValueKind::Auto if text.is_empty() => DocumentCellValue::Blank,
        ValueKind::Auto if text.starts_with('=') => DocumentCellValue::Formula {
            expression: text.into(),
            cached: None,
        },
        ValueKind::Auto if text.parse::<f64>().is_ok() => {
            DocumentCellValue::Number { value: text.into() }
        }
        _ => DocumentCellValue::Text { text: text.into() },
    }
}
fn validate_edits(edits: &[DocumentEdit]) -> Option<String> {
    for edit in edits {
        if let DocumentEdit::Cell { address, value, .. } = edit {
            match value {
                DocumentCellValue::Number { value }
                    if value
                        .parse::<f64>()
                        .ok()
                        .is_none_or(|number| !number.is_finite()) =>
                {
                    return Some(format!(
                        "Enter a valid number in {address}, or choose Text."
                    ));
                }
                DocumentCellValue::Formula { expression, .. }
                    if !expression.starts_with('=') || expression.len() < 2 =>
                {
                    return Some(format!(
                        "Start the formula in {address} with =, or choose Text."
                    ));
                }
                _ => {}
            }
        }
    }
    None
}

#[cfg(test)]
#[allow(dead_code)] // Exercised by the separate native document validation target.
impl Xylo {
    pub(super) fn documents_validation_focus_grid(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(grid) = &self.documents.grid {
            grid.read(cx).focus_handle(cx).focus(window, cx);
        }
    }
    pub(super) fn documents_validation_selected(&self) -> Option<(u32, u32)> {
        self.documents.selected
    }
    pub(super) fn documents_validation_notice(&self) -> &str {
        &self.documents.notice
    }
    pub(super) fn documents_validation_conflicted(&self) -> bool {
        self.documents.source_conflict
    }
    pub(super) fn documents_validation_paragraph(
        &mut self,
        index: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.document_select_paragraph(index, window, cx);
    }
    pub(super) fn documents_validation_ready(&self) -> bool {
        self.documents.ready()
    }
    pub(super) fn documents_validation_saved(&self) -> bool {
        self.documents.drafts.ready()
    }
    pub(super) fn documents_validation_output(&self) -> Option<PathBuf> {
        self.documents.output.clone()
    }
    pub(super) fn documents_validation_save(&mut self, cx: &mut Context<Self>) {
        self.document_save_copy(cx);
    }
    pub(super) fn documents_validation_create(
        &mut self,
        spreadsheet: bool,
        cx: &mut Context<Self>,
    ) {
        self.document_create(spreadsheet, cx);
    }
    pub(super) fn documents_validation_cell(
        &mut self,
        row: u32,
        column: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.document_select_cell(row, column, window, cx);
    }
    pub(super) fn documents_validation_sheet(
        &mut self,
        sheet: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.document_select_sheet(sheet, window, cx);
    }
    pub(super) fn documents_validation_current_cell(&self, cx: &App) -> Option<DocumentCell> {
        self.document_selected_cell(cx)
    }
    pub(super) fn documents_validation_pending(&self) -> Option<Uuid> {
        self.documents
            .pending_copy
            .as_ref()
            .map(|copy| copy.operation_id)
    }
    pub(super) fn documents_validation_check(&mut self, cx: &mut Context<Self>) {
        self.document_check_copy(cx);
    }
}

#[cfg(test)]
#[allow(dead_code)]
impl Xylo {
    pub(super) fn documents_validation_conflict(&self) -> bool {
        self.documents.drafts.conflicted()
    }
    pub(super) fn documents_validation_missing(&self) -> bool {
        self.documents.source_unavailable
            && self.documents.source.is_some()
            && self.documents.drafts.loaded()
    }
    pub(super) fn documents_validation_recent(&self) -> usize {
        self.documents.drafts.recent_len()
    }
    pub(super) fn documents_validation_no_reads(&self) -> bool {
        self.documents.reads.is_empty()
    }
    pub(super) fn documents_validation_relink(
        &self,
    ) -> Option<xylo_runtime::DocumentDraftRelinkRequest> {
        self.documents.pending_relink.as_deref().cloned()
    }
}
