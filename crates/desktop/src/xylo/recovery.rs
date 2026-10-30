//! Operator review observes durable effects; it never re-executes an action.
use super::*;
use xylo_runtime::{
    ActionResolutionOutcome, ActionResolutionReceipt, ActionResolutionRequest, ActionReviewCursor,
    ActionReviewDetail, ActionReviewForm, ActionReviewFormSave, ActionReviewFormSnapshot,
    ActionReviewFormWrite, ActionReviewPage, ActionReviewScope, ActionReviewSummary,
};
#[path = "recovery_forms.rs"]
mod forms;
#[path = "recovery_view.rs"]
mod view;

type ScopeKey = (Uuid, PathBuf);
fn key(scope: &ActionReviewScope) -> ScopeKey {
    (scope.session_id, scope.workspace.clone())
}
fn empty_form() -> ActionReviewForm {
    ActionReviewForm {
        selected: None,
        evidence: String::new(),
        outcome: None,
        pending: None,
    }
}
struct Save {
    request_id: Uuid,
    generation: u64,
    write: ActionReviewFormWrite,
}
struct Entry {
    form: ActionReviewForm,
    revision: Option<u64>,
    generation: u64,
    dirty: bool,
    load: Option<Uuid>,
    load_watchdog: Option<Task<()>>,
    save_watchdog: Option<Task<()>>,
    save: Option<Save>,
    retry: Option<Save>,
    conflict: Option<ActionReviewFormSnapshot>,
    error: Option<String>,
    dispatch_after_save: bool,
}
impl Default for Entry {
    fn default() -> Self {
        Self {
            form: empty_form(),
            revision: None,
            generation: 0,
            dirty: false,
            load: None,
            load_watchdog: None,
            save_watchdog: None,
            save: None,
            retry: None,
            conflict: None,
            error: None,
            dispatch_after_save: false,
        }
    }
}
#[derive(Clone)]
enum ReadKind {
    Page(Option<ActionReviewCursor>),
    Detail(Uuid),
    Receipt(ActionResolutionRequest),
}
struct Read {
    scope: ActionReviewScope,
    epoch: u64,
    kind: ReadKind,
}
struct Recording {
    request_id: Uuid,
    request: ActionResolutionRequest,
}
pub(super) struct RecoveryUi {
    scope: Option<ActionReviewScope>,
    entries: BTreeMap<ScopeKey, Entry>,
    reads: BTreeMap<Uuid, Read>,
    recording: Option<Recording>,
    waiting_receipts: BTreeMap<Uuid, ActionResolutionRequest>,
    watchdog: Option<Task<()>>,
    items: Vec<ActionReviewSummary>,
    cursor: Option<ActionReviewCursor>,
    requested_cursor: Option<ActionReviewCursor>,
    has_more: bool,
    loaded: bool,
    detail: Option<ActionReviewDetail>,
    pub open: bool,
    note: Entity<TextareaState>,
    output: Entity<TextareaState>,
    subscription: Option<Subscription>,
    restoring: bool,
    confirming: bool,
    epoch: u64,
    debounce: Option<Task<()>>,
    flush: Option<DraftFlushAction>,
    notice: String,
}
impl RecoveryUi {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Xylo>) -> Self {
        Self {
            scope: None,
            entries: BTreeMap::new(),
            reads: BTreeMap::new(),
            recording: None,
            waiting_receipts: BTreeMap::new(),
            watchdog: None,
            items: Vec::new(),
            cursor: None,
            requested_cursor: None,
            has_more: false,
            loaded: false,
            detail: None,
            open: false,
            note: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(3)
                    .placeholder("Describe what you inspected and the outcome you observed")
            }),
            output: cx.new(|cx| TextareaState::new(window, cx).rows(3)),
            subscription: None,
            restoring: false,
            confirming: false,
            epoch: 0,
            debounce: None,
            flush: None,
            notice: String::new(),
        }
    }
    fn entry(&self) -> Option<&Entry> {
        self.entries.get(&key(self.scope.as_ref()?))
    }
    pub(super) fn mutation_busy(&self) -> bool {
        self.confirming || self.recording.is_some()
    }
    fn read_busy(&self) -> bool {
        !self.reads.is_empty()
    }
}
impl Xylo {
    pub(super) fn subscribe_recovery(&mut self, cx: &mut Context<Self>) {
        self.recovery.subscription = Some(cx.subscribe(
            &self.recovery.note,
            |view, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    view.recovery_note_changed(cx);
                }
            },
        ));
    }
    pub(super) fn recovery_scope_changed(&mut self, cx: &mut Context<Self>) {
        for id in self.recovery.reads.keys() {
            if let Some(host) = &self.host {
                let _ = host.cancel_capability(*id);
            }
        }
        self.recovery.reads.clear();
        self.recovery.scope = None;
        self.recovery.epoch = self.recovery.epoch.saturating_add(1);
        self.recovery.items.clear();
        self.recovery.detail = None;
        self.recovery.open = false;
        self.recovery.loaded = false;
        self.recovery.cursor = None;
        self.recovery.has_more = false;
        self.recovery.notice.clear();
        cx.notify();
    }
    pub(super) fn recovery_activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.clone().filter(|_| self.workspace_ready) else {
            return;
        };
        if !self.conversations.iter().any(|c| c.id == self.session)
            && self.messages.is_empty()
            && !self.running()
        {
            return;
        }
        let scope = ActionReviewScope {
            session_id: self.session,
            workspace,
        };
        if self.recovery.scope.as_ref() == Some(&scope) {
            return;
        }
        self.recovery_scope_changed(cx);
        while self.recovery.entries.len() >= 32 {
            let remove = self
                .recovery
                .entries
                .iter()
                .find(|(_, e)| {
                    !e.dirty
                        && e.save.is_none()
                        && e.load.is_none()
                        && e.form.pending.is_none()
                        && e.conflict.is_none()
                })
                .map(|(key, _)| key.clone());
            if let Some(remove) = remove {
                self.recovery.entries.remove(&remove);
            } else {
                self.recovery.notice =
                    "Save your open review notes before opening another chat.".into();
                return;
            }
        }
        self.recovery.scope = Some(scope.clone());
        self.recovery.entries.entry(key(&scope)).or_default();
        self.recovery_load_form(window, cx);
        self.refresh_action_reviews(cx);
    }
    fn recovery_read(&mut self, kind: ReadKind, cx: &mut Context<Self>) {
        let Some(scope) = self.recovery.scope.clone() else {
            return;
        };
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| match &kind {
                ReadKind::Page(cursor) => {
                    host.action_reviews(id, scope.clone(), cursor.clone(), 50)
                }
                ReadKind::Detail(action) => host.action_review(id, scope.clone(), *action),
                ReadKind::Receipt(request) => {
                    host.action_resolution(id, scope.clone(), request.operation_id)
                }
            });
        if result.is_ok() {
            self.recovery.reads.insert(
                id,
                Read {
                    scope,
                    epoch: self.recovery.epoch,
                    kind,
                },
            );
        } else {
            self.recovery.notice = "This review could not be opened. Your note stays here.".into();
        }
        cx.notify();
    }
    pub(super) fn refresh_action_reviews(&mut self, cx: &mut Context<Self>) {
        if self
            .recovery
            .reads
            .values()
            .any(|read| matches!(read.kind, ReadKind::Page(_)))
        {
            return;
        }
        self.recovery_read(ReadKind::Page(None), cx);
    }
    fn more_action_reviews(&mut self, cx: &mut Context<Self>) {
        if self.recovery.read_busy() || !self.recovery.has_more {
            return;
        }
        self.recovery_read(ReadKind::Page(self.recovery.cursor.clone()), cx);
    }
    pub(super) fn action_reviews_received(
        &mut self,
        id: Uuid,
        result: Result<ActionReviewPage, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(read) = self.recovery.reads.remove(&id) else {
            return;
        };
        let ReadKind::Page(cursor) = read.kind else {
            return;
        };
        if read.epoch != self.recovery.epoch || self.recovery.scope.as_ref() != Some(&read.scope) {
            return;
        }
        match result {
            Ok(page)
                if page.scope == read.scope
                    && page.items.len() <= 50
                    && page
                        .items
                        .iter()
                        .all(|item| item.state == "outcome_unknown")
                    && (!page.has_more || page.cursor.is_some())
                    && page
                        .items
                        .iter()
                        .map(|item| item.id)
                        .collect::<BTreeSet<_>>()
                        .len()
                        == page.items.len() =>
            {
                self.recovery.items = page.items;
                self.recovery.cursor = page.cursor;
                self.recovery.requested_cursor = cursor;
                self.recovery.has_more = page.has_more;
                self.recovery.loaded = true;
            }
            Ok(_) => {
                self.recovery.notice =
                    "This result does not match the current chat. Refresh the review.".into()
            }
            Err(error) => self.recovery.notice = review_error(&error),
        }
        cx.notify();
    }
    fn review_select(&mut self, action: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if self.recovery.mutation_busy() {
            return;
        }
        let Some(entry) = self.recovery.entry() else {
            return;
        };
        if entry.revision.is_none() || entry.form.pending.is_some() {
            return;
        }
        if entry.form.selected == Some(action) {
            self.recovery_read(ReadKind::Detail(action), cx);
            return;
        }
        if !entry.form.evidence.is_empty() {
            let epoch = self.recovery.epoch;
            let generation = entry.generation;
            self.recovery.confirming = true;
            let answer = window.prompt(PromptLevel::Warning, "Discard this review note?",
                Some("The interrupted action stays in review. Only this unfinished observation note will be replaced."),
                &[PromptButton::cancel("Keep reviewing"), PromptButton::new("Discard note")], cx);
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
                        view.review_apply_selection(action, window, cx);
                    }
                    cx.notify();
                });
            })
            .detach();
        } else {
            self.review_apply_selection(action, window, cx);
        }
    }
    fn review_apply_selection(
        &mut self,
        action: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(scope) = self.recovery.scope.clone() {
            let entry = self.recovery.entries.get_mut(&key(&scope)).unwrap();
            entry.form = ActionReviewForm {
                selected: Some(action),
                ..empty_form()
            };
            entry.generation = entry.generation.saturating_add(1);
            entry.dirty = true;
            self.recovery_cancel_close_intents(cx);
            self.apply_recovery_form(window, cx);
            self.persist_recovery_forms(cx);
            self.recovery.detail = None;
            self.recovery_read(ReadKind::Detail(action), cx);
        }
    }
    pub(super) fn action_review_received(
        &mut self,
        id: Uuid,
        result: Result<ActionReviewDetail, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(read) = self.recovery.reads.remove(&id) else {
            return;
        };
        let ReadKind::Detail(action) = read.kind else {
            return;
        };
        if read.epoch != self.recovery.epoch
            || self.recovery.scope.as_ref() != Some(&read.scope)
            || self
                .recovery
                .entry()
                .is_none_or(|entry| entry.form.selected != Some(action))
        {
            return;
        }
        match result {
            Ok(detail) if detail.scope == read.scope && detail.action.id == action => {
                let text = detail.result.as_ref().and_then(|payload| payload.json.as_ref()).and_then(observation_text)
                    .unwrap_or_else(|| if detail.result.is_some() { "A saved observation is retained for this action. Inspect the target and its files before recording an outcome.".into() } else { "No confirmed output was returned. Inspect the actual target before recording an outcome.".into() });
                self.recovery.output.update(cx, |input, cx| {
                    input.set_value(truncate(&text, 16000), window, cx)
                });
                self.recovery.detail = Some(detail);
            }
            Ok(_) => {
                self.recovery.notice =
                    "This action does not match the selected chat. Refresh before recording.".into()
            }
            Err(error) => self.recovery.notice = review_error(&error),
        }
        cx.notify();
    }
    fn record_observed_outcome(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.recovery_note_changed(cx);
        if self.recovery.mutation_busy() || self.running() {
            return;
        }
        let (Some(scope), Some(entry), Some(detail)) = (
            self.recovery.scope.clone(),
            self.recovery.entry(),
            self.recovery.detail.as_ref(),
        ) else {
            return;
        };
        if entry.form.pending.is_some()
            || entry.conflict.is_some()
            || entry.revision.is_none()
            || detail.action.busy
            || detail.action.blocked_reason.is_some()
            || detail.action.state != "outcome_unknown"
            || detail.resolution.is_some()
            || entry.form.selected != Some(detail.action.id)
        {
            return;
        }
        let Some(outcome) = entry.form.outcome else {
            self.recovery.notice = "Choose the outcome you observed.".into();
            cx.notify();
            return;
        };
        let evidence = entry.form.evidence.trim().to_owned();
        if evidence.is_empty() || evidence.len() > 4096 {
            self.recovery.notice =
                "Describe what you inspected in a note of at most 4096 bytes.".into();
            cx.notify();
            return;
        }
        let request = ActionResolutionRequest {
            operation_id: Uuid::new_v4(),
            scope,
            action_id: detail.action.id,
            expected_revision: detail.action.revision.clone(),
            outcome,
            evidence,
        };
        let epoch = self.recovery.epoch;
        let generation = entry.generation;
        self.recovery_cancel_close_intents(cx);
        self.recovery.confirming = true;
        let answer = window.prompt(PromptLevel::Warning, "Record this observed outcome?",
            Some("Confirm only after inspecting the actual target. Recording does not repeat the action, undo changes, or verify its execution output."),
            &[PromptButton::cancel("Keep reviewing"), PromptButton::new("Record outcome")], cx);
        cx.spawn_in(window, async move |view, cx| {
            let answer = answer.await;
            let _ = view.update_in(cx, |view, _, cx| {
                view.recovery.confirming = false;
                if matches!(answer, Ok(1))
                    && view.recovery.epoch == epoch
                    && !view.running()
                    && view.recovery.entry().is_some_and(|entry| {
                        entry.generation == generation && entry.form.pending.is_none()
                    })
                {
                    let entry = view.recovery.entries.get_mut(&key(&request.scope)).unwrap();
                    entry.form.pending = Some(request);
                    entry.dispatch_after_save = true;
                    entry.generation = entry.generation.saturating_add(1);
                    entry.dirty = true;
                    view.persist_recovery_forms(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn dispatch_observed_outcome(&mut self, cx: &mut Context<Self>) {
        if self.recovery.mutation_busy() || self.running() {
            return;
        }
        let Some(entry) = self.recovery.entry() else {
            return;
        };
        let Some(request) = entry.form.pending.clone() else {
            return;
        };
        if self.recovery.detail.as_ref().is_none_or(|detail| {
            detail.action.id != request.action_id
                || detail.scope != request.scope
                || detail.action.busy
                || detail.action.blocked_reason.is_some()
                || detail.action.state != "outcome_unknown"
        }) {
            self.recovery.notice =
                "Check the action and its recorded outcome before saving this observation.".into();
            cx.notify();
            return;
        }
        if entry.conflict.is_some()
            || entry.revision.is_none()
            || entry.dirty
            || entry.save.is_some()
        {
            return;
        }
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.resolve_action(id, request.clone()));
        if result.is_ok() {
            self.recovery.recording = Some(Recording {
                request_id: id,
                request,
            });
            self.recovery.watchdog = Some(cx.spawn(async move |view, cx| {
                cx.background_executor().timer(std::time::Duration::from_secs(30)).await;
                let _ = view.update(cx, |view, cx| {
                    if view.recovery.recording.as_ref().is_some_and(|record| record.request_id == id) {
                        let record = view.recovery.recording.take().unwrap();
                        view.recovery.waiting_receipts.insert(id, record.request);
                        while view.recovery.waiting_receipts.len() > 64 { view.recovery.waiting_receipts.pop_first(); }
                        view.recovery.watchdog = None;
                        view.recovery.notice = "The acknowledgment has not arrived. Your exact request is saved; check its recorded outcome. No action was repeated or undone.".into();
                        view.cancel_recovery_flush(cx);
                    }
                });
            }));
        } else {
            self.recovery.notice = "The outcome was not confirmed. Your saved request stays here; check it before continuing.".into();
        }
        cx.notify();
    }
    fn check_observed_outcome(&mut self, cx: &mut Context<Self>) {
        if self.recovery.read_busy() || self.recovery.mutation_busy() {
            return;
        }
        if let Some(request) = self
            .recovery
            .entry()
            .and_then(|entry| entry.form.pending.clone())
        {
            self.recovery_read(ReadKind::Receipt(request), cx);
        }
    }
    pub(super) fn action_resolved(
        &mut self,
        id: Uuid,
        result: Result<ActionResolutionReceipt, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = if self
            .recovery
            .recording
            .as_ref()
            .is_some_and(|record| record.request_id == id)
        {
            self.recovery.watchdog = None;
            self.recovery.recording.take().unwrap().request
        } else if let Some(request) = self.recovery.waiting_receipts.remove(&id) {
            request
        } else {
            return;
        };
        match result {
            Ok(receipt) => self.accept_observed_outcome(request, receipt, window, cx),
            Err(error) => {
                self.recovery.notice = review_error(&error);
                cx.notify();
            }
        }
    }
    pub(super) fn action_resolution_received(
        &mut self,
        id: Uuid,
        result: Result<Option<ActionResolutionReceipt>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(read) = self.recovery.reads.remove(&id) else {
            return;
        };
        let ReadKind::Receipt(request) = read.kind else {
            return;
        };
        if read.epoch != self.recovery.epoch || self.recovery.scope.as_ref() != Some(&read.scope) {
            return;
        }
        match result {
            Ok(Some(receipt)) => self.accept_observed_outcome(request, receipt, window, cx),
            Ok(None) => {
                self.recovery.notice = "No recorded outcome is confirmed yet. The original request and your note are retained. You can save that same observed outcome after checking the action.".into();
                cx.notify();
            }
            Err(error) => {
                self.recovery.notice = review_error(&error);
                cx.notify();
            }
        }
    }
    fn accept_observed_outcome(
        &mut self,
        request: ActionResolutionRequest,
        receipt: ActionResolutionReceipt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if receipt.operation_id != request.operation_id
            || receipt.scope != request.scope
            || receipt.action_id != request.action_id
            || receipt.outcome != request.outcome
            || receipt.evidence != request.evidence
            || receipt.original_revision != request.expected_revision
            || receipt.source != "operator_review"
            || receipt.revision.is_empty()
        {
            self.recovery.notice = "The recorded outcome did not match your saved request. Keep this review and check it again.".into();
            cx.notify();
            return;
        }
        let Some(entry) = self.recovery.entries.get_mut(&key(&request.scope)) else {
            return;
        };
        if entry.form.pending.as_ref() != Some(&request) {
            return;
        }
        entry.form.pending = None;
        entry.dispatch_after_save = false;
        if entry.form.evidence.trim() == request.evidence
            && entry.form.outcome == Some(request.outcome)
        {
            entry.form.evidence.clear();
            entry.form.outcome = None;
        }
        entry.generation = entry.generation.saturating_add(1);
        entry.dirty = true;
        self.recovery
            .waiting_receipts
            .retain(|_, pending| pending.operation_id != request.operation_id);
        if self
            .recovery
            .recording
            .as_ref()
            .is_some_and(|record| record.request.operation_id == request.operation_id)
        {
            self.recovery.recording = None;
            self.recovery.watchdog = None;
        }
        if self.recovery.scope.as_ref() == Some(&request.scope) {
            self.recovery.reads.retain(|_, read| !matches!(read.kind, ReadKind::Detail(action) if action == request.action_id));
            self.recovery
                .items
                .retain(|item| item.id != request.action_id);
            if let Some(detail) = self
                .recovery
                .detail
                .as_mut()
                .filter(|detail| detail.action.id == request.action_id)
            {
                detail.resolution = Some(receipt);
            }
            self.apply_recovery_form(window, cx);
            self.recovery.notice =
                "Your observed outcome is recorded. This did not repeat or undo the action.".into();
            self.refresh_action_reviews(cx);
            if self.recovery.detail.is_none() {
                self.recovery_read(ReadKind::Detail(request.action_id), cx);
            }
        }
        self.persist_recovery_forms(cx);
        self.advance_recovery_flush(window, cx);
        cx.notify();
    }
    fn cancel_review_request(&mut self, cx: &mut Context<Self>) {
        if let Some(recording) = self.recovery.recording.take() {
            if let Some(host) = &self.host {
                let _ = host.cancel_capability(recording.request_id);
            }
            self.recovery
                .waiting_receipts
                .insert(recording.request_id, recording.request);
            while self.recovery.waiting_receipts.len() > 64 {
                self.recovery.waiting_receipts.pop_first();
            }
            self.recovery.watchdog = None;
            self.recovery.notice = "Stopping this request. The observed outcome may already be recorded; check the saved result before continuing.".into();
        } else {
            for id in self.recovery.reads.keys() {
                if let Some(host) = &self.host {
                    let _ = host.cancel_capability(*id);
                }
            }
            self.recovery.reads.clear();
        }
        cx.notify();
    }
    pub(super) fn recovery_before_switch(&mut self, cx: &mut Context<Self>) -> bool {
        self.recovery_note_changed(cx);
        if self.recovery.mutation_busy()
            || self
                .recovery
                .entries
                .values()
                .any(|entry| entry.dirty || entry.save.is_some() || entry.conflict.is_some())
        {
            self.persist_recovery_forms(cx);
            self.error = Some("Wait for your review note and observed outcome to finish saving before switching chats or projects.".into());
            cx.notify();
            false
        } else {
            true
        }
    }
    pub(super) fn recovery_blocks_submit(&self) -> bool {
        self.recovery.mutation_busy()
            || !self.recovery.items.is_empty()
            || self
                .recovery
                .entry()
                .is_some_and(|entry| entry.form.pending.is_some())
    }
}
fn review_error(error: &str) -> String {
    if error.to_ascii_lowercase().contains("cancel") {
        "The request stopped. A recorded outcome may still have been saved; check its result. Your note is retained.".into()
    } else if error.contains("changed") || error.contains("revision") {
        "This action changed. Refresh its details before recording an outcome. Your note is retained.".into()
    } else if error.contains("active")
        || error.contains("busy")
        || error.contains("running")
        || error.contains("stopped working")
    {
        "This action or later work is still active. Let it finish, then check the result before recording.".into()
    } else {
        "This review could not be confirmed. Your saved request and note are retained; check the action and its recorded outcome.".into()
    }
}
fn observation_text(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return match serde_json::from_str::<serde_json::Value>(text) {
            Ok(payload) => observation_text(&payload),
            Err(_) => Some(text.to_owned()),
        };
    }
    for name in ["output", "text", "content", "stdout", "summary"] {
        if let Some(text) = value
            .get(name)
            .and_then(observation_text)
            .filter(|text| !text.is_empty())
        {
            return Some(text);
        }
    }
    None
}
fn review_label(name: &str) -> &'static str {
    match name {
        "write_file" | "file_write" | "apply_patch" => "Update project files",
        "file_edit" => "Edit a project file",
        "run_command" | "shell_command" | "shell" | "terminal" | "execute" => "Run a command",
        "environment_execute" | "remote_execute" => "Run a command in an environment",
        "plugin_run_installed" => "Run a plugin",
        "image_generate" => "Create an image",
        "memory_remember" => "Save a memory",
        "skill_propose" => "Propose a learned skill",
        "document_save_copy" => "Save a document copy",
        "agent_delegate" => "Work with a teammate",
        _ => "Connected tool action",
    }
}

#[cfg(test)]
#[allow(dead_code)]
impl Xylo {
    pub(super) fn validation_review_ready(&self) -> bool {
        self.recovery.loaded
            && !self.recovery.read_busy()
            && self.recovery.entry().is_some_and(|entry| {
                entry.revision.is_some() && entry.save.is_none() && !entry.dirty
            })
    }
    pub(super) fn validation_review_conflict(&self) -> bool {
        self.recovery
            .entry()
            .is_some_and(|entry| entry.conflict.is_some())
    }
    pub(super) fn validation_review_items(&self) -> Vec<ActionReviewSummary> {
        self.recovery.items.clone()
    }
    pub(super) fn validation_review_detail(&self) -> Option<ActionReviewDetail> {
        self.recovery.detail.clone()
    }
    pub(super) fn validation_review_note(&self, cx: &App) -> String {
        self.recovery.note.read(cx).value().to_string()
    }
    pub(super) fn validation_review_output(&self, cx: &App) -> String {
        self.recovery.output.read(cx).value().to_string()
    }
    pub(super) fn validation_review_form(&self) -> Option<ActionReviewForm> {
        self.recovery.entry().map(|entry| entry.form.clone())
    }
    pub(super) fn validation_review_recording(&self) -> Option<(Uuid, ActionResolutionRequest)> {
        self.recovery
            .recording
            .as_ref()
            .map(|record| (record.request_id, record.request.clone()))
    }
    pub(super) fn validation_review_read(&self) -> Option<Uuid> {
        self.recovery.reads.keys().next().copied()
    }
    pub(super) fn validation_review_notice(&self) -> &str {
        &self.recovery.notice
    }
    pub(super) fn validation_review_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.recovery
            .note
            .update(cx, |input, cx| input.focus(window, cx));
    }
    pub(super) fn validation_review_restore_controller(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.recovery = RecoveryUi::new(window, cx);
        self.subscribe_recovery(cx);
        self.recovery_activate(window, cx);
    }
}
