//! Document recovery uses independent CAS rows, not chat or source-file writes.
//! Every copy intent is acknowledged durably before the helper may publish it.
use super::*;
use std::time::Duration;
use xylo_runtime::{
    DocumentDraftCursor, DocumentDraftKey, DocumentDraftPage, DocumentDraftSave,
    DocumentDraftSnapshot, DocumentDraftSummary, DocumentDraftWrite,
};
type Key = (PathBuf, PathBuf);
struct Save {
    id: Uuid,
    generation: u64,
    write: DocumentDraftWrite,
}
#[derive(Default)]
struct Entry {
    state: Option<DocumentDraftState>,
    revision: Option<i64>,
    generation: u64,
    dirty: bool,
    load: Option<Uuid>,
    save: Option<Save>,
    retry: Option<Save>,
    conflict: Option<DocumentDraftSnapshot>,
    error: Option<String>,
}
#[derive(Default)]
pub(in super::super) struct DocumentDraftUi {
    active: Option<Key>,
    entries: BTreeMap<Key, Entry>,
    debounce: Option<Task<()>>,
    flush: Option<DraftFlushAction>,
    list: Vec<DocumentDraftSummary>,
    before: Option<DocumentDraftCursor>,
    has_more: bool,
    list_request: Option<(Uuid, PathBuf, bool)>,
}
impl DocumentDraftUi {
    #[cfg(test)]
    pub fn recent_len(&self) -> usize {
        self.list.len()
    }
    #[cfg(test)]
    pub fn conflicted(&self) -> bool {
        self.active
            .as_ref()
            .and_then(|key| self.entries.get(key))
            .is_some_and(|entry| entry.conflict.is_some())
    }
    pub fn clear_active(&mut self) {
        self.active = None;
    }
    pub fn is_active_path(&self, path: &Path) -> bool {
        self.active.as_ref().is_some_and(|key| key.1 == path)
    }
    pub fn loaded(&self) -> bool {
        self.active
            .as_ref()
            .and_then(|key| self.entries.get(key))
            .is_some_and(|entry| entry.revision.is_some() && entry.conflict.is_none())
    }
    pub fn ready(&self) -> bool {
        self.loaded()
            && self
                .active
                .as_ref()
                .and_then(|key| self.entries.get(key))
                .is_some_and(|entry| !entry.dirty && entry.save.is_none() && entry.error.is_none())
    }
}
fn same(a: &Option<DocumentDraftState>, b: &Option<DocumentDraftState>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.source == b.source
                && a.edits == b.edits
                && a.export_name == b.export_name
                && a.sheet == b.sheet
                && a.selected == b.selected
                && a.pending_relink == b.pending_relink
                && match (&a.pending_copy, &b.pending_copy) {
                    (None, None) => true,
                    (Some(a), Some(b)) => {
                        a.operation_id == b.operation_id
                            && a.generation == b.generation
                            && a.name == b.name
                            && a.edits == b.edits
                    }
                    _ => false,
                }
        }
        _ => false,
    }
}

