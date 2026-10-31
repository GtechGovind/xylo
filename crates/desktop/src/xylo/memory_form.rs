//! Scoped Memory form autosave. IDs/generations fence late loads and receipts;
//! persistence never occurs on the rendering thread or in the composer tables.
use super::settings_ui;
use super::*;
use xylo_runtime::{MemoryFormSave, MemoryFormSnapshot, MemoryFormState, MemoryFormWrite};

struct Save {
    id: Uuid,
    generation: u64,
    write: MemoryFormWrite,
}
#[derive(Default)]
struct Entry {
    state: MemoryFormState,
    revision: Option<i64>,
    generation: u64,
    dirty: bool,
    load: Option<(Uuid, u64)>,
    save: Option<Save>,
    retry: Option<Save>,
    conflict: Option<MemoryFormSnapshot>,
    error: Option<String>,
}
#[derive(Default)]
pub(super) struct MemoryForms {
    active: Option<String>,
    entries: BTreeMap<String, Entry>,
    debounce: Option<Task<()>>,
    flush: Option<DraftFlushAction>,
    oversize: bool,
}
fn same(a: &MemoryFormState, b: &MemoryFormState) -> bool {
    a.text == b.text
        && a.editing == b.editing
        && a.retry_input == b.retry_input
        && serde_json::to_string(&a.retry).ok() == serde_json::to_string(&b.retry).ok()
}
impl Xylo {
    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn validation_memory_form_clean(&self, scope: &str) -> bool {
        self.facts.forms.entries.get(scope).is_some_and(|entry| {
            entry.revision.is_some()
                && !entry.dirty
                && entry.save.is_none()
                && entry.conflict.is_none()
        })
    }
    fn ensure_memory_form(&mut self, scope: &str, cx: &mut Context<Self>) -> bool {
        if self.facts.forms.entries.contains_key(scope) {
            return true;
        }
        if self.facts.forms.entries.len() >= 64 {
            let removable = self
                .facts
                .forms
                .entries
                .iter()
                .find(|(key, entry)| {
                    Some(*key) != self.facts.forms.active.as_ref()
                        && !entry.dirty
                        && entry.save.is_none()
                        && entry.load.is_none()
                        && entry.conflict.is_none()
                        && entry.error.is_none()
                })
                .map(|(key, _)| key.clone());
            if let Some(key) = removable {
                self.facts.forms.entries.remove(&key);
            } else {
                self.error =
                    Some("Save your pending Memory entries before opening another project.".into());
                cx.notify();
                return false;
            }
        }
        self.facts
            .forms
            .entries
            .insert(scope.into(), Entry::default());
        true
    }
    pub(super) fn memory_form_before_switch(&mut self, cx: &mut Context<Self>) -> bool {
        self.memory_form_changed(cx);
        if self.facts.forms.oversize {
            self.error = Some(
                "Shorten this Memory entry or copy it to a file before switching projects.".into(),
            );
            cx.notify();
            return false;
        }
        if let Some(scope) = self.facts.forms.active.clone() {
            self.persist_memory_form(&scope, cx);
        }
        if self.facts.forms.entries.len() >= 64
            && !self.facts.forms.entries.iter().any(|(scope, entry)| {
                Some(scope) != self.facts.forms.active.as_ref()
                    && !entry.dirty
                    && entry.save.is_none()
                    && entry.load.is_none()
                    && entry.conflict.is_none()
                    && entry.error.is_none()
            })
        {
            self.error = Some("Save your pending Memory entries before switching projects.".into());
            cx.notify();
            return false;
        }
        true
    }
    pub(super) fn memory_form_activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let scope = self.scope();
        if !self.ensure_memory_form(&scope, cx) {
            return;
        }
        self.facts.forms.active = Some(scope.clone());
        self.facts.forms.oversize = false;
        self.apply_memory_form(&scope, window, cx);
        let entry = &self.facts.forms.entries[&scope];
        if entry.revision.is_none() && entry.load.is_none() {
            let id = Uuid::new_v4();
            let generation = entry.generation;
            let result = self
                .host
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
                .and_then(|host| host.memory_form_load(id, scope.clone()));
            if result.is_ok() {
                self.facts.forms.entries.get_mut(&scope).unwrap().load = Some((id, generation));
            } else {
                self.facts.forms.entries.get_mut(&scope).unwrap().error = Some(
                    "Your saved Memory entry could not be loaded. Retry before saving.".into(),
                );
            }
            self.result(result, cx);
        }
    }
    fn apply_memory_form(&mut self, scope: &str, window: &mut Window, cx: &mut Context<Self>) {
        if scope != self.scope() || self.facts.forms.active.as_deref() != Some(scope) {
            return;
        }
        let Some(entry) = self.facts.forms.entries.get(scope) else {
            return;
        };
        self.facts.editing = entry.state.editing;
        self.facts.retry = entry.state.retry.clone();
        let text = entry.state.text.clone();
        self.memory_content
            .update(cx, |input, cx| input.set_value(text, window, cx));
    }
    pub(super) fn memory_form_changed(&mut self, cx: &mut Context<Self>) {
        let scope = self.scope();
        if !self.ensure_memory_form(&scope, cx) {
            return;
        }
        if self.facts.forms.active.is_none() {
            self.facts.forms.active = Some(scope.clone());
        }
        if self.facts.forms.active.as_deref() != Some(&scope) {
            return;
        }
        let retry_input = self.facts.retry.as_ref().and_then(|retry| {
            self.facts
                .mutation
                .as_ref()
                .filter(|operation| operation.scope == scope && operation.record.id == retry.id)
                .map(|operation| operation.input.clone())
                .or_else(|| {
                    self.facts
                        .forms
                        .entries
                        .get(&scope)
                        .filter(|entry| {
                            entry
                                .state
                                .retry
                                .as_ref()
                                .is_some_and(|record| record.id == retry.id)
                        })
                        .and_then(|entry| entry.state.retry_input.clone())
                })
        });
        let state = MemoryFormState {
            text: self.memory_content.read(cx).value().to_string(),
            editing: self.facts.editing,
            retry: self.facts.retry.clone(),
            retry_input,
        };
        self.facts.forms.oversize = state.text.len() > 262144;
        if self.facts.forms.oversize {
            cx.notify();
            return;
        }
        let entry = self.facts.forms.entries.get_mut(&scope).unwrap();
        if same(&entry.state, &state) {
            return;
        }
        let changed_input = entry.state.text != state.text || entry.state.editing != state.editing;
        if changed_input {
            self.facts.forms.flush = None;
        }
        entry.state = state;
        entry.generation = entry.generation.saturating_add(1);
        entry.dirty = true;
        entry.error = None;
        if changed_input {
            self.cancel_draft_flush(cx);
            self.cancel_document_drafts_flush(cx);
            self.cancel_recovery_flush(cx);
        }
        self.facts.forms.debounce = Some(cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(300))
                .await;
            let _ = view.update(cx, |view, cx| {
                view.facts.forms.debounce = None;
                view.persist_memory_forms(cx);
            });
        }));
        cx.notify();
    }
    pub(super) fn memory_form_ready(&self) -> bool {
        self.facts
            .forms
            .entries
            .get(&self.scope())
            .is_some_and(|entry| {
                entry.revision.is_some() && entry.conflict.is_none() && !self.facts.forms.oversize
            })
    }
    pub(super) fn durable_memory_retry(&self, scope: &str, target: Uuid) -> bool {
        self.facts.forms.entries.get(scope).is_some_and(|entry| {
            entry.revision.is_some()
                && !entry.dirty
                && entry.save.is_none()
                && entry.conflict.is_none()
                && entry
                    .state
                    .retry
                    .as_ref()
                    .is_some_and(|record| record.id == target)
        })
    }
    pub(super) fn persist_memory_forms(&mut self, cx: &mut Context<Self>) {
        let scopes: Vec<_> = self.facts.forms.entries.keys().cloned().collect();
        for scope in scopes {
            self.persist_memory_form(&scope, cx);
        }
    }
    pub(super) fn persist_memory_form(&mut self, scope: &str, cx: &mut Context<Self>) {
        let Some(entry) = self.facts.forms.entries.get_mut(scope) else {
            return;
        };
        if !entry.dirty || entry.save.is_some() || entry.conflict.is_some() || entry.error.is_some()
        {
            return;
        }
        let Some(revision) = entry.revision else {
            return;
        };
        let save = entry
            .retry
            .take()
            .filter(|save| save.generation == entry.generation)
            .unwrap_or_else(|| Save {
                id: Uuid::new_v4(),
                generation: entry.generation,
                write: MemoryFormWrite {
                    scope: scope.into(),
                    expected_revision: revision,
                    mutation_id: Uuid::new_v4(),
                    form: entry.state.clone(),
                },
            });
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.memory_form_save(save.id, save.write.clone()));
        if result.is_ok() {
            entry.save = Some(save);
        } else {
            entry.retry = Some(save);
            entry.error =
                Some("Your Memory entry remains here. Retry saving its recovery copy.".into());
            self.facts.forms.flush = None;
        }
        self.result(result, cx);
    }
    pub(super) fn memory_form_loaded(
        &mut self,
        id: Uuid,
        result: Result<MemoryFormSnapshot, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scope) = self
            .facts
            .forms
            .entries
            .iter()
            .find(|(_, entry)| entry.load.is_some_and(|(expected, _)| expected == id))
            .map(|(scope, _)| scope.clone())
        else {
            return;
        };
        let entry = self.facts.forms.entries.get_mut(&scope).unwrap();
        entry.load = None;
        match result {
            Ok(snapshot) if snapshot.scope == scope => {
                entry.revision = Some(snapshot.revision);
                if entry.dirty
                    && !snapshot.form.text.is_empty()
                    && !same(&entry.state, &snapshot.form)
                {
                    entry.conflict = Some(snapshot);
                } else if !entry.dirty {
                    entry.state = snapshot.form;
                    entry.error = None;
                }
            }
            _ => {
                entry.error = Some(
                    "Your saved Memory entry could not be loaded. Retry before saving.".into(),
                );
                self.facts.forms.flush = None;
            }
        }
        self.apply_memory_form(&scope, window, cx);
        self.persist_memory_form(&scope, cx);
        self.advance_memory_flush(window, cx);
        cx.notify();
    }
    pub(super) fn memory_form_saved(
        &mut self,
        id: Uuid,
        result: Result<MemoryFormSave, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scope) = self
            .facts
            .forms
            .entries
            .iter()
            .find(|(_, entry)| entry.save.as_ref().is_some_and(|save| save.id == id))
            .map(|(scope, _)| scope.clone())
        else {
            return;
        };
        let entry = self.facts.forms.entries.get_mut(&scope).unwrap();
        let save = entry.save.take().unwrap();
        let mut persisted_retry = None;
        match result {
            Ok(MemoryFormSave::Saved(snapshot))
                if snapshot.scope == scope && same(&snapshot.form, &save.write.form) =>
            {
                entry.revision = Some(snapshot.revision);
                entry.dirty = entry.generation != save.generation;
                entry.error = None;
                entry.retry = None;
                persisted_retry = save.write.form.retry.as_ref().map(|record| record.id);
            }
            Ok(MemoryFormSave::Conflict(snapshot)) if snapshot.scope == scope => {
                entry.revision = Some(snapshot.revision);
                if same(&entry.state, &snapshot.form) {
                    entry.dirty = false;
                    entry.error = None;
                    persisted_retry = entry.state.retry.as_ref().map(|record| record.id);
                } else {
                    entry.conflict = Some(snapshot);
                    self.facts.forms.flush = None;
                }
            }
            _ => {
                entry.retry = Some(save);
                entry.error =
                    Some("Your Memory entry remains here. Retry saving its recovery copy.".into());
                self.facts.forms.flush = None;
                if self
                    .facts
                    .mutation
                    .as_ref()
                    .is_some_and(|operation| operation.scope == scope && !operation.dispatched)
                {
                    self.facts.mutation = None;
                }
            }
        }
        if let Some(target) = persisted_retry {
            self.dispatch_memory_mutation(&scope, target, cx);
        }
        self.persist_memory_form(&scope, cx);
        self.advance_memory_flush(window, cx);
        cx.notify();
    }
    pub(super) fn settle_memory_form(
        &mut self,
        scope: &str,
        record: &MemoryRecord,
        input: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if scope == self.scope() {
            self.memory_form_changed(cx);
        }
        let Some(entry) = self.facts.forms.entries.get_mut(scope) else {
            return;
        };
        if entry.state.text == input && entry.state.editing == record.supersedes {
            entry.state.text.clear();
            entry.state.editing = None;
        } else if record.supersedes.is_some() && entry.state.editing == record.supersedes {
            entry.state.editing = Some(record.id);
        }
        entry.state.retry = None;
        entry.state.retry_input = None;
        entry.error = None;
        entry.dirty = true;
        entry.generation = entry.generation.saturating_add(1);
        self.apply_memory_form(scope, window, cx);
        self.persist_memory_form(scope, cx);
    }
    pub(super) fn memory_forms_flush(
        &mut self,
        action: DraftFlushAction,
        cx: &mut Context<Self>,
    ) -> bool {
        self.memory_form_changed(cx);
        self.facts.forms.debounce = None;
        if self.facts.forms.oversize {
            self.facts.notice = "Shorten or copy your Memory entry before closing Xylo.".into();
            return false;
        }
        if self
            .facts
            .forms
            .entries
            .values()
            .all(|entry| !entry.dirty && entry.save.is_none() && entry.conflict.is_none())
            && self.facts.mutation.is_none()
        {
            return true;
        }
        self.facts.forms.flush = Some(action);
        self.persist_memory_forms(cx);
        cx.notify();
        false
    }
    pub(super) fn memory_forms_flush_pending(&self) -> bool {
        self.facts.forms.flush.is_some()
    }
    pub(super) fn cancel_memory_forms_flush(&mut self, cx: &mut Context<Self>) {
        self.facts.forms.flush = None;
        cx.notify();
    }
    pub(super) fn advance_memory_flush(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.facts.forms.oversize
            || self.facts.mutation.is_some()
            || self
                .facts
                .forms
                .entries
                .values()
                .any(|entry| entry.dirty || entry.save.is_some() || entry.conflict.is_some())
        {
            return;
        }
        if let Some(action) = self.facts.forms.flush.take() {
            match action {
                DraftFlushAction::Quit => self.begin_shutdown(cx),
                DraftFlushAction::Update => self.begin_update_install(window, cx),
            }
        }
    }
    fn resolve_memory_form(
        &mut self,
        use_saved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let scope = self.scope();
        let Some(entry) = self.facts.forms.entries.get_mut(&scope) else {
            return;
        };
        let Some(snapshot) = entry.conflict.take() else {
            return;
        };
        entry.revision = Some(snapshot.revision);
        entry.retry = None;
        entry.error = None;
        if use_saved {
            entry.state = snapshot.form;
            entry.dirty = false;
            if self
                .facts
                .mutation
                .as_ref()
                .is_some_and(|operation| operation.scope == scope && !operation.dispatched)
            {
                self.facts.mutation = None;
            }
        } else {
            entry.generation = entry.generation.saturating_add(1);
            entry.dirty = true;
        }
        self.apply_memory_form(&scope, window, cx);
        self.persist_memory_form(&scope, cx);
        cx.notify();
    }
    /// Draft state under the entry form: saving, saved, a conflict with a
    /// different saved entry, or a failed save with Retry.
    /// Whether the closed form has anything to report (a draft, a conflict,
    /// an error or an oversize warning); a quiet form takes no page space.
    pub(super) fn memory_form_has_status(&self) -> bool {
        self.facts.forms.oversize
            || self
                .facts
                .forms
                .entries
                .get(&self.scope())
                .is_none_or(|entry| {
                    entry.conflict.is_some()
                        || entry.error.is_some()
                        || entry.dirty
                        || entry.save.is_some()
                        || entry.revision.is_none()
                        || !entry.state.text.trim().is_empty()
                        || entry.state.editing.is_some()
                })
    }
    pub(super) fn memory_form_status(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.facts.forms.oversize {
            return settings_ui::caption("Current text is not saved: Memory drafts are limited to 256 KiB (262,144 UTF-8 bytes). Copy your text before shortening it.").into_any_element();
        }
        let Some(entry) = self.facts.forms.entries.get(&self.scope()) else {
            return settings_ui::caption("Loading draft…").into_any_element();
        };
        if entry.conflict.is_none()
            && entry.error.is_none()
            && !entry.dirty
            && entry.save.is_none()
            && entry.revision.is_some()
            && entry.state.text.trim().is_empty()
            && entry.state.editing.is_none()
        {
            return div().into_any_element();
        }
        if entry.conflict.is_some() {
            return div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(notice(
                    BadgeTone::Warning,
                    "A different saved Memory entry exists. Your text is still in the form. Choose which entry to keep.",
                ))
                .child(settings_ui::form_actions([
                    settings_ui::text_button("memory-form-use-saved", "Use saved entry")
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.resolve_memory_form(true, window, cx)
                        }))
                        .into_any_element(),
                    settings_ui::secondary_button("memory-form-keep", "Keep this entry")
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.resolve_memory_form(false, window, cx)
                        }))
                        .into_any_element(),
                ]))
                .into_any_element();
        }
        if let Some(error) = &entry.error {
            return div()
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(settings_ui::inline_error(error)),
                )
                .child(
                    settings_ui::text_button("memory-form-retry", "Retry").on_click(cx.listener(
                        |view, _, window, cx| {
                            let scope = view.scope();
                            if let Some(entry) = view.facts.forms.entries.get_mut(&scope) {
                                entry.error = None;
                            }
                            if view
                                .facts
                                .forms
                                .entries
                                .get(&scope)
                                .is_some_and(|entry| entry.revision.is_none())
                            {
                                view.memory_form_activate(window, cx);
                            } else {
                                view.persist_memory_form(&scope, cx);
                            }
                        },
                    )),
                )
                .into_any_element();
        }
        settings_ui::caption(if entry.save.is_some() || entry.dirty {
            "Saving draft…"
        } else if entry.revision.is_none() {
            "Loading draft…"
        } else {
            "Draft saved"
        })
        .into_any_element()
    }
}
