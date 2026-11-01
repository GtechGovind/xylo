//! Explicit draft deletion uses the existing revisioned store, with one-step undo.
use super::*;

#[derive(Default)]
pub(super) struct SavedDraftUi {
    pending: Option<Mutation>,
    undo: Option<(DraftSnapshot, i64)>,
    pub(super) notice: String,
}
struct Mutation {
    id: Uuid,
    key: DraftKey,
    revision: i64,
    before: Option<DraftSnapshot>,
    restoring: bool,
    generation: Option<u64>,
}
impl SavedDraftUi {
    pub(super) fn busy(&self, key: &DraftKey) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| &pending.key == key)
    }
    pub(super) fn loading(&self, id: Uuid) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.id == id && pending.before.is_none())
    }
    pub(super) fn saving(&self, id: Uuid) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.id == id && pending.before.is_some())
    }
    pub(super) fn can_undo(&self) -> bool {
        self.undo.is_some()
    }
}
impl Xylo {
    fn draft_can_be_removed(&self, key: &DraftKey) -> bool {
        self.drafts.entries.get(key).is_none_or(|entry| {
            !entry.dirty
                && entry.load.is_none()
                && entry.save.is_none()
                && entry.retry.is_none()
                && entry.send.is_none()
                && entry.late.is_none()
                && entry.guidance.is_none()
                && entry.conflict.is_none()
        })
    }
    pub(super) fn delete_saved_draft(&mut self, draft: DraftSummary, cx: &mut Context<Self>) {
        if self.drafts.saved.pending.is_some() {
            return;
        }
        self.draft_changed(cx);
        if !self.draft_can_be_removed(&draft.key) {
            self.drafts.saved.notice =
                "This draft has unsaved changes. Let it save, then delete it.".into();
            self.persist_draft_key(&draft.key, cx);
            cx.notify();
            return;
        }
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Service unavailable"))
            .and_then(|host| host.load_draft(id, draft.key.clone()));
        match result {
            Ok(()) => {
                self.drafts.saved.pending = Some(Mutation {
                    id,
                    generation: self
                        .drafts
                        .entries
                        .get(&draft.key)
                        .map(|entry| entry.generation),
                    key: draft.key,
                    revision: draft.revision,
                    before: None,
                    restoring: false,
                })
            }
            Err(_) => self.drafts.saved.notice = "Couldn’t delete this draft. Try again.".into(),
        }
        cx.notify();
    }
    pub(super) fn saved_draft_loaded(
        &mut self,
        result: Result<DraftSnapshot, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(mut pending) = self.drafts.saved.pending.take() else {
            return;
        };
        match result {
            Ok(snapshot)
                if snapshot.key == pending.key
                    && snapshot.revision == pending.revision
                    && self.draft_can_be_removed(&pending.key) =>
            {
                pending.before = Some(snapshot);
                self.write_saved_draft_mutation(pending, cx);
            }
            _ => {
                self.drafts.saved.notice =
                    "The draft changed. Refresh the list and try again.".into();
                self.drafts.list_request = None;
                self.request_drafts(false, cx);
                self.persist_draft_key(&pending.key, cx);
            }
        }
        cx.notify();
    }
    fn write_saved_draft_mutation(&mut self, mut pending: Mutation, cx: &mut Context<Self>) {
        let before = pending.before.as_ref().expect("Loaded saved draft");
        pending.id = Uuid::new_v4();
        let write = DraftWrite {
            key: pending.key.clone(),
            expected_revision: pending.revision,
            mutation_id: Uuid::new_v4(),
            text: if pending.restoring {
                before.text.clone()
            } else {
                String::new()
            },
            attachments: if pending.restoring {
                before.attachments.clone()
            } else {
                vec![]
            },
        };
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Service unavailable"))
            .and_then(|host| host.save_draft(pending.id, write));
        match result {
            Ok(()) => self.drafts.saved.pending = Some(pending),
            Err(_) => {
                self.drafts.saved.notice = "The draft couldn’t be updated. Try again.".into();
                self.persist_draft_key(&pending.key, cx);
            }
        }
    }
    pub(super) fn undo_saved_draft_delete(&mut self, cx: &mut Context<Self>) {
        if self.drafts.saved.pending.is_some() {
            return;
        }
        let Some((before, revision)) = self.drafts.saved.undo.clone() else {
            return;
        };
        self.draft_changed(cx);
        if !self.draft_can_be_removed(&before.key) {
            self.drafts.saved.notice =
                "This chat has newer draft edits. Save them before undoing.".into();
            cx.notify();
            return;
        }
        self.write_saved_draft_mutation(
            Mutation {
                id: Uuid::new_v4(),
                key: before.key.clone(),
                revision,
                generation: self
                    .drafts
                    .entries
                    .get(&before.key)
                    .map(|entry| entry.generation),
                before: Some(before),
                restoring: true,
            },
            cx,
        );
        cx.notify();
    }
    pub(super) fn saved_draft_written(
        &mut self,
        result: Result<DraftSave, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.drafts.saved.pending.take() else {
            return;
        };
        let before = pending.before.unwrap();
        match result {
            Ok(DraftSave::Saved(snapshot))
                if snapshot.key == pending.key
                    && snapshot.revision > pending.revision
                    && if pending.restoring {
                        snapshot.text == before.text && snapshot.attachments == before.attachments
                    } else {
                        snapshot.is_empty()
                    } =>
            {
                if pending.restoring {
                    self.drafts.saved.undo = None;
                } else {
                    self.drafts.saved.undo = Some((before, snapshot.revision));
                }
                let active = self.drafts.active.as_ref() == Some(&snapshot.key);
                if active {
                    self.draft_changed(cx);
                }
                if let Some(entry) = self.drafts.entries.get_mut(&snapshot.key) {
                    let unchanged = Some(entry.generation) == pending.generation
                        && entry.send.is_none()
                        && entry.guidance.is_none();
                    entry.revision = Some(snapshot.revision);
                    if unchanged {
                        entry.text = snapshot.text.clone();
                        entry.attachments = snapshot.attachments.clone();
                        entry.dirty = false;
                        entry.error = None;
                        entry.send_error = None;
                        entry.generation = entry.generation.wrapping_add(1);
                        if active {
                            self.composer.update(cx, |input, cx| {
                                input.set_value(snapshot.text.clone(), window, cx)
                            });
                        }
                    } else {
                        entry.dirty = true;
                    }
                }
                self.drafts
                    .summaries
                    .retain(|draft| draft.key != snapshot.key);
                self.drafts.saved.notice = if pending.restoring {
                    "Draft restored."
                } else {
                    "Draft deleted."
                }
                .into();
                self.advance_draft_key(&snapshot.key, window, cx);
            }
            Ok(DraftSave::Conflict(_)) => {
                self.drafts.saved.notice =
                    "The draft changed elsewhere and was kept. Reopen it to review.".into()
            }
            _ => self.drafts.saved.notice = "The draft couldn’t be updated. Try again.".into(),
        }
        self.drafts.list_request = None;
        self.request_drafts(false, cx);
        self.persist_draft_key(&pending.key, cx);
        cx.notify();
    }
}

