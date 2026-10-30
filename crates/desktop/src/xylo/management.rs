//! Shared scope and close guards for explicit native setup. Saving or closing a
//! form never starts a plugin, SSH session, or container on the user's behalf.
use super::*;
impl Xylo {
    pub(super) fn cancel_setup_close_intents(&mut self, cx: &mut Context<Self>) {
        self.cancel_draft_flush(cx);
        self.cancel_memory_forms_flush(cx);
        self.cancel_document_drafts_flush(cx);
        self.cancel_recovery_flush(cx);
    }
    pub(super) fn management_busy(&self) -> bool {
        self.plugins.active() || self.environments.active() || self.extensions.active()
    }
    pub(super) fn management_dirty(&self, cx: &App) -> bool {
        self.plugins.dirty()
            || self.environments.dirty()
            || self
                .extensions
                .setup
                .as_ref()
                .is_some_and(|form| form.dirty(cx))
    }
    pub(super) fn management_before_switch(&mut self, cx: &mut Context<Self>) -> bool {
        if self
            .extensions
            .setup
            .as_ref()
            .is_some_and(|form| form.dirty(cx))
        {
            self.error = Some("Save component setup, or use Back to discard its changes, before switching projects.".into());
            cx.notify();
            return false;
        }
        if self.management_busy() {
            self.error=Some("Wait for the plugin or environment action, or cancel its request, before switching projects.".into());
            cx.notify();
            false
        } else {
            true
        }
    }
    pub(super) fn management_scope_changed(&mut self, cx: &mut Context<Self>) {
        self.extensions.setup = None;
        self.connector_inspection_request = None;
        self.connector_inspected = None;
        self.connector_loading = false;
        self.connector_tools.clear();
        self.plugin_scope_changed(cx);
        self.environment_scope_changed(cx);
    }
    pub(super) fn management_check_close(
        &mut self,
        action: DraftFlushAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.management_confirming {
            return false;
        }
        if self.management_busy() {
            self.error=Some("Wait for the plugin or environment action, or cancel its request, before closing Xylo.".into());
            cx.notify();
            return false;
        }
        if !self.management_dirty(cx) {
            return true;
        }
        let extension_revision = self.extensions.setup.as_ref().map(|form| form.revision(cx));
        let extension_identity = self.extensions.setup.as_ref().map(|form| form.identity());
        let plugin_generation = self.plugins.generation;
        let environment_generation = self.environments.form.generation;
        self.management_confirming = true;
        let title = match action {
            DraftFlushAction::Quit => "Discard unsaved setup before quitting?",
            DraftFlushAction::Update => "Discard unsaved setup before installing the update?",
        };
        let answer = window.prompt(
            PromptLevel::Warning, title,
            Some("Your unsaved component setup, entered secrets, plugin review, permission choices or environment form will be discarded. Saved packages and profiles will stay unchanged."),
            &[PromptButton::cancel("Keep editing"), PromptButton::new("Discard setup changes")], cx,
        );
        cx.spawn_in(window, async move |view, cx| {
            let answer = answer.await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.management_confirming = false;
                if matches!(answer, Ok(1))
                    && extension_revision
                        == view.extensions.setup.as_ref().map(|form| form.revision(cx))
                    && extension_identity
                        == view.extensions.setup.as_ref().map(|form| form.identity())
                    && plugin_generation == view.plugins.generation
                    && environment_generation == view.environments.form.generation
                    && !view.management_busy()
                {
                    view.extensions.setup = None;
                    view.discard_plugin_setup();
                    view.discard_environment_setup(window, cx);
                    match action {
                        DraftFlushAction::Quit => view.request_quit(window, cx),
                        DraftFlushAction::Update => view.begin_update_install(window, cx),
                    }
                }
                cx.notify();
            });
        })
        .detach();
        false
    }
}
