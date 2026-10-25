//! Revision-checked recovery drafts are independent of chat-composer text.
use super::*;
impl Workbench {
    pub(in crate::workbench) fn recovery_note_changed(&mut self, cx: &mut Context<Self>) {
        if self.recovery.restoring {
            return;
        }
        let Some(scope) = self.recovery.scope.clone() else {
            return;
        };
        let text = self.recovery.note.read(cx).value().to_string();
        let entry = self.recovery.entries.get_mut(&key(&scope)).unwrap();
        if entry.revision.is_none() || text == entry.form.evidence {
            return;
        }
        entry.form.evidence = text;
        entry.generation = entry.generation.saturating_add(1);
        entry.dirty = true;
        entry.error = None;
        self.recovery_cancel_close_intents(cx);
        self.recovery.debounce = Some(cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(300))
                .await;
            let _ = view.update(cx, |view, cx| {
                view.recovery.debounce = None;
                view.persist_recovery_forms(cx);
            });
        }));
        cx.notify();
    }
    pub(in crate::workbench) fn recovery_load_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scope) = self.recovery.scope.clone() else {
            return;
        };
        let entry = self.recovery.entries.get_mut(&key(&scope)).unwrap();
        if entry.revision.is_some() {
            self.recovery_restore_selection(window, cx);
            return;
        }
        if entry.load.is_some() {
            return;
        }
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.action_review_form(id, scope));
        if result.is_ok() {
            entry.load = Some(id);
            entry.load_watchdog = Some(cx.spawn(async move |view, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(30))
                    .await;
                let _ = view.update(cx, |view, cx| {
                    if let Some(entry) = view
                        .recovery
                        .entries
                        .values_mut()
                        .find(|entry| entry.load == Some(id))
                    {
                        entry.load = None;
                        entry.load_watchdog = None;
                        entry.error = Some(
                            "Your saved review has not arrived. Refresh to check it again.".into(),
                        );
                        cx.notify();
                    }
                });
            }));
        } else {
            entry.error = Some("Your saved review could not open. Try opening it again.".into());
        }
        cx.notify();
    }
    fn recovery_restore_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_recovery_form(window, cx);
        let Some(entry) = self.recovery.entry() else {
            return;
        };
        let selected = entry.form.selected;
        let pending = entry.form.pending.clone();
        if let Some(selected) = selected {
            self.recovery_read(ReadKind::Detail(selected), cx);
        }
        if let Some(request) = pending {
            self.recovery.open = true;
            self.recovery_read(ReadKind::Receipt(request), cx);
        }
    }
    pub(in crate::workbench) fn apply_recovery_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self
            .recovery
            .entry()
            .map(|entry| entry.form.evidence.clone())
            .unwrap_or_default();
        self.recovery.restoring = true;
        self.recovery
            .note
            .update(cx, |input, cx| input.set_value(text, window, cx));
        self.recovery.restoring = false;
    }
    pub(in crate::workbench) fn action_review_form_received(
        &mut self,
        id: Uuid,
        result: Result<ActionReviewFormSnapshot, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scope_key) = self
            .recovery
            .entries
            .iter()
            .find(|(_, entry)| entry.load == Some(id))
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        let entry = self.recovery.entries.get_mut(&scope_key).unwrap();
        entry.load = None;
        entry.load_watchdog = None;
        match result {
            Ok(snapshot)
                if key(&snapshot.scope) == scope_key
                    && snapshot.form.pending.as_ref().is_none_or(|pending| {
                        pending.scope == snapshot.scope
                            && Some(pending.action_id) == snapshot.form.selected
                    }) =>
            {
                entry.revision = Some(snapshot.revision);
                if !entry.dirty {
                    entry.form = snapshot.form;
                }
                entry.error = None;
                if self
                    .recovery
                    .scope
                    .as_ref()
                    .is_some_and(|scope| key(scope) == scope_key)
                {
                    self.recovery_restore_selection(window, cx);
                }
            }
            Ok(_) => {
                entry.error = Some(
                    "This saved review does not match the chat. Your current note stays here."
                        .into(),
                )
            }
            Err(error) => entry.error = Some(review_error(&error)),
        }
        cx.notify();
    }
    pub(in crate::workbench) fn persist_recovery_forms(&mut self, cx: &mut Context<Self>) {
        let scopes = self.recovery.entries.keys().cloned().collect::<Vec<_>>();
        for scope_key in scopes {
            let entry = self.recovery.entries.get_mut(&scope_key).unwrap();
            if !entry.dirty
                || entry.save.is_some()
                || entry.conflict.is_some()
                || entry.revision.is_none()
            {
                continue;
            }
            if entry.form.evidence.len() > 4096 || entry.form.evidence.contains('\0') {
                entry.error = Some(
                    "Shorten or copy your review note; it can contain at most 4096 bytes.".into(),
                );
                continue;
            }
            let mut attempt = entry.retry.take().unwrap_or_else(|| Save {
                request_id: Uuid::nil(),
                generation: entry.generation,
                write: ActionReviewFormWrite {
                    scope: ActionReviewScope {
                        session_id: scope_key.0,
                        workspace: scope_key.1.clone(),
                    },
                    expected_revision: entry.revision.unwrap(),
                    form: entry.form.clone(),
                },
            });
            let id = Uuid::new_v4();
            attempt.request_id = id;
            let result = self
                .host
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
                .and_then(|host| host.save_action_review_form(id, attempt.write.clone()));
            if result.is_ok() {
                entry.save = Some(attempt);
                entry.save_watchdog = Some(cx.spawn(async move |view, cx| {
                    cx.background_executor().timer(std::time::Duration::from_secs(30)).await;
                    let _ = view.update(cx, |view, cx| {
                        if let Some(entry) = view.recovery.entries.values_mut().find(|entry| entry.save.as_ref().is_some_and(|save| save.request_id == id)) {
                            entry.retry = entry.save.take();
                            entry.save_watchdog = None;
                            entry.error = Some("Your note's save acknowledgment has not arrived. Save the note again to check the same saved version; your current typing stays here.".into());
                            view.cancel_recovery_flush(cx);
                        }
                    });
                }));
            } else {
                entry.retry = Some(attempt);
                entry.error = Some("Your review note could not be saved. It remains here.".into());
            }
        }
        cx.notify();
    }
    pub(in crate::workbench) fn action_review_form_saved(
        &mut self,
        id: Uuid,
        result: Result<ActionReviewFormSave, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scope_key) = self
            .recovery
            .entries
            .iter()
            .find(|(_, entry)| {
                entry
                    .save
                    .as_ref()
                    .is_some_and(|save| save.request_id == id)
            })
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        let entry = self.recovery.entries.get_mut(&scope_key).unwrap();
        let attempt = entry.save.take().unwrap();
        entry.save_watchdog = None;
        match result {
            Ok(saved)
                if key(&saved.snapshot.scope) == scope_key
                    && saved.snapshot.revision > attempt.write.expected_revision
                    && saved.snapshot.form == attempt.write.form =>
            {
                // An exact conflicting snapshot is also a lost-ACK proof. Later
                // typing stays dirty and is saved using the new durable revision.
                entry.revision = Some(saved.snapshot.revision);
                entry.error = None;
                entry.dirty = entry.generation != attempt.generation;
            }
            Ok(saved) if key(&saved.snapshot.scope) == scope_key && !saved.saved => {
                entry.conflict = Some(saved.snapshot);
                entry.error = Some(
                    "This review was edited elsewhere. Compare the saved review before continuing."
                        .into(),
                );
            }
            Ok(_) => {
                entry.retry = Some(attempt);
                entry.error = Some(
                    "The saved note did not match this review. Your note and request are retained."
                        .into(),
                );
            }
            Err(error) => {
                entry.retry = Some(attempt);
                entry.error = Some(review_error(&error));
            }
        }
        let save_more = entry.dirty && entry.conflict.is_none() && entry.error.is_none();
        let dispatch = entry.dispatch_after_save
            && !entry.dirty
            && entry.conflict.is_none()
            && entry.error.is_none();
        if save_more {
            self.persist_recovery_forms(cx);
        }
        if dispatch
            && self
                .recovery
                .scope
                .as_ref()
                .is_some_and(|scope| key(scope) == scope_key)
        {
            self.recovery
                .entries
                .get_mut(&scope_key)
                .unwrap()
                .dispatch_after_save = false;
            self.dispatch_observed_outcome(cx);
        }
        self.advance_recovery_flush(window, cx);
        cx.notify();
    }
    pub(in crate::workbench) fn recovery_flush(
        &mut self,
        action: DraftFlushAction,
        cx: &mut Context<Self>,
    ) -> bool {
        self.recovery_note_changed(cx);
        self.recovery.debounce = None;
        if !self.recovery.mutation_busy()
            && self.recovery.entries.values().all(|entry| {
                !entry.dirty
                    && entry.save.is_none()
                    && entry.retry.is_none()
                    && entry.conflict.is_none()
            })
        {
            return true;
        }
        self.recovery.flush = Some(action);
        self.persist_recovery_forms(cx);
        cx.notify();
        false
    }
    pub(in crate::workbench) fn recovery_flush_pending(&self) -> bool {
        self.recovery.flush.is_some()
    }
    pub(in crate::workbench) fn recovery_cancel_close_intents(&mut self, cx: &mut Context<Self>) {
        self.cancel_recovery_flush(cx);
        self.cancel_draft_flush(cx);
        self.cancel_memory_forms_flush(cx);
        self.cancel_document_drafts_flush(cx);
    }
    pub(in crate::workbench) fn cancel_recovery_flush(&mut self, cx: &mut Context<Self>) {
        self.recovery.flush = None;
        cx.notify();
    }
    pub(in crate::workbench) fn advance_recovery_flush(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.recovery.mutation_busy()
            || self.recovery.entries.values().any(|entry| {
                entry.dirty
                    || entry.save.is_some()
                    || entry.retry.is_some()
                    || entry.conflict.is_some()
            })
        {
            return;
        }
        if let Some(action) = self.recovery.flush.take() {
            match action {
                DraftFlushAction::Quit => self.begin_shutdown(cx),
                DraftFlushAction::Update => self.begin_update_install(window, cx),
            }
        }
    }
    pub(in crate::workbench) fn choose_saved_recovery_form(
        &mut self,
        use_saved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scope) = self.recovery.scope.clone() else {
            return;
        };
        let Some(entry) = self.recovery.entries.get_mut(&key(&scope)) else {
            return;
        };
        let Some(saved) = entry.conflict.clone() else {
            return;
        };
        if entry.form.pending.is_some() || (!use_saved && saved.form.pending.is_some()) {
            self.recovery.notice =
                "Check the pending recorded outcome before replacing a saved review.".into();
            cx.notify();
            return;
        }
        if use_saved {
            let epoch = self.recovery.epoch;
            let generation = entry.generation;
            self.recovery.confirming = true;
            let answer = window.prompt(PromptLevel::Warning, "Use the saved review?", Some("Your current note will be replaced by the saved review. The interrupted action is not executed."),
                &[PromptButton::cancel("Keep my note"), PromptButton::new("Use saved review")], cx);
            cx.spawn_in(window, async move |view, cx| {
                let answer = answer.await;
                let _ = view.update_in(cx, |view, window, cx| {
                    view.recovery.confirming = false;
                    if matches!(answer, Ok(1))
                        && view.recovery.epoch == epoch
                        && view
                            .recovery
                            .entry()
                            .is_some_and(|entry| entry.generation == generation)
                    {
                        let entry = view.recovery.entries.get_mut(&key(&saved.scope)).unwrap();
                        entry.form = saved.form;
                        entry.revision = Some(saved.revision);
                        entry.dirty = false;
                        entry.retry = None;
                        entry.conflict = None;
                        entry.error = None;
                        view.recovery_restore_selection(window, cx);
                    }
                    cx.notify();
                });
            })
            .detach();
        } else {
            entry.revision = Some(saved.revision);
            entry.retry = None;
            entry.conflict = None;
            entry.error = None;
            entry.dirty = true;
            self.persist_recovery_forms(cx);
        }
    }
}