#[cfg(all(test, not(feature = "validation")))]
mod tests {
    use super::{DraftSave, DraftSnapshot, Mutation, Xylo};
    use gpui_kit::{AppContext, TestAppContext, WindowOptions};
    use uuid::Uuid;
    #[gpui_kit::test]
    fn deletion_receipt_preserves_text_typed_after_delete(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (window, view) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Isolated fixture")), window, cx))
            })
            .unwrap()
        });
        cx.update_window(window, |_, window, cx| {
            view.update(cx, |view, cx| {
                view.validation_draft_surface(None, false, window, cx);
                let key = view.current_draft_key();
                let entry = view.drafts.entries.get_mut(&key).unwrap();
                entry.revision = Some(1);
                entry.dirty = false;
                entry.error = None;
                entry.save = None;
                entry.retry = None;
                let before = DraftSnapshot {
                    key: key.clone(),
                    text: entry.text.clone(),
                    attachments: vec![],
                    revision: 1,
                    updated_at: 1,
                };
                view.drafts.saved.pending = Some(Mutation {
                    id: Uuid::new_v4(),
                    key: key.clone(),
                    revision: 1,
                    before: Some(before),
                    restoring: false,
                    generation: Some(entry.generation),
                });
                view.composer.update(cx, |input, cx| {
                    input.set_value("Newer work stays here", window, cx)
                });
                view.draft_changed(cx);
                view.saved_draft_written(
                    Ok(DraftSave::Saved(DraftSnapshot {
                        key: key.clone(),
                        text: String::new(),
                        attachments: vec![],
                        revision: 2,
                        updated_at: 2,
                    })),
                    window,
                    cx,
                );
                assert_eq!(
                    view.composer.read(cx).value().as_str(),
                    "Newer work stays here"
                );
                assert_eq!(view.drafts.entries[&key].text, "Newer work stays here");
                assert_eq!(view.drafts.entries[&key].revision, Some(2));
                assert!(view.drafts.entries[&key].dirty);
                assert!(view.drafts.saved.can_undo());
            })
        })
        .unwrap();
    }
}
