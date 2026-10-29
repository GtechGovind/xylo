//! Product controls for imported documents; structured data stays in the editor controller.
use super::*;
impl Xylo {
    pub(in crate::xylo) fn document_discard_recovery(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.documents.source_conflict
            || self.documents.pending_copy.is_some()
            || self.documents.copy_request.is_some()
        {
            return;
        }
        let epoch = self.documents.epoch;
        let generation = self.documents.generation;
        let answer=window.prompt(PromptLevel::Warning,"Discard recovered document edits?",Some("Only the recovered edits for this document will be removed. The source file and saved copies will stay unchanged."),&[PromptButton::cancel("Cancel"),PromptButton::new("Discard edits and reopen")],cx);
        cx.spawn_in(window, async move |view, cx| {
            if matches!(answer.await, Ok(1)) {
                let _ = view.update_in(cx, |view, window, cx| {
                    if epoch != view.documents.epoch
                        || generation != view.documents.generation
                        || view.documents.pending_copy.is_some()
                        || view.documents.copy_request.is_some()
                    {
                        return;
                    }
                    let Some(snapshot) = view.documents.snapshot.clone() else {
                        return;
                    };
                    view.documents.source_conflict = false;
                    view.documents.edits.clear();
                    view.documents.pending_copy = None;
                    view.documents.recovery_applied = true;
                    view.document_snapshot(snapshot, ReadKind::Initial, window, cx);
                    view.document_draft_changed(cx);
                    view.document_persist_active(cx);
                });
            }
        })
        .detach();
    }
    pub(in super::super) fn documents_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut panel = card()
            .gap_3()
            .w_full()
            .min_w_0()
            .child(div().text_lg().child("Documents"))
            .child(
                div().flex().gap_2().child(
                    button("document-open", "Open document")
                        .disabled(!self.workspace_ready)
                        .on_click(cx.listener(|view, _, _, cx| view.document_pick(cx))),
                ),
            );
        panel = panel.child(
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .child(
                    button("document-new-word", "New Word document")
                        .disabled(!self.workspace_ready || !self.documents.reads.is_empty())
                        .on_click(cx.listener(|view, _, _, cx| view.document_create(false, cx))),
                )
                .child(
                    button("document-new-sheet", "New spreadsheet")
                        .disabled(!self.workspace_ready || !self.documents.reads.is_empty())
                        .on_click(cx.listener(|view, _, _, cx| view.document_create(true, cx))),
                )
                .child(
                    button("document-open-pdf", "Open PDF")
                        .disabled(!self.workspace_ready)
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.pick_asset(super::native_tools::Asset::Document, window, cx)
                        })),
                )
                .child(
                    button("document-preview-install", "Add PDF preview")
                        .on_click(cx.listener(|view, _, _, cx| view.open_assets(2, cx))),
                ),
        );
        if !self.documents.notice.is_empty() {
            panel = panel.child(subtitle(self.documents.notice.clone()));
        }
        panel = panel.child(self.document_recovery_status(cx));
        let Some(source) = &self.documents.source else {
            return panel
                .child(self.document_saved_drafts(cx))
                .into_any_element();
        };
        panel = panel.child(
            div().text_sm().child(
                source
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ),
        );
        if self.documents.source_conflict
            || self.documents.source_unavailable
            || self.documents.snapshot.is_none()
            || self.documents.pending_relink.is_some()
        {
            return panel
                .child(self.document_missing_panel(cx))
                .into_any_element();
        }
        let editable = self.documents.ready();
        if let Some(snapshot) = &self.documents.snapshot {
            match &snapshot.content {
                DocumentContent::Spreadsheet { sheets, .. } => {
                    panel = panel.child(div().flex().flex_wrap().gap_1().children(
                        sheets.iter().enumerate().map(|(index, sheet)| {
                            let name = sheet.name.clone();
                            button(("document-sheet", index), name.clone())
                                .when(
                                    self.documents.selected_sheet.as_ref() == Some(&name),
                                    |button| button.bg(rgb(lavender())),
                                )
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.document_select_sheet(name.clone(), window, cx)
                                }))
                        }),
                    ));
                    panel = panel.child(
                        div()
                            .flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .w(px(90.))
                                    .child(Input::new(&self.documents.coordinate)),
                            )
                            .child(
                                div().flex_1().min_w_0().child(
                                    Input::new(&self.documents.value)
                                        .readonly(!self.document_selected_editable(cx)),
                                ),
                            ),
                    );
                    panel = panel.child(div().flex().flex_wrap().gap_1().children(
                        ValueKind::ALL.iter().enumerate().map(|(index, kind)| {
                            let kind = *kind;
                            button(("document-value-kind", index), kind.label())
                                .disabled(!self.document_selected_editable(cx))
                                .when(self.documents.value_kind == kind, |button| {
                                    button.bg(rgb(lavender()))
                                })
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.document_set_kind(kind, window, cx);
                                }))
                        }),
                    ));
                    if self.documents.value_kind == ValueKind::Boolean {
                        panel = panel.child(
                            div()
                                .flex()
                                .gap_2()
                                .child(
                                    button("document-boolean-true", "True")
                                        .disabled(!self.document_selected_editable(cx))
                                        .on_click(cx.listener(|view, _, window, cx| {
                                            view.document_boolean(true, window, cx)
                                        })),
                                )
                                .child(
                                    button("document-boolean-false", "False")
                                        .disabled(!self.document_selected_editable(cx))
                                        .on_click(cx.listener(|view, _, window, cx| {
                                            view.document_boolean(false, window, cx)
                                        })),
                                ),
                        );
                    }
                    if let Some(cell) = self.document_selected_cell(cx)
                        && !cell.editable
                    {
                        panel = panel.child(subtitle(
                            cell.reason
                                .unwrap_or_else(|| "This cell is protected.".into()),
                        ));
                    }
                    if let Some(grid) = &self.documents.grid {
                        panel = panel.child(
                            div()
                                .h(px(440.))
                                .w_full()
                                .overflow_hidden()
                                .child(DataTable::new(grid)),
                        );
                    }
                    panel=panel.child(subtitle("Use arrow keys to move between cells. Double-click a cell to edit its value. Formulas are saved for your spreadsheet app to calculate."));
                }
                DocumentContent::Docx {
                    paragraphs,
                    total_paragraphs,
                    offset,
                    has_more,
                } => {
                    panel = panel.child(
                        div()
                            .flex()
                            .gap_2()
                            .items_center()
                            .child(
                                button("document-paragraphs-previous", "Previous paragraphs")
                                    .disabled(*offset == 0 || !self.documents.reads.is_empty())
                                    .on_click(cx.listener({
                                        let offset = *offset;
                                        move |view, _, _, cx| {
                                            view.document_read_paragraphs(
                                                offset
                                                    .saturating_sub(view.documents.paragraph_limit),
                                                cx,
                                            )
                                        }
                                    })),
                            )
                            .child(subtitle(format!(
                                "{}–{} of {} paragraphs",
                                offset + 1,
                                offset + paragraphs.len() as u32,
                                total_paragraphs
                            )))
                            .child(
                                button("document-paragraphs-next", "Next paragraphs")
                                    .disabled(!*has_more || !self.documents.reads.is_empty())
                                    .on_click(cx.listener({
                                        let offset = *offset;
                                        move |view, _, _, cx| {
                                            view.document_read_paragraphs(
                                                offset + view.documents.paragraph_limit,
                                                cx,
                                            )
                                        }
                                    })),
                            ),
                    );
                    let rows = paragraphs.clone();
                    panel = panel.child(
                        uniform_list(
                            "document-paragraphs",
                            rows.len(),
                            cx.processor(
                                move |view, range: std::ops::Range<usize>, _window, cx| {
                                    range
                                        .map(|row| {
                                            let paragraph = rows[row].clone();
                                            let index = paragraph.index;
                                            button(
                                                ("document-paragraph", index as usize),
                                                format!(
                                                    "{}  {}",
                                                    index + 1,
                                                    paragraph
                                                        .text
                                                        .chars()
                                                        .take(100)
                                                        .collect::<String>()
                                                ),
                                            )
                                            .w_full()
                                            .justify_start()
                                            .when(
                                                view.documents.paragraph == Some(index),
                                                |button| button.bg(rgb(lavender())),
                                            )
                                            .on_click(cx.listener(move |view, _, window, cx| {
                                                view.document_select_paragraph(index, window, cx)
                                            }))
                                            .into_any_element()
                                        })
                                        .collect()
                                },
                            ),
                        )
                        .h(px(160.)),
                    );
                    let paragraph = paragraphs
                        .iter()
                        .find(|paragraph| Some(paragraph.index) == self.documents.paragraph);
                    let protected = paragraph.is_none_or(|paragraph| !paragraph.editable);
                    if let Some(reason) = paragraph.and_then(|paragraph| paragraph.reason.clone()) {
                        panel = panel.child(subtitle(reason));
                    }
                    panel = panel.child(
                        Textarea::new(&self.documents.text).readonly(!editable || protected),
                    );
                }
                DocumentContent::Text { .. } => {
                    panel = panel.child(Textarea::new(&self.documents.text).readonly(!editable))
                }
            }
        }
        panel = panel.child(
            div()
                .flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&self.documents.name)),
                )
                .child(
                    primary("document-save-copy", "Save a copy")
                        .disabled(!editable || self.documents.pending_copy.is_some())
                        .on_click(cx.listener(|view, _, _, cx| view.document_save_copy(cx))),
                ),
        );
        if !self.documents.reads.is_empty() {
            panel = panel.child(
                button("document-cancel-load", "Cancel loading")
                    .on_click(cx.listener(|view, _, _, cx| view.document_cancel_reads(cx))),
            );
        }
        if self.documents.loading_paused
            || self
                .documents
                .grid
                .as_ref()
                .is_some_and(|grid| !grid.read(cx).delegate().failed.is_empty())
        {
            panel = panel.child(
                button("document-resume-loading", "Resume loading").on_click(cx.listener(
                    |view, _, window, cx| {
                        view.documents.loading_paused = false;
                        if let Some(grid) = &view.documents.grid {
                            grid.update(cx, |grid, cx| {
                                grid.delegate_mut().failed.clear();
                                cx.notify();
                            });
                        }
                        if let Some((row, column)) = view.documents.selected {
                            view.document_show_cell(row, column, window, cx);
                        }
                    },
                )),
            );
        }
        if self.documents.copy_request.is_some() {
            panel = panel.child(
                button("document-cancel-copy", "Cancel save request")
                    .on_click(cx.listener(|view, _, _, cx| view.document_cancel_copy(cx))),
            );
        }
        if self.documents.pending_copy.is_some() {
            panel = panel.child(
                button("document-check-copy", "Check saved copy")
                    .disabled(self.documents.copy_request.is_some())
                    .on_click(cx.listener(|view, _, _, cx| view.document_check_copy(cx))),
            );
        }
        if let Some(path) = &self.documents.output {
            let path = path.clone();
            panel = panel.child(
                button("document-open-copy", "Open saved copy")
                    .on_click(move |_, _, cx| cx.open_with_system(&path)),
            );
        }
        panel.into_any_element()
    }
}
