//! Explicit native update controls. The UI receives safe summaries; verified
//! offers and installer bytes remain exclusively in the application host.
//! The controls render as the "About" section of General settings.
use super::settings_ui;
use super::*;
use crate::motion;
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
    /// When the pending update check was sent, for the slow-wait caption.
    check_since: Option<std::time::Instant>,
}
impl Xylo {
    /// Whether any project shell is alive, including shells parked for
    /// projects that are not open; installing an update stops all of them.
    fn shells_running(&self) -> bool {
        self.terminal.is_some() || !self.terminal_cache.is_empty()
    }
    /// Names the projects whose shells a stop would end.
    fn shell_projects(&self) -> Vec<String> {
        let name = |root: &PathBuf| {
            root.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.display().to_string())
        };
        self.terminal
            .as_ref()
            .and(self.workspace.as_ref())
            .into_iter()
            .chain(self.terminal_cache.keys())
            .map(name)
            .collect()
    }
    /// Installing closes Xylo, so open project shells are named and closed
    /// only after the person confirms; the install request then continues.
    fn confirm_close_shells_and_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let detail = format!(
            "Installing closes Xylo. The open project shells of {} will be closed and their running commands end.",
            self.shell_projects().join(", ")
        );
        self.confirm_destructive(
            "Close project shells and install?",
            &detail,
            "Close shells and install",
            window,
            cx,
            |view, window, cx| {
                view.stop_all_shells(cx);
                view.request_update_install(window, cx);
            },
        );
    }
    fn stop_all_shells(&mut self, cx: &mut Context<Self>) {
        self.terminal_subscription = None;
        let shells = self.terminal.take().into_iter().chain(
            std::mem::take(&mut self.terminal_cache)
                .into_values()
                .map(|(terminal, _)| terminal),
        );
        for terminal in shells {
            terminal.update(cx, |terminal, cx| terminal.close_all(cx));
        }
        self.terminal_visible = false;
        self.persist_ui(cx);
        cx.notify();
    }
    fn update_request(&mut self, name: &str, cx: &mut Context<Self>) {
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.capability(id, name.into(), json!({})));
        if let Err(error) = &result {
            self.updates.installing = false;
            // A failed background status refresh must not replace the
            // actionable outcome of the action that triggered it.
            if name != "updates_status" || self.updates.notice.is_empty() {
                self.updates.notice = product_error(&error.to_string());
            }
        } else {
            self.updates.pending.insert(id, name.into());
            if name == "updates_check" {
                self.updates.check_since = Some(self.arm_slow_hint(cx));
            }
            if matches!(name, "updates_check" | "updates_download") {
                self.updates.notice = if name == "updates_check" {
                    "Checking for updates…"
                } else {
                    "Downloading and verifying…"
                }
                .into();
                self.schedule_update_poll(cx);
            }
        }
        cx.notify();
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
        if name == "updates_check" {
            self.updates.check_since = None;
        }
        if name == "updates_install" {
            self.updates.installing = false;
        }
        self.updates.notice = product_error(message);
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
        if name == "updates_check" {
            self.updates.check_since = None;
        }
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
        if self.speech.busy() || self.image_generation.busy() || self.documents.busy() {
            self.updates.notice = "Finish active requests before installing.".into();
            cx.notify();
            return;
        }
        if self.shells_running() {
            self.confirm_close_shells_and_install(window, cx);
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
        if self.shells_running()
            || self.speech.busy()
            || self.image_generation.busy()
            || self.documents.busy()
        {
            self.updates.notice =
                "Stop your project shells and finish active requests before installing.".into();
            cx.notify();
            return;
        }
        self.cancel_project_search(cx);
        self.updates.installing = true;
        window.blur(cx);
        self.update_request("updates_install", cx);
    }
    /// Cancels an update check or download in flight.
    fn cancel_update_request(&mut self) {
        if let Some(host) = &self.host {
            for (id, name) in &self.updates.pending {
                if matches!(name.as_str(), "updates_check" | "updates_download") {
                    let _ = host.cancel_capability(*id);
                }
            }
        }
    }
    /// "About": the running version and the update status row with its one
    /// action (Check, Download or Install), feedback, progress and notes.
    pub(super) fn about_section(&self, cx: &mut Context<Self>) -> Div {
        let status = &self.updates.status;
        let busy = self
            .updates
            .pending
            .values()
            .any(|name| name != "updates_status");
        let offer = status["available"].as_object();
        let version = offer
            .and_then(|offer| offer.get("version"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let can_install = status["can_install"] == true;
        let can_download = status["can_download"] == true;
        let pending = |name: &str| self.updates.pending.values().any(|pending| pending == name);
        let (checking, downloading) = (pending("updates_check"), pending("updates_download"));
        let progress_fraction = status["progress"].as_object().and_then(|progress| {
            let done = progress.get("downloaded_bytes").and_then(Value::as_u64)?;
            let total = progress.get("total_bytes").and_then(Value::as_u64)?;
            (total > 0).then(|| done as f32 / total as f32)
        });
        let mut description = match status["progress"].as_object() {
            Some(progress) => {
                let done = progress
                    .get("downloaded_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let total = progress
                    .get("total_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                format!(
                    "Downloaded {:.1} of {:.1} MB.",
                    done as f64 / 1_000_000.,
                    total as f64 / 1_000_000.
                )
            }
            None if can_install => "Downloaded and verified. Installing closes Xylo.".into(),
            None => "Install after your tasks finish.".into(),
        };
        if self.speech.busy() {
            description
                .push_str(" Finish your recording, playback or speech task before installing.");
        }
        let primary_action = if can_install {
            settings_ui::primary_button("install-app-update", "Install and close Xylo")
                .disabled(busy || self.speech.busy())
                .on_click(
                    cx.listener(|view, _, window, cx| view.request_update_install(window, cx)),
                )
                .map(|button| settings_ui::busy(button, self.updates.installing))
        } else if can_download {
            settings_ui::primary_button("download-app-update", "Download update")
                .disabled(busy)
                .on_click(cx.listener(|view, _, _, cx| view.update_request("updates_download", cx)))
                .map(|button| settings_ui::busy(button, downloading))
        } else {
            settings_ui::primary_button("check-app-updates", "Check for updates")
                .disabled(busy || status["can_check"] != true)
                .on_click(cx.listener(|view, _, _, cx| view.update_request("updates_check", cx)))
                .map(|button| settings_ui::busy(button, checking))
        };
        let mut controls = div().flex().items_center().gap(px(8.));
        if busy {
            controls = controls.child(
                settings_ui::text_button("cancel-app-update", "Cancel")
                    .on_click(cx.listener(|view, _, _, _| view.cancel_update_request())),
            );
        } else if can_install || can_download {
            controls = controls.child(
                settings_ui::text_button("check-app-updates", "Check again")
                    .disabled(status["can_check"] != true)
                    .on_click(
                        cx.listener(|view, _, _, cx| view.update_request("updates_check", cx)),
                    ),
            );
        }
        controls = controls.child(primary_action);
        let headline = match status["phase"].as_str() {
            Some("checking") => "Checking for updates…".to_owned(),
            Some("downloading") => "Downloading update…".to_owned(),
            Some("installing") => "Installing update…".to_owned(),
            _ if can_install => format!("Xylo {version} is ready to install"),
            _ if offer.is_some() => format!("Xylo {version} is available"),
            Some("up_to_date") => "You're up to date".to_owned(),
            _ => "Update status".to_owned(),
        };
        let feedback = (!self.updates.notice.is_empty())
            .then(|| settings_ui::Feedback::Pending(self.updates.notice.clone().into()));
        let status_row = settings_ui::setting_row(
            headline.clone(),
            Some(description.into()),
            Some(controls.into_any_element()),
        )
        .feedback(feedback);
        let mut application = div().w_full().flex().flex_col().child(settings_ui::rows([
            settings_ui::setting_row(
                "Current version",
                None,
                Some(
                    settings_ui::value_text(format!("Xylo {}", env!("CARGO_PKG_VERSION")))
                        .into_any_element(),
                ),
            )
            .into_any_element(),
            motion::fade_on_change("update-status", &headline, div().w_full().child(status_row))
                .into_any_element(),
        ]));
        if let Some(fraction) = progress_fraction.filter(|_| downloading || !can_install) {
            application = application.child(
                div()
                    .pb(px(12.))
                    .child(motion::progress_bar("update-progress", fraction)),
            );
        }
        if checking {
            application =
                application.children(self.updates.check_since.and_then(settings_ui::slow_caption));
        }
        if let Some(notes) = offer
            .and_then(|offer| offer.get("notes"))
            .and_then(Value::as_str)
            .filter(|notes| !notes.trim().is_empty())
        {
            application = application.child(
                div()
                    .id("update-release-notes-frame")
                    .max_h(px(180.))
                    .overflow_y_scroll()
                    .mb(px(12.))
                    .p(px(12.))
                    .rounded(px(settings_ui::CONTAINER_RADIUS))
                    .border_1()
                    .border_color(rgb(border()))
                    .child(
                        TextView::markdown("update-release-notes", notes.to_string())
                            .selectable(true),
                    ),
            );
        }
        settings_ui::section("About", None, application)
    }
}