fn draft_key(key: &Key) -> DocumentDraftKey {
    DocumentDraftKey {
        workspace: key.0.clone(),
        source: key.1.clone(),
    }
}
impl Xylo {
    pub(in super::super) fn document_draft_activate(
        &mut self,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        let key = (workspace, path);
        self.documents.drafts.active = Some(key.clone());
        if self.documents.drafts.entries.contains_key(&key) {
            return;
        }
        // Do not evict an unacknowledged write or unresolved conflict.
        if self.documents.drafts.entries.len() >= 16 {
            if let Some(removable) = self
                .documents
                .drafts
                .entries
                .iter()
                .find(|(key, entry)| {
                    Some(*key) != self.documents.drafts.active.as_ref()
                        && !entry.dirty
                        && entry.save.is_none()
                        && entry.load.is_none()
                        && entry.conflict.is_none()
                        && entry.error.is_none()
                })
                .map(|(key, _)| key.clone())
            {
                self.documents.drafts.entries.remove(&removable);
            } else {
                self.documents.notice =
                    "Save pending document edits before opening another document.".into();
                return;
            }
        }
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.document_draft_load(id, draft_key(&key)));
        let mut entry = Entry::default();
        if result.is_ok() {
            entry.load = Some(id);
        } else {
            entry.error = Some(
                "Your saved edits could not be loaded. Retry before editing this document.".into(),
            );
        }
        self.documents.drafts.entries.insert(key, entry);
        self.result(result, cx);
    }
    pub(in super::super) fn document_apply_recovery(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.documents.recovery_applied {
            return;
        }
        let Some(key) = self.documents.drafts.active.clone() else {
            return;
        };
        let Some(entry) = self.documents.drafts.entries.get(&key) else {
            return;
        };
        if entry.revision.is_none() {
            return;
        }
        self.documents.recovery_applied = true;
        let Some(state) = entry.state.clone() else {
            return;
        };
        let source_changed = self
            .documents
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.source != state.source);
        self.documents.restoring = true;
        self.documents.source = Some(state.source);
        self.documents.edits = state.edits;
        self.documents.pending_copy = state.pending_copy;
        self.documents.pending_relink = state.pending_relink;
        self.documents.relink_needs_dispatch = false;
        self.documents.copy_needs_dispatch = false;
        self.documents.selected_sheet = state.sheet;
        self.documents.selected = state.selected.as_deref().and_then(parse_address);
        self.documents.generation = entry.generation;
        Self::set_input(&self.documents.name, state.export_name, window, cx);
        if source_changed {
            self.documents.source_conflict = true;
            self.documents.notice =
                "Your recovered edits refer to an earlier version of this source.".into();
        } else if self.documents.pending_copy.is_some() {
            self.documents.notice =
                "An earlier copy needs its result checked before saving again.".into();
        }
        self.documents.restoring = false;
        if self.documents.snapshot.is_none() || self.documents.source_conflict {
            self.document_recovered_select(0, window, cx);
        }
    }
    pub(in super::super) fn document_draft_changed(&mut self, cx: &mut Context<Self>) {
        if self.documents.restoring {
            return;
        }
        let Some(key) = self.documents.drafts.active.clone() else {
            return;
        };
        let Some(source) = self.documents.source.clone() else {
            return;
        };
        let state = Some(DocumentDraftState {
            source,
            edits: self.documents.edits.clone(),
            export_name: self.documents.name.read(cx).value().to_string(),
            sheet: self.documents.selected_sheet.clone(),
            selected: self
                .documents
                .selected
                .map(|(row, column)| address(row, column)),
            pending_copy: self.documents.pending_copy.clone(),
            pending_relink: self.documents.pending_relink.clone(),
        });
        let Some(entry) = self.documents.drafts.entries.get_mut(&key) else {
            return;
        };
        if entry
            .error
            .as_ref()
            .is_some_and(|error| error.starts_with("These edits are too large"))
            && serde_json::to_vec(&state)
                .is_ok_and(|value| value.len() <= xylo_runtime::MAX_DOCUMENT_DRAFT_BYTES)
        {
            entry.error = None;
            entry.retry = None;
            entry.dirty = true;
        }
        if entry.revision.is_none() || same(&entry.state, &state) {
            return;
        }
        if serde_json::to_vec(&state).map_or(true, |value| {
            value.len() > xylo_runtime::MAX_DOCUMENT_DRAFT_BYTES
        }) {
            entry.error=Some("These edits are too large to recover. Save a smaller copy or undo the last change before closing.".into());
            entry.dirty = true;
            self.documents.drafts.flush = None;
            self.cancel_draft_flush(cx);
            self.cancel_memory_forms_flush(cx);
            self.cancel_recovery_flush(cx);
            return;
        }
        entry.state = state;
        entry.generation = entry.generation.saturating_add(1);
        self.documents.generation = entry.generation;
        entry.dirty = true;
        // Fresh user changes revoke a pending quit/update intent. Programmatic
        // restoration/no-op set_value never reaches this branch.
        self.documents.drafts.flush = None;
        self.cancel_draft_flush(cx);
        self.cancel_memory_forms_flush(cx);
        self.cancel_recovery_flush(cx);
        self.documents.drafts.debounce = Some(cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            let _ = view.update(cx, |view, cx| view.document_persist_active(cx));
        }));
        cx.notify();
    }
    pub(in super::super) fn document_persist_active(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self.documents.drafts.active.clone() {
            self.document_persist(&key, cx);
        }
    }
    fn document_persist(&mut self, key: &Key, cx: &mut Context<Self>) {
        let Some(entry) = self.documents.drafts.entries.get_mut(key) else {
            return;
        };
        if !entry.dirty
            || entry.revision.is_none()
            || entry.load.is_some()
            || entry.save.is_some()
            || entry.conflict.is_some()
            || entry.error.is_some()
        {
            return;
        }
        let mut save = entry.retry.take().unwrap_or_else(|| Save {
            id: Uuid::new_v4(),
            generation: entry.generation,
            write: DocumentDraftWrite {
                key: draft_key(key),
                expected_revision: entry.revision.unwrap_or(0),
                mutation_id: Uuid::new_v4(),
                state: entry.state.clone(),
            },
        });
        save.id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.document_draft_save(save.id, save.write.clone()));
        if result.is_ok() {
            entry.save = Some(save);
        } else {
            entry.retry = Some(save);
            entry.error = Some("Your edits remain here. Retry saving their recovery copy.".into());
            self.documents.drafts.flush = None;
        }
        self.result(result, cx);
    }
    pub(in super::super) fn document_draft_loaded(
        &mut self,
        id: Uuid,
        result: Result<DocumentDraftSnapshot, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self
            .documents
            .drafts
            .entries
            .iter()
            .find(|(_, entry)| entry.load == Some(id))
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        let entry = self.documents.drafts.entries.get_mut(&key).unwrap();
        entry.load = None;
        match result {
            Ok(snapshot) if snapshot.key == draft_key(&key) => {
                entry.revision = Some(snapshot.revision);
                entry.state = snapshot.state;
                entry.error = None;
            }
            _ => {
                entry.error = Some(
                    "Your saved document edits could not be loaded. Retry before editing.".into(),
                );
                self.documents.drafts.flush = None;
            }
        }
        if self.documents.drafts.active.as_ref() == Some(&key) {
            self.document_apply_recovery(window, cx);
            // The source may have arrived first. Reapply the now-loaded draft
            // through the same typed content presenter, without a second read.
            if let Some(snapshot) = self.documents.snapshot.clone() {
                self.document_snapshot(snapshot, ReadKind::Initial, window, cx);
            }
        }
        self.advance_document_drafts_flush(window, cx);
        cx.notify();
    }
    pub(in super::super) fn document_draft_saved(
        &mut self,
        id: Uuid,
        result: Result<DocumentDraftSave, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self
            .documents
            .drafts
            .entries
            .iter()
            .find(|(_, entry)| entry.save.as_ref().is_some_and(|save| save.id == id))
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        let entry = self.documents.drafts.entries.get_mut(&key).unwrap();
        let save = entry.save.take().unwrap();
        match result {
            Ok(DocumentDraftSave::Saved(snapshot))
                if snapshot.key == draft_key(&key) && same(&snapshot.state, &save.write.state) =>
            {
                entry.revision = Some(snapshot.revision);
                entry.dirty = entry.generation != save.generation;
                entry.error = None;
                entry.retry = None;
            }
            Ok(DocumentDraftSave::Conflict(snapshot)) if snapshot.key == draft_key(&key) => {
                entry.revision = Some(snapshot.revision);
                if same(&snapshot.state, &entry.state) {
                    entry.dirty = false;
                    entry.error = None;
                } else {
                    entry.conflict = Some(snapshot);
                    self.documents.drafts.flush = None;
                }
            }
            _ => {
                entry.retry = Some(save);
                entry.error =
                    Some("Your edits remain here. Retry saving their recovery copy.".into());
                self.documents.drafts.flush = None;
            }
        }
        self.document_persist(&key, cx);
        if self.documents.drafts.active.as_ref() == Some(&key) && self.documents.drafts.ready() {
            self.document_dispatch_copy(cx);
            self.document_dispatch_relink(cx);
        }
        self.advance_document_drafts_flush(window, cx);
        cx.notify();
    }
    pub(in super::super) fn document_before_workspace_switch(
        &mut self,
        cx: &mut Context<Self>,
    ) -> bool {
        self.document_draft_changed(cx);
        self.document_persist_active(cx);
        if self.documents.copy_request.is_some() || self.documents.relink_request.is_some() {
            self.documents.notice =
                "Wait for the saved copy's result before switching documents or projects.".into();
            cx.notify();
            return false;
        }
        if self.documents.drafts.active.is_some() && !self.documents.drafts.ready() {
            self.documents.notice="Your document edits are being saved. Resolve any recovery warning before switching.".into();
            cx.notify();
            return false;
        }
        true
    }
    pub(in super::super) fn document_after_workspace_switch(
        &mut self,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.document_cancel_reads(cx);
        self.documents.epoch = self.documents.epoch.saturating_add(1);
        self.documents.drafts.active = None;
        self.documents.source = None;
        self.documents.snapshot = None;
        self.documents.grid = None;
        self.documents.grid_subscription = None;
        self.documents.edits.clear();
        self.documents.pending_copy = None;
        self.documents.copy_request = None;
        self.documents.pending_relink = None;
        self.documents.relink_request = None;
        self.documents.relink_watchdog = None;
        self.documents.source_unavailable = false;
        self.documents.output = None;
        self.documents.notice.clear();
        self.documents.drafts.list.clear();
        self.documents.drafts.before = None;
        self.documents.drafts.has_more = false;
        self.documents.drafts.list_request = None;
        self.document_workspace_ready(cx);
    }
    pub(in super::super) fn document_workspace_ready(&mut self, cx: &mut Context<Self>) {
        self.document_list_drafts(false, cx);
    }
    fn document_list_drafts(&mut self, more: bool, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.clone().filter(|_| self.workspace_ready) else {
            return;
        };
        if self.documents.drafts.list_request.is_some() {
            return;
        }
        let id = Uuid::new_v4();
        let before = more.then(|| self.documents.drafts.before.clone()).flatten();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.document_draft_list(id, workspace.clone(), before, 50));
        if result.is_ok() {
            self.documents.drafts.list_request = Some((id, workspace, more));
        }
        self.result(result, cx);
    }
    pub(in super::super) fn document_drafts_listed(
        &mut self,
        id: Uuid,
        result: Result<DocumentDraftPage, String>,
        cx: &mut Context<Self>,
    ) {
        let Some((pending, workspace, more)) = self.documents.drafts.list_request.clone() else {
            return;
        };
        if pending != id {
            return;
        }
        self.documents.drafts.list_request = None;
        if self.workspace.as_ref() != Some(&workspace) {
            return;
        }
        match result {
            Ok(page) if page.workspace == workspace => {
                if !more {
                    self.documents.drafts.list.clear();
                }
                for draft in page.drafts {
                    if !self
                        .documents
                        .drafts
                        .list
                        .iter()
                        .any(|existing| existing.key == draft.key)
                    {
                        self.documents.drafts.list.push(draft);
                    }
                }
                self.documents.drafts.before = page.before;
                self.documents.drafts.has_more = page.has_more;
            }
            Err(_) => {
                self.documents.notice =
                    "Saved documents could not be listed. Your existing edits remain saved.".into()
            }
            _ => {}
        }
        cx.notify();
    }
    pub(in super::super) fn document_drafts_flush(
        &mut self,
        action: DraftFlushAction,
        cx: &mut Context<Self>,
    ) -> bool {
        self.document_draft_changed(cx);
        self.documents.drafts.debounce = None;
        if self.documents.drafts.entries.values().all(|entry| {
            !entry.dirty
                && entry.save.is_none()
                && entry.load.is_none()
                && entry.conflict.is_none()
                && entry.error.is_none()
        }) {
            return true;
        }
        self.documents.drafts.flush = Some(action);
        let keys = self
            .documents
            .drafts
            .entries
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            self.document_persist(&key, cx);
        }
        cx.notify();
        false
    }
    pub(in super::super) fn document_drafts_flush_pending(&self) -> bool {
        self.documents.drafts.flush.is_some()
    }
    pub(in super::super) fn cancel_document_drafts_flush(&mut self, cx: &mut Context<Self>) {
        self.documents.drafts.flush = None;
        cx.notify();
    }
    pub(in super::super) fn advance_document_drafts_flush(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.documents.drafts.entries.values().any(|entry| {
            entry.dirty
                || entry.save.is_some()
                || entry.load.is_some()
                || entry.conflict.is_some()
                || entry.error.is_some()
        }) {
            return;
        }
        if let Some(action) = self.documents.drafts.flush.take() {
            match action {
                DraftFlushAction::Quit => self.begin_shutdown(cx),
                DraftFlushAction::Update => self.begin_update_install(window, cx),
            }
        }
    }
    fn document_resolve_recovery(
        &mut self,
        use_saved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.documents.drafts.active.clone() else {
            return;
        };
        let Some(entry) = self.documents.drafts.entries.get_mut(&key) else {
            return;
        };
        let Some(conflict) = entry.conflict.take() else {
            return;
        };
        entry.revision = Some(conflict.revision);
        entry.retry = None;
        entry.error = None;
        if use_saved {
            let cleared = conflict.state.is_none();
            entry.state = conflict.state;
            entry.dirty = false;
            if cleared {
                self.documents.pending_relink = None;
                self.documents.relink_request = None;
                self.documents.relink_watchdog = None;
                self.documents.pending_copy = None;
                self.documents.source = None;
                self.documents.snapshot = None;
                self.documents.edits.clear();
                self.documents.grid = None;
                self.documents.source_conflict = false;
                self.documents.source_unavailable = false;
                self.documents.notice = "This saved proposal moved or was cleared elsewhere. Open its current saved record from Recent documents; other saved edits and copies stay preserved.".into();
                self.document_workspace_ready(cx);
                cx.notify();
                return;
            }
            self.documents.copy_needs_dispatch = false;
            self.documents.recovery_applied = false;
            self.documents.edits.clear();
            self.documents.pending_copy = None;
            self.document_apply_recovery(window, cx);
            if let Some(snapshot) = self.documents.snapshot.clone() {
                self.document_snapshot(snapshot, ReadKind::Initial, window, cx);
            }
        } else {
            entry.dirty = true;
            entry.generation = entry.generation.saturating_add(1);
            self.document_persist(&key, cx);
        }
        cx.notify();
    }
    pub(in super::super) fn document_recovery_status(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(key) = self.documents.drafts.active.as_ref() else {
            return div().into_any_element();
        };
        let Some(entry) = self.documents.drafts.entries.get(key) else {
            return subtitle("Loading saved edits…").into_any_element();
        };
        if entry.conflict.is_some() {
            return div().flex().flex_col().gap_2().child(subtitle("Another window saved different document edits. Your changes are still here. Choose which edits to keep.")).child(div().flex().gap_2().child(button("document-recovery-saved","Use saved edits").on_click(cx.listener(|view,_,window,cx|view.document_resolve_recovery(true,window,cx)))).child(button("document-recovery-keep","Keep these edits").on_click(cx.listener(|view,_,window,cx|view.document_resolve_recovery(false,window,cx))))).into_any_element();
        }
        if let Some(error) = &entry.error {
            return div()
                .flex()
                .flex_col()
                .gap_2()
                .child(subtitle(error.clone()))
                .child(
                    button("document-recovery-retry", "Retry saving edits").on_click(cx.listener(
                        |view, _, _, cx| {
                            let Some(key) = view.documents.drafts.active.clone() else {
                                return;
                            };
                            let Some(entry) = view.documents.drafts.entries.get_mut(&key) else {
                                return;
                            };
                            entry.error = None;
                            if entry.revision.is_none() {
                                let id = Uuid::new_v4();
                                let result = view
                                    .host
                                    .as_ref()
                                    .ok_or_else(|| {
                                        anyhow::anyhow!("Application service unavailable")
                                    })
                                    .and_then(|host| host.document_draft_load(id, draft_key(&key)));
                                if result.is_ok() {
                                    entry.load = Some(id);
                                }
                                view.result(result, cx);
                            } else {
                                view.document_persist(&key, cx);
                            }
                        },
                    )),
                )
                .into_any_element();
        }
        subtitle(if entry.revision.is_none() {
            "Loading saved edits…"
        } else if entry.dirty || entry.save.is_some() {
            "Saving edits on this computer…"
        } else {
            "Edits saved on this computer."
        })
        .into_any_element()
    }
    pub(in super::super) fn document_saved_drafts(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.documents.drafts.list.clone();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(subtitle("Recent documents and recovered edits"))
            .child(
                uniform_list(
                    "document-recent",
                    rows.len(),
                    cx.processor(move |_, range: std::ops::Range<usize>, _window, cx| {
                        range
                            .map(|index| {
                                let draft = rows[index].clone();
                                let path = draft.key.source;
                                let label = format!(
                                    "{} · {} edits{}",
                                    path.file_name().unwrap_or_default().to_string_lossy(),
                                    draft.edits,
                                    if draft.has_pending_copy {
                                        " · copy needs checking"
                                    } else {
                                        ""
                                    }
                                );
                                button(("document-resume", index), label)
                                    .w_full()
                                    .justify_start()
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        view.document_resume_saved(path.clone(), window, cx)
                                    }))
                                    .into_any_element()
                            })
                            .collect()
                    }),
                )
                .h(px(180.)),
            )
            .when(self.documents.drafts.has_more, |panel| {
                panel.child(
                    button("document-recent-more", "More documents")
                        .disabled(self.documents.drafts.list_request.is_some())
                        .on_click(
                            cx.listener(|view, _, _, cx| view.document_list_drafts(true, cx)),
                        ),
                )
            })
            .into_any_element()
    }
}

