//! Bounded, revision-correlated composer state. All persistence goes through the
//! host; no SQLite/filesystem work occurs on this thread.
use super::*;
use xylo_runtime::drafts::{
    DraftAdmission, DraftCursor, DraftKey, DraftPage, DraftSave, DraftSnapshot, DraftSubmission,
    DraftSummary, DraftWrite, MAX_DRAFT_ATTACHMENTS, MAX_DRAFT_BYTES,
};
const MAX_CACHED_DRAFTS: usize = 128;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DraftFlushAction {
    Quit,
    Update,
}
struct SaveAttempt {
    id: Uuid,
    generation: u64,
    write: DraftWrite,
}
struct Submission {
    id: Uuid,
    generation: u64,
    text: String,
    turn: TurnRequest,
    dispatched: bool,
}
struct GuidanceIntent {
    request_id: Uuid,
    canonical_id: Option<Uuid>,
    target_turn: Option<Uuid>,
    generation: u64,
    text: String,
    dispatched: bool,
}
#[derive(Default)]
struct Entry {
    text: String,
    attachments: Vec<ImageAttachment>,
    revision: Option<i64>,
    generation: u64,
    dirty: bool,
    load: Option<Uuid>,
    save: Option<SaveAttempt>,
    retry: Option<SaveAttempt>,
    send: Option<Submission>,
    conflict: Option<DraftSnapshot>,
    conflict_generation: Option<u64>,
    guidance: Option<GuidanceIntent>,
    guidance_clear: Option<(i64, u64)>,
    error: Option<String>,
}
pub(super) struct DraftUi {
    window: AnyWindowHandle,
    active: Option<DraftKey>,
    entries: BTreeMap<DraftKey, Entry>,
    debounce: Option<Task<()>>,
    timer_generation: u64,
    oversize: bool,
    flush: Option<DraftFlushAction>,
    list_request: Option<(Uuid, Option<PathBuf>, Option<PathBuf>)>,
    list_scope: Option<Option<PathBuf>>,
    summaries: Vec<DraftSummary>,
    before: Option<DraftCursor>,
    has_more: bool,
    notice: String,
    scroll: UniformListScrollHandle,
}
impl DraftUi {
    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn validation_list_request(&self) -> Option<Uuid> {
        self.list_request.as_ref().map(|request| request.0)
    }
    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn validation_summaries(&self) -> &[DraftSummary] {
        &self.summaries
    }
    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn validation_save(&self, key: &DraftKey) -> Option<(Uuid, DraftWrite)> {
        self.entries
            .get(key)?
            .save
            .as_ref()
            .map(|save| (save.id, save.write.clone()))
    }
    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn validation_load(&self, key: &DraftKey) -> Option<Uuid> {
        self.entries.get(key)?.load
    }
    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn validation_revision(&self, key: &DraftKey) -> Option<i64> {
        self.entries.get(key)?.revision
    }
    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn validation_dirty(&self, key: &DraftKey) -> bool {
        self.entries.get(key).is_some_and(|entry| entry.dirty)
    }
    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn validation_capture_guidance(&mut self, key: &DraftKey, id: Uuid) {
        let entry = self.entries.get_mut(key).expect("Loaded draft");
        entry.guidance = Some(GuidanceIntent {
            request_id: id,
            canonical_id: None,
            target_turn: None,
            generation: entry.generation,
            text: entry.text.clone(),
            dispatched: true,
        });
    }
    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn validation_guidance_pending(&self, key: &DraftKey) -> bool {
        self.entries
            .get(key)
            .is_some_and(|entry| entry.guidance.is_some())
    }
    pub(super) fn new(window: &Window) -> Self {
        Self {
            window: window.window_handle(),
            active: None,
            entries: BTreeMap::new(),
            debounce: None,
            timer_generation: 0,
            oversize: false,
            flush: None,
            list_request: None,
            list_scope: None,
            summaries: Vec::new(),
            before: None,
            has_more: false,
            notice: String::new(),
            scroll: UniformListScrollHandle::new(),
        }
    }
}
impl Xylo {
    /// Open terminal assistance as an ordinary, editable saved draft. Preserve
    /// the original chat (including attachments), and use the normal send,
    /// provider, permission and cancellation flow when the user submits it.
    pub(super) fn terminal_assistance_chat(
        &mut self,
        prompt: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if prompt.trim().is_empty() || prompt.len() > MAX_DRAFT_BYTES {
            return;
        }
        if !self.recovery_before_switch(cx) || !self.draft_before_switch(cx) {
            return;
        }
        self.stash_history();
        self.session = Uuid::new_v4();
        self.recovery_scope_changed(cx);
        self.messages.clear();
        self.page = Page::Chat;
        self.settings_open = false;
        let key = self.current_draft_key();
        self.activate_draft(key, window, cx);
        self.composer.update(cx, |input, cx| {
            input.set_value(prompt, window, cx);
            input.focus(window, cx);
        });
        self.draft_changed(cx);
        cx.notify();
    }
    fn current_draft_key(&self) -> DraftKey {
        DraftKey {
            session_id: self.session,
            workspace: self.workspace.clone(),
        }
    }
    fn ensure_draft_entry(&mut self, key: &DraftKey, cx: &mut Context<Self>) -> bool {
        if self.drafts.entries.contains_key(key) {
            return true;
        }
        while self.drafts.entries.len() >= MAX_CACHED_DRAFTS {
            let removable = self
                .drafts
                .entries
                .iter()
                .find(|(candidate, entry)| {
                    Some(*candidate) != self.drafts.active.as_ref()
                        && !entry.dirty
                        && entry.save.is_none()
                        && entry.retry.is_none()
                        && entry.send.is_none()
                        && entry.guidance.is_none()
                        && entry.load.is_none()
                        && entry.conflict.is_none()
                })
                .map(|(key, _)| key.clone());
            if let Some(key) = removable {
                self.drafts.entries.remove(&key);
            } else {
                self.error = Some("Save your pending drafts before opening another chat.".into());
                cx.notify();
                return false;
            }
        }
        self.drafts.entries.insert(key.clone(), Entry::default());
        true
    }
    /// Before changing session/workspace, capture current text and immediately
    /// enqueue its latest coalesced save. A failed save remains cached for retry.
    pub(super) fn draft_before_switch(&mut self, cx: &mut Context<Self>) -> bool {
        self.draft_changed(cx);
        if self.drafts.oversize {
            self.error =
                Some("Shorten this draft or copy it to a file before switching chats.".into());
            cx.notify();
            return false;
        }
        if let Some(key) = self.drafts.active.clone() {
            self.persist_draft_key(&key, cx);
        }
        if self.drafts.entries.len() >= MAX_CACHED_DRAFTS
            && !self.drafts.entries.iter().any(|(key, entry)| {
                Some(key) != self.drafts.active.as_ref()
                    && !entry.dirty
                    && entry.save.is_none()
                    && entry.retry.is_none()
                    && entry.send.is_none()
                    && entry.guidance.is_none()
                    && entry.load.is_none()
                    && entry.conflict.is_none()
            })
        {
            self.error = Some("Save your pending drafts before opening another chat.".into());
            cx.notify();
            return false;
        }
        true
    }
    /// Call after changing session/workspace, including new chat and startup.
    /// The stored window handle lets callers without a Window defer safely.
    pub(super) fn draft_switch_deferred(&mut self, cx: &mut Context<Self>) {
        let requested = self.current_draft_key();
        if !self.ensure_draft_entry(&requested, cx) {
            return;
        }
        self.drafts.debounce = None;
        self.drafts.timer_generation = self.drafts.timer_generation.wrapping_add(1);
        let handle = self.drafts.window;
        let weak = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| {
                let _ = weak.update(cx, |view, cx| {
                    if view.current_draft_key() == requested {
                        view.activate_draft(requested, window, cx);
                    }
                });
            });
        });
    }
    fn activate_draft(&mut self, key: DraftKey, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ensure_draft_entry(&key, cx) {
            return;
        }
        // The old key was captured before the parent mutated its session.
        self.drafts.active = Some(key.clone());
        self.drafts.oversize = false;
        let entry = &self.drafts.entries[&key];
        let text = entry.text.clone();
        let needs_load = entry.revision.is_none() && entry.load.is_none();
        self.composer
            .update(cx, |input, cx| input.set_value(text, window, cx));
        if needs_load {
            self.load_draft_key(&key, cx);
        }
        cx.notify();
    }
    fn load_draft_key(&mut self, key: &DraftKey, cx: &mut Context<Self>) {
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.load_draft(id, key.clone()));
        let entry = self.drafts.entries.get_mut(key).unwrap();
        match result {
            Ok(()) => {
                entry.load = Some(id);
                entry.error = None;
            }
            Err(error) => {
                tracing::warn!(%error,"Enqueue draft load");
                entry.error = Some("Saved drafts could not be loaded. Try again.".into());
            }
        }
        cx.notify();
    }
    /// Subscribe composer InputEvent::Change here. Matching programmatic text is
    /// ignored, so restores do not manufacture mutations or per-token writes.
    pub(super) fn draft_changed(&mut self, cx: &mut Context<Self>) {
        let key = self
            .drafts
            .active
            .clone()
            .unwrap_or_else(|| self.current_draft_key());
        if !self.ensure_draft_entry(&key, cx) {
            return;
        }
        self.drafts.active = Some(key.clone());
        let text = self.composer.read(cx).value().to_string();
        if text.len() > MAX_DRAFT_BYTES {
            if !self.drafts.oversize {
                let entry = self.drafts.entries.get_mut(&key).unwrap();
                entry.generation = entry.generation.wrapping_add(1);
                entry.dirty = true;
                self.cancel_memory_forms_flush(cx);
                self.cancel_document_drafts_flush(cx);
                self.cancel_recovery_flush(cx);
            }
            self.drafts.oversize = true;
            self.error =
                Some("This draft is too large to save. Shorten it or copy it to a file.".into());
            cx.notify();
            return;
        }
        self.drafts.oversize = false;
        let entry = self.drafts.entries.get_mut(&key).unwrap();
        if entry.text == text {
            return;
        }
        entry.text = text;
        entry.generation = entry.generation.wrapping_add(1);
        entry.dirty = true;
        entry.error = None;
        let needs_load = entry.revision.is_none() && entry.load.is_none();
        self.cancel_memory_forms_flush(cx);
        self.cancel_document_drafts_flush(cx);
        self.cancel_recovery_flush(cx);
        if needs_load {
            self.load_draft_key(&key, cx);
        }
        self.schedule_draft_save(cx);
        cx.notify();
    }
    fn schedule_draft_save(&mut self, cx: &mut Context<Self>) {
        self.drafts.timer_generation = self.drafts.timer_generation.wrapping_add(1);
        let generation = self.drafts.timer_generation;
        if self.drafts.flush.is_some() {
            self.persist_all_drafts(cx);
            return;
        }
        self.drafts.debounce = Some(cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(350))
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.drafts.timer_generation == generation {
                    view.drafts.debounce = None;
                    view.persist_all_drafts(cx);
                }
            });
        }));
    }
    fn persist_all_drafts(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<_> = self.drafts.entries.keys().cloned().collect();
        for key in keys {
            self.persist_draft_key(&key, cx);
        }
    }
    fn persist_draft_key(&mut self, key: &DraftKey, cx: &mut Context<Self>) {
        if self.drafts.oversize && self.drafts.active.as_ref() == Some(key) {
            return;
        }
        let Some(entry) = self.drafts.entries.get_mut(key) else {
            return;
        };
        if entry.revision.is_none() {
            if entry.load.is_none() {
                self.load_draft_key(key, cx);
            }
            return;
        }
        if !entry.dirty
            || entry.save.is_some()
            || entry.conflict.is_some()
            || entry.send.as_ref().is_some_and(|send| send.dispatched)
        {
            return;
        }
        let mut attempt = entry.retry.take().unwrap_or_else(|| SaveAttempt {
            id: Uuid::new_v4(),
            generation: entry.generation,
            write: DraftWrite {
                key: key.clone(),
                expected_revision: entry.revision.unwrap(),
                mutation_id: Uuid::new_v4(),
                text: entry.text.clone(),
                attachments: entry.attachments.clone(),
            },
        });
        attempt.id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.save_draft(attempt.id, attempt.write.clone()));
        match result {
            Ok(()) => {
                entry.save = Some(attempt);
                entry.error = None;
            }
            Err(error) => {
                entry.retry = Some(attempt);
                tracing::warn!(%error,"Enqueue draft save");
                entry.error = Some(
                    "Your draft could not be saved. Keep this window open and try again.".into(),
                );
            }
        }
        cx.notify();
    }
    pub(super) fn draft_loaded(
        &mut self,
        id: Uuid,
        result: Result<DraftSnapshot, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self
            .drafts
            .entries
            .iter()
            .find(|(_, entry)| entry.load == Some(id))
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        if self.drafts.active.as_ref() == Some(&key) {
            self.draft_changed(cx);
        }
        let entry = self.drafts.entries.get_mut(&key).unwrap();
        entry.load = None;
        match result {
            Ok(snapshot) if snapshot.key == key => {
                entry.revision = Some(snapshot.revision);
                entry.error = None;
                if !entry.dirty {
                    entry.text = snapshot.text;
                    entry.attachments = snapshot.attachments;
                    if self.drafts.active.as_ref() == Some(&key) {
                        let text = entry.text.clone();
                        self.composer
                            .update(cx, |input, cx| input.set_value(text, window, cx));
                    }
                } else if !snapshot.is_empty()
                    && (snapshot.text != entry.text || snapshot.attachments != entry.attachments)
                {
                    entry.conflict = Some(snapshot);
                }
            }
            Ok(_) => {
                entry.error = Some("The saved draft belongs to a different chat.".into());
            }
            Err(error) => {
                tracing::warn!(detail=%error,"Draft load failed");
                entry.error = Some("The saved draft could not be loaded. Try again.".into());
            }
        }
        self.advance_draft_key(&key, window, cx);
        self.advance_draft_flush(window, cx);
        cx.notify();
    }
    pub(super) fn draft_saved(
        &mut self,
        id: Uuid,
        result: Result<DraftSave, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self
            .drafts
            .entries
            .iter()
            .find(|(_, entry)| entry.save.as_ref().is_some_and(|save| save.id == id))
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        if self.drafts.active.as_ref() == Some(&key) {
            self.draft_changed(cx);
        }
        let entry = self.drafts.entries.get_mut(&key).unwrap();
        let Some(attempt) = entry.save.take() else {
            return;
        };
        match result {
            Ok(DraftSave::Saved(snapshot))
                if snapshot.key == key
                    && snapshot.text == attempt.write.text
                    && snapshot.attachments == attempt.write.attachments
                    && snapshot.revision > attempt.write.expected_revision =>
            {
                // A later durable guidance receipt can overtake this save ACK.
                // Keep its newer revision and any newer local dirty/conflict state.
                if entry
                    .revision
                    .is_none_or(|revision| snapshot.revision >= revision)
                {
                    entry.revision = Some(snapshot.revision);
                    entry.dirty = entry.generation != attempt.generation;
                }
                entry.error = None;
            }
            Ok(DraftSave::Conflict(snapshot)) if snapshot.key == key => {
                entry.revision = Some(snapshot.revision);
                if entry.guidance_clear.is_some_and(|(revision, generation)| {
                    revision == snapshot.revision
                        && snapshot.is_empty()
                        && attempt.generation > generation
                }) {
                    // This is the known guidance CAS-clear racing our NEWER
                    // typing, not an unknown writer. Rebase that newer input.
                    entry.conflict = None;
                    entry.conflict_generation = None;
                } else {
                    entry.conflict_generation = Some(attempt.generation);
                    entry.conflict = Some(snapshot);
                }
                entry.dirty = true;
            }
            Ok(_) => {
                entry.retry = Some(attempt);
                entry.error =
                    Some("Draft save acknowledgement did not match. Try saving again.".into());
            }
            Err(error) => {
                tracing::warn!(detail=%error,"Draft save failed");
                entry.retry = Some(attempt);
                entry.error = Some(
                    "Your draft could not be saved. Keep this window open and try again.".into(),
                );
            }
        }
        if entry.error.is_none() {
            self.advance_draft_key(&key, window, cx);
        }
        self.advance_draft_flush(window, cx);
        cx.notify();
    }
    fn advance_draft_key(&mut self, key: &DraftKey, _window: &mut Window, cx: &mut Context<Self>) {
        let ready = self.drafts.entries.get(key).is_some_and(|entry| {
            entry.revision.is_some()
                && !entry.dirty
                && entry.save.is_none()
                && entry.retry.is_none()
                && entry.conflict.is_none()
                && entry.error.is_none()
        });
        if ready {
            if self
                .drafts
                .entries
                .get(key)
                .is_some_and(|entry| entry.guidance.is_some())
            {
                self.dispatch_persisted_guidance(key, cx);
            } else {
                self.dispatch_draft_submission(key, cx);
            }
        } else if self
            .drafts
            .entries
            .get(key)
            .is_some_and(|entry| entry.error.is_none() && entry.conflict.is_none())
        {
            self.persist_draft_key(key, cx);
        }
    }
    /// Parent submit routes here after its running-task/permission UI decisions.
    /// The captured prompt remains visible until authoritative durable admission.
    pub(super) fn submit_persisted_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.draft_changed(cx);
        if self.drafts.oversize {
            return;
        }
        let key = self.current_draft_key();
        if self.drafts.active.as_ref() != Some(&key) {
            self.error = Some("Wait for this chat's draft to open before sending.".into());
            cx.notify();
            return;
        }
        let Some(workspace) = self.workspace.clone() else {
            self.error = Some("Open a project folder before starting a conversation.".into());
            cx.notify();
            return;
        };
        if let Err(error) = self.settings.provider.validate() {
            self.error = Some(error.to_string());
            self.navigate(Page::Providers, cx);
            cx.notify();
            return;
        }
        let entry = self.drafts.entries.get_mut(&key).unwrap();
        if entry.send.is_some() || entry.guidance.is_some() || entry.conflict.is_some() {
            return;
        }
        if entry.text.trim().is_empty() && entry.attachments.is_empty() {
            return;
        }
        entry.send = Some(Submission {
            id: Uuid::new_v4(),
            generation: entry.generation,
            text: entry.text.clone(),
            turn: TurnRequest {
                session_id: key.session_id,
                turn_id: Uuid::new_v4(),
                prompt: entry.text.trim().to_owned(),
                workspace,
                provider: self.settings.provider.clone(),
                permissions: self.settings.permissions,
                attachments: entry.attachments.clone(),
            },
            dispatched: false,
        });
        self.advance_draft_key(&key, window, cx);
        cx.notify();
    }
    fn dispatch_draft_submission(&mut self, key: &DraftKey, cx: &mut Context<Self>) {
        let Some(entry) = self.drafts.entries.get_mut(key) else {
            return;
        };
        let Some(send) = entry.send.as_mut() else {
            return;
        };
        if send.dispatched {
            return;
        }
        let Some(revision) = entry.revision else {
            return;
        };
        // A save may have coalesced newer typing while a send was preparing. The
        // submitted user intent is immutable; only clear the captured generation.
        let submission = DraftSubmission {
            key: key.clone(),
            expected_revision: revision,
            text: send.text.clone(),
            attachments: send.turn.attachments.clone(),
        };
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.submit_draft(send.id, send.turn.clone(), submission));
        match result {
            Ok(()) => send.dispatched = true,
            Err(error) => {
                entry.send = None;
                entry.error =
                    Some("The message could not be queued. Your draft is still here.".into());
                tracing::warn!(%error,"Draft send enqueue failed");
            }
        }
        cx.notify();
    }
    pub(super) fn draft_admitted(
        &mut self,
        id: Uuid,
        result: Result<DraftAdmission, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self
            .drafts
            .entries
            .iter()
            .find(|(_, entry)| entry.send.as_ref().is_some_and(|send| send.id == id))
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        if self.drafts.active.as_ref() == Some(&key) {
            self.draft_changed(cx);
        }
        let entry = self.drafts.entries.get_mut(&key).unwrap();
        let Some(send) = entry.send.take() else {
            return;
        };
        match result {
            Ok(receipt) if receipt.turn_id == send.turn.turn_id && receipt.draft.key == key => {
                entry.revision = Some(receipt.draft.revision);
                entry.error = None;
                if entry.generation == send.generation && receipt.cleared {
                    entry.text.clear();
                    entry.attachments.clear();
                    entry.dirty = false;
                    entry.generation = entry.generation.wrapping_add(1);
                    if self.drafts.active.as_ref() == Some(&key) {
                        self.composer
                            .update(cx, |input, cx| input.set_value("", window, cx));
                    }
                } else {
                    entry.dirty = entry.text != receipt.draft.text
                        || entry.attachments != receipt.draft.attachments;
                }
                let mut user = message(key.session_id, "user", send.turn.prompt);
                user.id = Uuid::new_v5(&send.turn.turn_id, b"user-message");
                user.attachments = send.turn.attachments;
                if self.session == key.session_id
                    && self.workspace == key.workspace
                    && !self.messages.iter().any(|item| item.id == user.id)
                {
                    self.messages.push(user);
                    self.page = Page::Chat;
                }
                // Only real worker events own running/terminal state. A very
                // fast completed turn can precede this admission receipt.
                self.advance_draft_key(&key, window, cx);
            }
            Ok(_) => {
                entry.error = Some(
                    "The message acknowledgement did not match. Your draft is still here.".into(),
                );
            }
            Err(error) => {
                tracing::warn!(detail=%error,"Draft admission failed");
                self.error = Some("The message could not start. Your draft is still here.".into());
                entry.error = None;
            }
        }
        self.persist_draft_key(&key, cx);
        self.advance_draft_flush(window, cx);
        cx.notify();
    }
    pub(super) fn draft_attachments(&self) -> Vec<ImageAttachment> {
        self.drafts
            .active
            .as_ref()
            .and_then(|key| self.drafts.entries.get(key))
            .map(|entry| entry.attachments.clone())
            .unwrap_or_default()
    }
    pub(super) fn add_draft_attachment(
        &mut self,
        attachment: ImageAttachment,
        cx: &mut Context<Self>,
    ) -> bool {
        self.draft_changed(cx);
        let key = self.current_draft_key();
        let Some(entry) = self.drafts.entries.get_mut(&key) else {
            return false;
        };
        if attachment.session_id != key.session_id
            || entry.attachments.len() >= MAX_DRAFT_ATTACHMENTS
        {
            self.error = Some("Attach at most four images to this chat.".into());
            cx.notify();
            return false;
        }
        if entry
            .attachments
            .iter()
            .any(|image| image.id == attachment.id)
        {
            return true;
        }
        entry.attachments.push(attachment);
        entry.dirty = true;
        entry.generation = entry.generation.wrapping_add(1);
        self.schedule_draft_save(cx);
        true
    }
    pub(super) fn remove_draft_attachment(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if let Some(entry) = self
            .drafts
            .active
            .as_ref()
            .and_then(|key| self.drafts.entries.get_mut(key))
        {
            let before = entry.attachments.len();
            entry.attachments.retain(|image| image.id != id);
            if entry.attachments.len() != before {
                entry.dirty = true;
                entry.generation = entry.generation.wrapping_add(1);
                self.schedule_draft_save(cx);
                cx.notify();
            }
        }
    }
    /// False means teardown must wait. Call this before changing quitting/installing
    /// flags or closing host services. A failed flush keeps the window and text.
    pub(super) fn flush_drafts(
        &mut self,
        action: DraftFlushAction,
        cx: &mut Context<Self>,
    ) -> bool {
        self.draft_changed(cx);
        if self.drafts.oversize {
            return false;
        }
        if self.drafts.entries.values().all(|entry| {
            !entry.dirty
                && entry.save.is_none()
                && entry.retry.is_none()
                && entry.send.is_none()
                && entry.conflict.is_none()
        }) {
            return true;
        }
        self.drafts.flush = Some(action);
        self.drafts.debounce = None;
        self.persist_all_drafts(cx);
        cx.notify();
        false
    }
    pub(super) fn draft_flush_pending(&self) -> bool {
        self.drafts.flush.is_some()
    }
    pub(super) fn cancel_draft_flush(&mut self, cx: &mut Context<Self>) {
        self.drafts.flush = None;
        cx.notify();
    }
    fn advance_draft_flush(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.drafts.oversize
            || self.drafts.entries.values().any(|entry| {
                entry.dirty
                    || entry.save.is_some()
                    || entry.retry.is_some()
                    || entry.send.is_some()
                    || entry.conflict.is_some()
            })
        {
            return;
        }
        if let Some(action) = self.drafts.flush.take() {
            match action {
                DraftFlushAction::Quit => self.begin_shutdown(cx),
                DraftFlushAction::Update => self.begin_update_install(window, cx),
            }
        }
    }
    fn resolve_draft_conflict(
        &mut self,
        use_saved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.drafts.active.clone() else {
            return;
        };
        let Some(entry) = self.drafts.entries.get_mut(&key) else {
            return;
        };
        entry.conflict_generation = None;
        let Some(saved) = entry.conflict.take() else {
            return;
        };
        entry.revision = Some(saved.revision);
        entry.retry = None;
        entry.error = None;
        entry.send = None;
        if use_saved {
            entry.text = saved.text;
            entry.attachments = saved.attachments;
            entry.dirty = false;
            entry.generation = entry.generation.wrapping_add(1);
            let text = entry.text.clone();
            self.composer
                .update(cx, |input, cx| input.set_value(text, window, cx));
        } else {
            entry.dirty = true;
            self.persist_draft_key(&key, cx);
        }
        self.advance_draft_flush(window, cx);
        cx.notify();
    }
    /// Routine autosaving stays quiet; decisions and failures remain visible.
    pub(super) fn draft_needs_attention(&self) -> bool {
        self.drafts.flush.is_some()
            || self
                .drafts
                .active
                .as_ref()
                .and_then(|key| self.drafts.entries.get(key))
                .is_some_and(|entry| {
                    entry.error.is_some() || entry.conflict.is_some() || entry.guidance.is_some()
                })
    }
    pub(super) fn draft_status_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let entry = self
            .drafts
            .active
            .as_ref()
            .and_then(|key| self.drafts.entries.get(key));
        let label = if self.drafts.flush.is_some() {
            "Saving your drafts before closing…".into()
        } else if let Some(error) = entry.and_then(|entry| entry.error.clone()) {
            error
        } else if entry.is_some_and(|entry| entry.conflict.is_some()) {
            "A saved draft arrived after you started typing. Choose which version to keep.".into()
        } else if entry.is_some_and(|entry| entry.guidance.is_some()) {
            "Guidance queued for this task…".into()
        } else if entry.is_some_and(|entry| entry.save.is_some() || entry.dirty) {
            "Saving draft…".into()
        } else {
            String::new()
        };
        let mut row = div()
            .flex()
            .items_center()
            .gap_2()
            .flex_wrap()
            .justify_end()
            .when(!label.is_empty(), |row| row.child(subtitle(label)))
            .child(
                button("saved-drafts", "Saved drafts")
                    .text_xs()
                    .px_2()
                    .min_h(px(28.))
                    .on_click(cx.listener(|view, _, _, cx| view.open_saved_drafts(cx))),
            );
        if entry.is_some_and(|entry| entry.conflict.is_some()) {
            row = row
                .child(
                    button("draft-keep-current", "Keep current text").on_click(cx.listener(
                        |view, _, window, cx| view.resolve_draft_conflict(false, window, cx),
                    )),
                )
                .child(
                    button("draft-use-saved", "Use saved draft").on_click(cx.listener(
                        |view, _, window, cx| view.resolve_draft_conflict(true, window, cx),
                    )),
                );
        }
        if entry.is_some_and(|entry| entry.error.is_some()) {
            row = row.child(
                button("draft-retry", "Try saving again")
                    .on_click(cx.listener(|view, _, _, cx| view.persist_all_drafts(cx))),
            );
        }
        if self.drafts.flush.is_some() {
            row = row.child(
                button("draft-cancel-close", "Keep working")
                    .on_click(cx.listener(|view, _, _, cx| view.cancel_draft_flush(cx))),
            );
        }
        row.into_any_element()
    }
    pub(super) fn open_saved_drafts(&mut self, cx: &mut Context<Self>) {
        self.page = Page::Drafts;
        self.settings_open = false;
        self.drafts.list_scope = None;
        self.request_drafts(false, cx);
    }
    fn request_drafts(&mut self, more: bool, cx: &mut Context<Self>) {
        if self.drafts.list_request.is_some() {
            return;
        }
        let id = Uuid::new_v4();
        let scope = self
            .drafts
            .list_scope
            .clone()
            .unwrap_or_else(|| self.workspace.clone());
        let before = if more {
            self.drafts.before.clone()
        } else {
            None
        };
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.list_drafts(id, scope.clone(), before, 100));
        if result.is_ok() {
            self.drafts.list_request = Some((id, self.workspace.clone(), scope));
        }
        self.result(result, cx);
    }
    pub(super) fn drafts_listed(
        &mut self,
        id: Uuid,
        result: Result<DraftPage, String>,
        cx: &mut Context<Self>,
    ) {
        let Some((expected, workspace, scope)) = &self.drafts.list_request else {
            return;
        };
        if *expected != id {
            return;
        }
        let workspace = workspace.clone();
        let scope = scope.clone();
        self.drafts.list_request = None;
        if self.workspace != workspace {
            return;
        }
        match result {
            Ok(page) if page.workspace == scope => {
                self.drafts.summaries = page.drafts;
                self.drafts.before = page.before;
                self.drafts.has_more = page.has_more;
                self.drafts.notice = if self.drafts.summaries.is_empty() {
                    "No saved drafts in this project.".into()
                } else {
                    String::new()
                };
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(detail=%error,"Draft listing failed");
                self.drafts.notice = "Saved drafts could not be loaded. Try again.".into();
            }
        }
        cx.notify();
    }
    fn open_unscoped_drafts(&mut self, cx: &mut Context<Self>) {
        if self.drafts.list_request.is_some() {
            return;
        }
        self.drafts.list_scope = Some(None);
        self.drafts.before = None;
        self.request_drafts(false, cx);
    }
    fn open_saved_draft_summary(
        &mut self,
        draft: DraftSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.recovery_before_switch(cx) {
            return;
        }
        if self.editor_dirty {
            self.error = Some("Save your changes before opening another draft.".into());
            cx.notify();
            return;
        }
        if !self.draft_before_switch(cx) {
            return;
        }
        if let Some(workspace) = draft.key.workspace.clone() {
            self.open_conversation(draft.key.session_id, workspace, window, cx);
        } else {
            if self.workspace.is_some() && !self.memory_before_switch(cx) {
                return;
            }
            self.new_chat(cx);
            self.workspace = None;
            self.memory_after_switch(window, cx);
            self.settings.last_workspace = None;
            self.reset_file_browser();
            self.diff.clear();
            self.diff_lines.clear();
            self.editor_path = None;
            self.detach_editor_assist();
            self.terminal = None;
            self.terminal_visible = false;
            self.session = draft.key.session_id;
            self.page = Page::Chat;
            self.draft_switch_deferred(cx);
            self.persist_ui(cx);
        }
    }
    pub(super) fn drafts_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.entity().downgrade();
        let rows = uniform_list(
            "draft-results",
            self.drafts.summaries.len(),
            move |range, _, cx| {
                weak.update(cx, |view, cx| {
                    range
                        .filter_map(|index| {
                            let draft = view.drafts.summaries.get(index)?.clone();
                            let label = if draft.preview.trim().is_empty() {
                                format!("{} image attachments", draft.attachment_count)
                            } else {
                                draft
                                    .preview
                                    .split_whitespace()
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            };
                            Some(
                                button(("open-saved-draft", index), label)
                                    .w_full()
                                    .h(px(52.))
                                    .justify_start()
                                    .overflow_hidden()
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        view.open_saved_draft_summary(draft.clone(), window, cx);
                                    }))
                                    .into_any_element(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
            },
        )
        .track_scroll(&self.drafts.scroll)
        .flex_1()
        .min_h_0();
        div()
            .size_full()
            .p_6()
            .flex()
            .flex_col()
            .gap_3()
            .child(heading(
                "Saved drafts",
                "Continue an unsent message in this project.",
            ))
            .child(subtitle(self.drafts.notice.clone()))
            .child(
                button("drafts-without-project", "Drafts before choosing a project")
                    .disabled(self.drafts.list_request.is_some())
                    .on_click(cx.listener(|view, _, _, cx| view.open_unscoped_drafts(cx))),
            )
            .child(rows)
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        button("drafts-refresh", "First drafts")
                            .on_click(cx.listener(|view, _, _, cx| view.request_drafts(false, cx))),
                    )
                    .child(
                        button("drafts-next", "Next drafts")
                            .disabled(!self.drafts.has_more || self.drafts.list_request.is_some())
                            .on_click(cx.listener(|view, _, _, cx| view.request_drafts(true, cx))),
                    ),
            )
            .into_any_element()
    }
}

