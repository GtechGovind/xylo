//! Explicit native update controls. The UI receives safe summaries; verified
//! offers and installer bytes remain exclusively in the application host.
use super::*;
use serde_json::{Value, json};

#[derive(Default)]
pub(super) struct UpdateUi {
    pub(super) status: Value,
    pub(super) pending: BTreeMap<Uuid, String>,
    pub(super) poll: Option<Task<()>>,
    pub(super) notice: String,
    pub(super) sequence: u64,
    pub installing: bool,
    pub after_save: Option<PathBuf>,
    pub(super) prompt_open: bool,
}
impl Workbench {
    fn update_request(&mut self, name: &str, cx: &mut Context<Self>) {
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.capability(id, name.into(), json!({})));
        if result.is_ok() {
            self.updates.pending.insert(id, name.into());
            if matches!(name, "updates_check" | "updates_download") {
                self.updates.notice = if name == "updates_check" {
                    "Checking for updates…"
                } else {
                    "Downloading and verifying…"
                }
                .into();
                self.schedule_update_poll(cx);
            }
        } else {
            self.updates.installing = false;
        }
        self.result(result, cx);
    }
    pub(super) fn refresh_updates(&mut self, cx: &mut Context<Self>) {
        self.update_request("updates_status", cx);
    }
    fn schedule_update_poll(&mut self, cx: &mut Context<Self>) {
        if self.updates.poll.is_some() || self.quitting {
            return;
        }
        self.updates.poll = Some(cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;
            let _ = view.update(cx, |view, cx| {
                view.updates.poll = None;
                view.refresh_updates(cx);
            });
        }));
    }
    pub(super) fn update_error(&mut self, id: Uuid, message: &str, cx: &mut Context<Self>) -> bool {
        let Some(name) = self.updates.pending.remove(&id) else {
            return false;
        };
        if name == "updates_install" {
            self.updates.installing = false;
        }
        self.updates.notice = message.into();
        self.updates.poll = None;
        if name != "updates_status" {
            self.refresh_updates(cx);
        }
        cx.notify();
        true
    }
    pub(super) fn update_result(
        &mut self,
        id: Uuid,
        value: &Value,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(name) = self.updates.pending.remove(&id) else {
            return false;
        };
        let sequence = value["sequence"].as_u64().unwrap_or(0);
        if sequence >= self.updates.sequence {
            let previous_phase = self.updates.status["phase"].clone();
            self.updates.sequence = sequence;
            self.updates.status = value.clone();
            if let Some(message) = value["message"].as_str() {
                self.updates.notice = message.into();
            } else if (name != "updates_status" || previous_phase != value["phase"])
                && !matches!(
                    value["phase"].as_str(),
                    Some("checking" | "downloading" | "installing")
                )
            {
                self.updates.notice = match value["phase"].as_str() {
                    Some("up_to_date") => "You're up to date.",
                    Some("available") => "An update is available.",
                    Some("ready") => "Update downloaded and verified.",
                    Some("installed") => {
                        "Update installed. Xylo will close; open it again to continue."
                    }
                    _ => "",
                }
                .into();
            }
        }
        if matches!(
            self.updates.status["phase"].as_str(),
            Some("checking" | "downloading")
        ) {
            self.schedule_update_poll(cx);
        } else {
            self.updates.poll = None;
        }
        if name == "updates_install" {
            if self.updates.status["restart_required"] == true {
                self.begin_shutdown(cx);
            } else {
                self.updates.installing = false;
            }
        }
        cx.notify();
        true
    }
    pub(super) fn request_update_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.updates.installing || self.updates.prompt_open || self.updates.after_save.is_some()
        {
            return;
        }
        if self.updates.status["can_install"] != true {
            self.updates.notice = "Download and verify an available update first.".into();
            cx.notify();
            return;
        }
        if self.terminal.is_some()
            || self.speech.busy()
            || self.image_generation.busy()
            || self.documents.busy()
        {
            self.updates.notice =
                "Close the project terminal and finish active requests before installing.".into();
            cx.notify();
            return;
        }
        if !self.editor_dirty {
            self.begin_update_install(window, cx);
            return;
        }
        self.updates.prompt_open = true;
        let answer = window.prompt(
            PromptLevel::Warning,
            "Save changes before installing?",
            Some("Xylo needs to close after installation. Your running tasks must finish first."),
            &[
                PromptButton::ok("Save"),
                PromptButton::new("Discard"),
                PromptButton::cancel("Cancel"),
            ],
            cx,
        );
        cx.spawn_in(window, async move |view, cx| {
            let answer = answer.await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.updates.prompt_open = false;
                match answer {
                    Ok(0) => {
                        view.updates.after_save = view.editor_path.clone();
                        view.request_editor_save(false, window, cx);
                    }
                    Ok(1) => view.begin_update_install(window, cx),
                    _ => cx.notify(),
                }
            });
        })
        .detach();
    }
    pub(super) fn begin_update_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.recovery.mutation_busy() {
            self.updates.notice = "Wait for the observed outcome request before installing.".into();
            cx.notify();
            return;
        }
        if !self.management_check_close(DraftFlushAction::Update, window, cx) {
            return;
        }
        if !self.flush_drafts(DraftFlushAction::Update, cx)
            || !self.memory_forms_flush(DraftFlushAction::Update, cx)
            || !self.document_drafts_flush(DraftFlushAction::Update, cx)
            || !self.recovery_flush(DraftFlushAction::Update, cx)
        {
            return;
        }
        self.updates.after_save = None;
        self.updates.poll = None;
        if self.terminal.is_some()
            || self.speech.busy()
            || self.image_generation.busy()
            || self.documents.busy()
        {
            self.updates.notice =
                "Close the project terminal and finish active requests before installing.".into();
            cx.notify();
            return;
        }
        self.cancel_project_search(cx);
        self.updates.installing = true;
        window.blur(cx);
        self.update_request("updates_install", cx);
    }
    pub(super) fn updates_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let status = &self.updates.status;
        let busy = self
            .updates
            .pending
            .values()
            .any(|name| name != "updates_status");
        let mut panel=card().max_w(px(820.)).child(div().font_weight(FontWeight::MEDIUM).child("Application updates"))
            .child(subtitle(format!("Xylo {}",env!("CARGO_PKG_VERSION"))))
            .child(subtitle("Updates are checked and downloaded only when you choose. Install after your tasks finish."))
            .child(button("check-app-updates","Check for updates").disabled(busy||status["can_check"]!=true)
                .on_click(cx.listener(|view,_,_,cx|view.update_request("updates_check",cx))));
        if let Some(offer) = status["available"].as_object() {
            let version = offer.get("version").and_then(Value::as_str).unwrap_or("");
            let notes = offer.get("notes").and_then(Value::as_str).unwrap_or("");
            panel = panel.child(subtitle(format!("Version {version}"))).child(
                TextView::markdown("update-release-notes", notes.to_string()).selectable(true),
            );
        }
        if let Some(progress) = status["progress"].as_object() {
            let done = progress
                .get("downloaded_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let total = progress
                .get("total_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            panel = panel.child(subtitle(format!(
                "Downloaded {:.1} of {:.1} MB",
                done as f64 / 1_000_000.,
                total as f64 / 1_000_000.
            )));
        }
        panel = panel.child(
            div()
                .flex()
                .gap_2()
                .child(
                    button("download-app-update", "Download update")
                        .disabled(busy || status["can_download"] != true)
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.update_request("updates_download", cx)
                        })),
                )
                .child(
                    primary("install-app-update", "Install and close Xylo")
                        .disabled(
                            busy || status["can_install"] != true
                                || self.terminal.is_some()
                                || self.speech.busy(),
                        )
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.request_update_install(window, cx)
                        })),
                ),
        );
        if self.terminal.is_some() {
            panel = panel
                .child(subtitle(
                    "Close your project terminal before installing. Closing it stops its shell.",
                ))
                .child(
                    button(
                        "close-terminal-for-update",
                        "Close terminal and stop its shell",
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.terminal = None;
                        view.terminal_visible = false;
                        view.persist_ui(cx);
                        cx.notify();
                    })),
                );
        }
        if self.speech.busy() {
            panel = panel.child(subtitle(
                "Finish your recording, playback or speech task before installing.",
            ));
        }
        if busy {
            panel = panel.child(
                button("cancel-app-update", "Cancel download or check").on_click(cx.listener(
                    |view, _, _, _cx| {
                        if let Some(host) = &view.host {
                            for (id, name) in &view.updates.pending {
                                if matches!(name.as_str(), "updates_check" | "updates_download") {
                                    let _ = host.cancel_capability(*id);
                                }
                            }
                        }
                    },
                )),
            );
        }
        div()
            .id("updates-page")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(heading("Updates", "Keep your application up to date."))
            .child(panel.child(subtitle(self.updates.notice.clone())))
            .into_any_element()
    }
}