impl Xylo {
    pub(in crate::xylo) fn document_make_relink(
        &self,
        source: DocumentSource,
    ) -> Option<xylo_runtime::DocumentDraftRelinkRequest> {
        let key = self.documents.drafts.active.as_ref()?;
        let entry = self.documents.drafts.entries.get(key)?;
        if !self.documents.drafts.ready()
            || self.documents.pending_copy.is_some()
            || self.documents.pending_relink.is_some()
        {
            return None;
        }
        Some(xylo_runtime::DocumentDraftRelinkRequest {
            operation_id: Uuid::new_v4(),
            key: draft_key(key),
            expected_revision: entry.revision?.checked_add(1)?,
            source,
        })
    }
    pub(in crate::xylo) fn document_relink_acknowledged(&self) -> bool {
        self.documents.drafts.ready()
            && self
                .documents
                .pending_relink
                .as_ref()
                .is_some_and(|request| {
                    self.documents
                        .drafts
                        .active
                        .as_ref()
                        .and_then(|key| self.documents.drafts.entries.get(key))
                        .is_some_and(|entry| entry.revision == Some(request.expected_revision))
                })
    }
    pub(in crate::xylo) fn document_adopt_relink(
        &mut self,
        receipt: xylo_runtime::DocumentDraftRelinkReceipt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = (receipt.request.key.workspace, receipt.request.key.source);
        if self
            .documents
            .drafts
            .entries
            .get(&key)
            .is_some_and(|entry| {
                entry.dirty
                    || entry.save.is_some()
                    || entry.retry.is_some()
                    || entry.conflict.is_some()
            })
        {
            self.documents.notice="The original's location is saved. Newer local changes are still here; review their recovery warning before opening the moved record.".into();
            cx.notify();
            return;
        }
        let destination = (receipt.key.workspace.clone(), receipt.key.source.clone());
        if self
            .documents
            .drafts
            .entries
            .get(&destination)
            .is_some_and(|entry| {
                entry.dirty
                    || entry.save.is_some()
                    || entry.load.is_some()
                    || entry.retry.is_some()
                    || entry.conflict.is_some()
            })
        {
            self.documents.notice = "The original's location is saved. Finish reviewing the other open document's unsaved edits before opening it here.".into();
            cx.notify();
            return;
        }
        // A clean cached destination may describe an absent/tombstoned row from
        // before the atomic move. Force an authoritative reload of the proposal.
        self.documents.drafts.entries.remove(&destination);
        self.documents.drafts.entries.remove(&key);
        self.documents.drafts.active = None;
        self.documents.source = None;
        self.documents.snapshot = None;
        self.documents.pending_relink = None;
        self.documents.edits.clear();
        self.documents.relink_request = None;
        self.documents.relink_watchdog = None;
        self.document_resume_saved(receipt.key.source, window, cx);
        self.document_workspace_ready(cx);
    }
}
