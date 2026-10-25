//! Persisted save-copy intents and receipt reconciliation. Publication never
//! starts until the exact intent has been acknowledged by the draft store.
use super::*;
impl Workbench {
    pub(in super::super) fn document_save_copy(&mut self, cx: &mut Context<Self>) {
        if !self.documents.ready() || self.documents.pending_copy.is_some() {
            return;
        }
        let name = self.documents.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.documents.notice = "Give the copy a name first.".into();
            cx.notify();
            return;
        }
        if let Some(message) = validate_edits(&self.documents.edits) {
            self.documents.notice = message;
            cx.notify();
            return;
        }
        self.documents.copy_needs_dispatch = true;
        self.documents.pending_copy = Some(DocumentCopyIntent {
            operation_id: Uuid::new_v4(),
            generation: self.documents.generation,
            name,
            edits: self.documents.edits.clone(),
        });
        self.documents.notice = "Saving your edits before creating the copy…".into();
        self.document_draft_changed(cx);
        self.document_persist_active(cx);
    }
    pub(in super::super) fn document_dispatch_copy(&mut self, cx: &mut Context<Self>) {
        if !self.documents.copy_needs_dispatch
            || self.documents.copy_request.is_some()
            || !self.documents.drafts.ready()
        {
            return;
        }
        let Some(intent) = self.documents.pending_copy.as_ref() else {
            return;
        };
        let Some(source) = self.documents.source.as_ref() else {
            return;
        };
        self.documents.copy_needs_dispatch = false;
        let id = Uuid::new_v4();
        let args = json!({"operation_id":intent.operation_id,"source":source,"name":intent.name,"edits":intent.edits});
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.capability(id, "document_save_copy".into(), args));
        if result.is_ok() {
            self.documents.copy_request = Some(id);
            self.documents.notice = "Creating your copy…".into();
        } else {
            self.documents.notice =
                "The copy was not requested. Your edits remain saved; check status to continue."
                    .into();
        }
        self.result(result, cx);
    }
    pub(super) fn document_cancel_copy(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.documents.copy_request
            && let Some(host) = &self.host
        {
            let result = host.cancel_capability(id);
            if result.is_ok() {
                self.documents.notice="Cancelling this save request. Its result will be checked before another copy is allowed.".into();
            }
            self.result(result, cx);
        }
    }
    pub(super) fn document_check_copy(&mut self, cx: &mut Context<Self>) {
        if self.documents.copy_request.is_some() {
            return;
        }
        let Some(intent) = &self.documents.pending_copy else {
            return;
        };
        let id = Uuid::new_v4();
        let args = json!({"operation_id":intent.operation_id});
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.capability(id, "document_save_status".into(), args));
        if result.is_ok() {
            self.documents.copy_request = Some(id);
            self.documents.notice = "Checking the saved copy…".into();
        }
        self.result(result, cx);
    }
    pub(super) fn document_copy_receipt(
        &mut self,
        receipt: DocumentSaveReceipt,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(intent) = self.documents.pending_copy.as_ref() else {
            return;
        };
        if intent.operation_id != receipt.operation_id {
            return;
        }
        self.documents.notice = receipt.message;
        match receipt.state {
            DocumentSaveState::Committed => {
                let Some(result) = receipt
                    .result
                    .filter(|result| self.documents.source.as_ref() == Some(&result.source))
                else {
                    self.documents.notice =
                        "The copy result is incomplete. Check its status before saving again."
                            .into();
                    return;
                };
                let has_newer_edits = self.documents.edits != intent.edits;
                self.record_artifact(result.path.clone());
                self.documents.output = Some(result.path);
                self.documents.notice = if has_newer_edits {
                    "Your copy is ready. Newer edits remain in this document."
                } else {
                    "Your copy is ready. The source is unchanged."
                }
                .into();
                // The source remains the imported original. Retain all edits as the
                // current proposal, including typing during the save: clearing them would
                // display the original while claiming the saved copy was the editor.
                self.documents.pending_copy = None;
                self.document_draft_changed(cx);
                self.document_persist_active(cx);
                self.document_workspace_ready(cx);
            }
            DocumentSaveState::Cancelled | DocumentSaveState::Failed => {
                self.documents.pending_copy = None;
                self.document_draft_changed(cx);
                self.document_persist_active(cx);
            }
            DocumentSaveState::Prepared | DocumentSaveState::OutcomeUnknown => {}
        }
    }
}
