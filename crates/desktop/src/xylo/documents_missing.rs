//! Source-independent edit inspection and exact-revision source relocation.
use super::*;
use xylo_runtime::DocumentDraftRelinkReceipt;
impl Xylo {
    pub(in crate::xylo) fn document_resume_saved(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let epoch = self.documents.epoch;
        self.document_open(path.clone(), window, cx);
        if self.documents.epoch == epoch {
            return;
        }
        // This path comes from an acknowledged draft key, not an arbitrary alias
        // chosen by a picker. Load it even when the source cannot be parsed.
        self.document_draft_activate(path, cx);
        self.document_apply_recovery(window, cx);
    }
    pub(in crate::xylo) fn document_recovered_select(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.documents.recovered_selection = index;
        let text = self
            .documents
            .edits
            .get(index)
            .map(recovered_content)
            .unwrap_or_default();
        self.documents.restoring = true;
        self.documents
            .text
            .update(cx, |input, cx| input.set_value(text, window, cx));
        self.documents.restoring = false;
        cx.notify();
    }
    fn document_find_original(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        let epoch = self.documents.epoch;
        cx.spawn(async move |view, cx| {
            if let Some(file) = rfd::AsyncFileDialog::new()
                .set_title("Find the moved original in this project")
                .set_directory(workspace)
                .add_filter(
                    "Original documents",
                    &["txt", "md", "csv", "json", "docx", "xlsx"],
                )
                .pick_file()
                .await
            {
                let _ = view.update(cx, |view, cx| {
                    if epoch == view.documents.epoch {
                        view.document_locate_original(file.path().to_path_buf(), cx);
                    }
                });
            }
        })
        .detach();
    }
    pub(in crate::xylo) fn document_locate_original(
        &mut self,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if self.documents.pending_copy.is_some()
            || self.documents.pending_relink.is_some()
            || !self.documents.reads.is_empty()
            || !self.documents.drafts.ready()
        {
            return;
        }
        let Some(source) = self.documents.source.as_ref() else {
            return;
        };
        self.document_read(ReadKind::Locate,json!({"path":path,"expected_revision":source.revision,"row_count":1,"column_count":1,"limit":1}),cx);
        self.documents.notice = "Checking that this is the unchanged original…".into();
        cx.notify();
    }
    pub(in crate::xylo) fn document_found_original(
        &mut self,
        snapshot: DocumentSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = self.documents.source.as_ref() else {
            return;
        };
        if snapshot.source.path == source.path
            || snapshot.source.revision != source.revision
            || snapshot.source.kind != source.kind
            || snapshot.source.bytes != source.bytes
        {
            self.documents.notice =
                "Choose the unchanged original in its new location. Your saved edits stay here."
                    .into();
            cx.notify();
            return;
        }
        let Some(request) = self.document_make_relink(snapshot.source) else {
            return;
        };
        let epoch = self.documents.epoch;
        let generation = self.documents.generation;
        let answer=window.prompt(PromptLevel::Warning,"Use this original's new location?",Some("The file matches the saved original exactly. Your saved edits will move to this location in Xylo. No original file or saved copy will be changed."),&[PromptButton::cancel("Keep reviewing"),PromptButton::new("Use this original")],cx);
        cx.spawn_in(window, async move |view, cx| {
            if matches!(answer.await, Ok(1)) {
                let _ = view.update(cx, |view, cx| {
                    if epoch != view.documents.epoch
                        || generation != view.documents.generation
                        || view.documents.pending_copy.is_some()
                        || view.documents.pending_relink.is_some()
                    {
                        return;
                    }
                    view.documents.pending_relink = Some(Box::new(request));
                    view.documents.relink_needs_dispatch = true;
                    view.document_draft_changed(cx);
                    view.document_persist_active(cx);
                });
            }
        })
        .detach();
    }
    pub(in crate::xylo) fn document_dispatch_relink(&mut self, cx: &mut Context<Self>) {
        if !self.documents.relink_needs_dispatch
            || self.documents.relink_request.is_some()
            || !self.document_relink_acknowledged()
        {
            return;
        }
        self.document_send_relink(false, cx);
    }
    fn document_send_relink(&mut self, status: bool, cx: &mut Context<Self>) {
        if self.documents.relink_request.is_some() {
            return;
        }
        let Some(request) = self.documents.pending_relink.as_deref().cloned() else {
            return;
        };
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| {
                if status {
                    host.document_draft_relink_status(id, request.key.clone(), request.operation_id)
                } else {
                    host.document_draft_relink(id, request.clone())
                }
            });
        if result.is_ok() {
            self.documents.relink_needs_dispatch = false;
            self.documents.relink_request = Some((id, request));
            self.documents.notice = if status {
                "Checking the original's saved location…"
            } else {
                "Saving the original's new location…"
            }
            .into();
            self.documents.relink_watchdog=Some(cx.spawn(async move|view,cx|{
                cx.background_executor().timer(std::time::Duration::from_secs(30)).await;
                let _=view.update(cx,|view,cx|{if view.documents.relink_request.as_ref().is_some_and(|request|request.0==id) {
                    view.documents.relink_request=None;view.documents.relink_watchdog=None;
                    view.documents.notice="The location acknowledgment has not arrived. Your exact request and edits are saved; check the saved location.".into();view.cancel_document_drafts_flush(cx);
                }});
            }));
        } else {
            self.documents.notice =
                "The location was not confirmed. Your exact request and edits remain saved.".into();
        }
        cx.notify();
    }
    pub(in crate::xylo) fn document_relinked(
        &mut self,
        id: Uuid,
        result: Result<DocumentDraftRelinkReceipt, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.document_relink_status(id, result.map(Some), window, cx);
    }
    pub(in crate::xylo) fn document_relink_status(
        &mut self,
        id: Uuid,
        result: Result<Option<DocumentDraftRelinkReceipt>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((pending, request)) = self.documents.relink_request.as_ref() else {
            return;
        };
        if *pending != id {
            return;
        }
        let request = request.clone();
        self.documents.relink_request = None;
        self.documents.relink_watchdog = None;
        match result {
            Ok(Some(receipt)) if receipt.request==request && self.documents.pending_relink.as_deref()==Some(&request) && receipt.key.workspace==request.key.workspace && receipt.key.source==request.source.path && request.expected_revision.checked_add(1)==Some(receipt.previous_revision) => self.document_adopt_relink(receipt,window,cx),
            Ok(Some(_))=>self.documents.notice="The saved location did not match this document. Your exact request and edits stay here.".into(),
            Ok(None)=>self.documents.notice="No saved location change is confirmed. Your original edits and exact request stay here.".into(),
            Err(_)=>self.documents.notice="The new location was not confirmed. Saved edits may have changed or this original already has edits. Your original proposal stays saved; check its location or keep the previous location.".into(),
        }
        cx.notify();
    }
    fn document_keep_location(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.documents.relink_request.is_some() {
            return;
        }
        let Some(request) = self.documents.pending_relink.clone() else {
            return;
        };
        let epoch = self.documents.epoch;
        let answer=window.prompt(PromptLevel::Warning,"Keep the previous original location?",Some("Your recovered edits stay saved. Cancelling this location proposal does not move or change any file. If its saved result changed elsewhere, Xylo will keep both records and ask you to review."),&[PromptButton::cancel("Keep reviewing"),PromptButton::new("Keep previous location")],cx);
        cx.spawn_in(window, async move |view, cx| {
            if matches!(answer.await, Ok(1)) {
                let _ = view.update(cx, |view, cx| {
                    if view.documents.epoch == epoch
                        && view.documents.pending_relink == Some(request)
                        && view.documents.relink_request.is_none()
                    {
                        view.documents.pending_relink = None;
                        view.documents.relink_needs_dispatch = false;
                        view.document_draft_changed(cx);
                        view.document_persist_active(cx);
                    }
                });
            }
        })
        .detach();
    }
    pub(in crate::xylo) fn document_missing_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut panel=div().w_full().flex().flex_col().items_start().gap_3()
            .child(subtitle(if self.documents.source_conflict {"The original differs from the version these edits belong to. Your recovered edits stay saved."} else if self.documents.source_unavailable {"The original is not available at its previous location. Your recovered edits and saved-copy records stay saved."} else {"Opening the original. Your recovered edits are available below."}));
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
        if self.documents.pending_relink.is_some() {
            if self.documents.relink_request.is_some() {
                panel=panel.child(button("document-stop-location", "Stop waiting for this request").on_click(cx.listener(|view,_,_,cx| {
                    if let Some((id,_))=view.documents.relink_request.take() {
                        if let Some(host)=&view.host {let _=host.cancel_capability(id);}
                        view.documents.relink_watchdog=None;
                        view.documents.notice="Stopped waiting. The saved location may already have changed; check its result. Your exact request and edits are retained.".into();
                        view.cancel_document_drafts_flush(cx);
                    }
                })));
            }
            panel=panel.child(subtitle("An original-location proposal is saved. Check its result before choosing another file."))
                .child(div().flex().flex_wrap().gap_2()
                    .child(button("document-check-location","Check saved location").disabled(self.documents.relink_request.is_some()).on_click(cx.listener(|view,_,_,cx|view.document_send_relink(true,cx))))
                    .child(button("document-save-location","Use this original").disabled(self.documents.relink_request.is_some() || !self.document_relink_acknowledged()).on_click(cx.listener(|view,_,_,cx|view.document_send_relink(false,cx))))
                    .child(button("document-keep-location","Keep previous location").disabled(self.documents.relink_request.is_some()).on_click(cx.listener(|view,_,window,cx|view.document_keep_location(window,cx)))));
        } else {
            panel = panel.child(
                button("document-find-original", "Find moved original")
                    .disabled(
                        !self.documents.drafts.ready()
                            || self.documents.pending_copy.is_some()
                            || !self.documents.reads.is_empty(),
                    )
                    .on_click(cx.listener(|view, _, _, cx| view.document_find_original(cx))),
            );
        }
        panel=panel.child(subtitle("Only an exact match can reopen these edits. Office formatting, formulas and unchanged content require the original; Xylo will not rebuild an Office file from partial changes."));
        let rows = self
            .documents
            .edits
            .iter()
            .map(recovered_label)
            .collect::<Vec<_>>();
        panel = panel.child(
            div()
                .text_sm()
                .child(format!("{} recovered changes", rows.len())),
        );
        if !rows.is_empty() {
            let list_height = rows.len().min(4) as f32 * 36.;
            panel = panel
                .child(
                    button("document-copy-recovered", "Copy this recovered change").on_click(
                        cx.listener(|view, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                view.documents.text.read(cx).value().to_string(),
                            ))
                        }),
                    ),
                )
                .child(
                    uniform_list(
                        "document-recovered-changes",
                        rows.len(),
                        cx.processor(move |view, range: std::ops::Range<usize>, _, cx| {
                            range
                                .map(|index| {
                                    button(
                                        ("document-recovered-change", index),
                                        rows[index].clone(),
                                    )
                                    .w_full()
                                    .h(px(36.))
                                    .justify_start()
                                    .when(view.documents.recovered_selection == index, |button| {
                                        button.bg(rgb(lavender()))
                                    })
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        view.document_recovered_select(index, window, cx)
                                    }))
                                    .into_any_element()
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .w_full()
                    .h(px(list_height)),
                )
                .child(
                    Textarea::new(&self.documents.text)
                        .h(px(180.))
                        .readonly(true)
                        .w_full()
                        .aria_label("Recovered change content"),
                );
        }
        if self.documents.source_conflict
            && self.documents.snapshot.is_some()
            && self.documents.pending_copy.is_none()
            && self.documents.pending_relink.is_none()
        {
            panel = panel.child(
                button(
                    "document-reload-source",
                    "Discard recovered edits and reopen",
                )
                .on_click(
                    cx.listener(|view, _, window, cx| view.document_discard_recovery(window, cx)),
                ),
            );
        }
        panel.into_any_element()
    }
}
fn recovered_label(edit: &DocumentEdit) -> String {
    match edit {
        DocumentEdit::Text { .. } => "Text document".into(),
        DocumentEdit::Paragraph { index, .. } => format!("Paragraph {}", u64::from(*index) + 1),
        DocumentEdit::Cell {
            sheet,
            address,
            value,
        } => format!(
            "{} · {} · {}",
            truncate(sheet, 100),
            address,
            match value {
                DocumentCellValue::Blank => "Blank",
                DocumentCellValue::Text { .. } => "Text",
                DocumentCellValue::Number { .. } => "Number",
                DocumentCellValue::Boolean { .. } => "True / false",
                DocumentCellValue::Formula { .. } => "Formula",
                DocumentCellValue::Error { .. } => "Error value",
                DocumentCellValue::DateTime { .. } => "Date / time",
            }
        ),
    }
}

fn recovered_content(edit: &DocumentEdit) -> String {
    match edit {
        DocumentEdit::Text { text } => text.clone(),
        DocumentEdit::Paragraph { index, text } => {
            format!("Paragraph {}\n\n{text}", u64::from(*index) + 1)
        }
        DocumentEdit::Cell {
            sheet,
            address,
            value,
        } => {
            let mut text = format!("Worksheet: {sheet}\nCell: {address}\n\n{}", display(value));
            if let DocumentCellValue::Formula {
                cached: Some(cached),
                ..
            } = value
            {
                text.push_str("\n\nLast saved value (not recalculated):\n");
                text.push_str(cached);
            }
            text
        }
    }
}