impl Xylo {
    pub(super) fn submit_persisted_steering(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.draft_changed(cx);
        if self.drafts.oversize {
            return;
        }
        let key = self.current_draft_key();
        if self.drafts.active.as_ref() != Some(&key) {
            return;
        }
        let Some(entry) = self.drafts.entries.get_mut(&key) else {
            return;
        };
        if entry.send.is_some() || entry.guidance.is_some() || entry.conflict.is_some() {
            return;
        }
        if entry.text.trim().is_empty() {
            return;
        }
        if !entry.attachments.is_empty() || entry.text.len() > xylo_core::MAX_STEERING_BYTES {
            self.error =
                Some("Keep task guidance under 16 KiB and send images in a new turn.".into());
            cx.notify();
            return;
        }
        entry.guidance = Some(GuidanceIntent {
            request_id: Uuid::new_v4(),
            canonical_id: None,
            target_turn: None,
            generation: entry.generation,
            text: entry.text.clone(),
            dispatched: false,
        });
        self.advance_draft_key(&key, window, cx);
        cx.notify();
    }
    fn dispatch_persisted_guidance(&mut self, key: &DraftKey, cx: &mut Context<Self>) {
        let Some(entry) = self.drafts.entries.get_mut(key) else {
            return;
        };
        let Some(guidance) = entry.guidance.as_mut() else {
            return;
        };
        if guidance.dispatched {
            return;
        }
        // The captured guidance may have been edited while its initial save was
        // pending. Keep that new input as a draft; never submit a different intent.
        if entry.text != guidance.text || entry.generation != guidance.generation {
            entry.guidance = None;
            self.error = Some(
                "Your draft changed before guidance was sent. Send it again when ready.".into(),
            );
            cx.notify();
            return;
        }
        let Some(revision) = entry.revision else {
            return;
        };
        let submission = DraftSubmission {
            key: key.clone(),
            expected_revision: revision,
            text: guidance.text.clone(),
            attachments: Vec::new(),
        };
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.steer_draft(guidance.request_id, submission));
        match result {
            Ok(()) => guidance.dispatched = true,
            Err(error) => {
                tracing::warn!(%error,"Guidance enqueue failed");
                entry.guidance = None;
                self.error = Some("Guidance could not be queued. Your draft is still here.".into());
            }
        }
        cx.notify();
    }
    pub(super) fn steering_result(
        &mut self,
        id: Uuid,
        result: Result<xylo_runtime::steering::SteeringOutcome, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self
            .drafts
            .entries
            .iter()
            .find(|(_, entry)| {
                entry.guidance.as_ref().is_some_and(|guidance| {
                    guidance.request_id == id || guidance.canonical_id == Some(id)
                })
            })
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        if self.drafts.active.as_ref() == Some(&key) {
            self.draft_changed(cx);
        }
        let entry = self.drafts.entries.get_mut(&key).unwrap();
        let Some(mut guidance) = entry.guidance.take() else {
            return;
        };
        match result {
            Ok(outcome)
                if outcome.request.session_id == key.session_id
                    && outcome.request.text == guidance.text.trim()
                    && outcome.draft.key == key
                    && guidance
                        .target_turn
                        .is_none_or(|turn| turn == outcome.request.turn_id) =>
            {
                guidance.canonical_id = Some(outcome.request.id);
                guidance.target_turn = Some(outcome.request.turn_id);
                match outcome.receipt.state {
                    SteeringState::Queued => {
                        entry.guidance = Some(guidance);
                    }
                    SteeringState::Applied => {
                        let previous_revision = entry.revision.unwrap_or(0);
                        entry.revision = Some(previous_revision.max(outcome.draft.revision));
                        if let Some(clear) = outcome.cleared_revision {
                            entry.guidance_clear = Some((clear, guidance.generation));
                            if entry.conflict.as_ref().is_some_and(|snapshot| {
                                snapshot.revision == clear && snapshot.is_empty()
                            }) && entry
                                .conflict_generation
                                .is_some_and(|generation| generation > guidance.generation)
                            {
                                entry.conflict = None;
                                entry.conflict_generation = None;
                                entry.dirty = true;
                            }
                        }
                        if entry.generation == guidance.generation && !self.drafts.oversize {
                            if outcome.cleared_revision.is_some() || outcome.draft.is_empty() {
                                entry.text = outcome.draft.text.clone();
                                entry.attachments = outcome.draft.attachments.clone();
                                entry.generation = entry.generation.wrapping_add(1);
                                entry.dirty = false;
                                if self.drafts.active.as_ref() == Some(&key) {
                                    let text = entry.text.clone();
                                    self.composer
                                        .update(cx, |input, cx| input.set_value(text, window, cx));
                                }
                            } else if entry.text != outcome.draft.text
                                || entry.attachments != outcome.draft.attachments
                            {
                                entry.conflict = Some(outcome.draft.clone());
                            }
                        } else if previous_revision <= outcome.draft.revision {
                            let differs = entry.text != outcome.draft.text
                                || entry.attachments != outcome.draft.attachments;
                            let own_pending_save = entry.save.as_ref().is_some_and(|attempt| {
                                attempt.write.text == outcome.draft.text
                                    && attempt.write.attachments == outcome.draft.attachments
                                    && attempt.write.expected_revision < outcome.draft.revision
                            });
                            if differs
                                && outcome.draft.revision > previous_revision
                                && !outcome.draft.is_empty()
                                && !own_pending_save
                            {
                                // Only our exact known CAS-clear can rebase
                                // automatically. Preserve an unrelated writer.
                                entry.conflict = Some(outcome.draft.clone());
                                entry.conflict_generation = Some(entry.generation);
                            }
                            entry.dirty |= differs;
                        }
                        let mut user = message(key.session_id, "user", outcome.request.text);
                        user.id = Uuid::new_v5(&outcome.request.id, b"steering-message");
                        if self.session == key.session_id
                            && self.workspace == key.workspace
                            && !self.messages.iter().any(|message| message.id == user.id)
                        {
                            self.messages.push(user);
                        }
                        self.status = "Guidance added to the task.".into();
                    }
                    SteeringState::Cancelled => {
                        // A fresh revision makes a later explicit Send a new
                        // intent; it does not replay the cancelled record itself.
                        entry.generation = entry.generation.wrapping_add(1);
                        entry.dirty = true;
                        self.error = Some(
                            "The task stopped before guidance was added. Your draft is still here."
                                .into(),
                        );
                    }
                }
            }
            Ok(_) => {
                self.error = Some(
                    "Guidance acknowledgement did not match. Your draft is still here.".into(),
                );
            }
            Err(error) => {
                tracing::warn!(detail=%error,"Guidance admission failed");
                self.error = Some("Guidance could not be added. Your draft is still here.".into());
            }
        }
        self.persist_draft_key(&key, cx);
        self.advance_draft_flush(window, cx);
        cx.notify();
    }
}
