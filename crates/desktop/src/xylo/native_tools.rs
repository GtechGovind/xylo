//! Product controls for the isolated native services. Requests are UUID routed;
//! asset pickers, file reads, and services never block the render thread.
use super::*;
use serde_json::{Value, json};
#[derive(Clone, Copy)]
pub(super) enum Asset {
    Document,
}
pub(super) struct NativeUi {
    pub select_local_model: bool,
    pub url: Entity<InputState>,
    pending: BTreeMap<Uuid, (String, Option<(String, Value)>)>,
    pub message: String,
    pub text: String,
    pub preview: Option<PathBuf>,
    pub output: Option<PathBuf>,
}
impl NativeUi {
    pub(super) fn pending_id(&self, label: &str) -> Option<Uuid> {
        self.pending
            .iter()
            .find(|(_, pending)| pending.0 == label)
            .map(|(id, _)| *id)
    }
    pub fn new(window: &mut Window, cx: &mut Context<Xylo>) -> Self {
        Self {
            select_local_model: false,
            url: cx.new(|cx| InputState::new(window, cx).placeholder("https://example.com")),
            pending: BTreeMap::new(),
            message: String::new(),
            text: String::new(),
            preview: None,
            output: None,
        }
    }
}
impl Xylo {
    pub(super) fn capability(
        &mut self,
        name: &str,
        args: Value,
        label: &str,
        after: Option<(String, Value)>,
        cx: &mut Context<Self>,
    ) {
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|h| h.capability(id, name.into(), args));
        if result.is_ok() {
            self.native.pending.insert(id, (label.into(), after));
            if !name.starts_with("helper_asset_") {
                self.native.message = format!("{label}…");
            }
            self.error = None;
        }
        self.result(result, cx);
    }
    pub(super) fn capability_result(
        &mut self,
        id: Uuid,
        name: &str,
        result: Result<Value, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.account_capability_result(id, name, &result, window, cx) {
            return;
        }
        if self.plugin_owns_request(id)
            || name.starts_with("helper_plugin_")
            || name == "plugin_run_installed"
        {
            self.plugin_result(id, result, window, cx);
            return;
        }
        if self.environment_owns_request(id)
            || name.starts_with("helper_environment_")
            || name == "environment_execute"
        {
            self.environment_result(id, result, window, cx);
            return;
        }
        if self.document_owns_request(id)
            || matches!(
                name,
                "document_open" | "document_save_copy" | "document_save_status"
            )
        {
            self.document_capability_result(id, name, result, window, cx);
            return;
        }
        let pending = self.native.pending.remove(&id);
        let label = pending
            .as_ref()
            .map(|p| p.0.as_str())
            .unwrap_or("Task")
            .to_string();
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                if self.notification_error(id, &error, cx) {
                    return;
                }
                if self.speech_error(id, name, &error, cx) {
                    return;
                }
                if self.learning_error(id, &error, cx) {
                    return;
                }
                if self.update_error(id, &error, cx) {
                    return;
                }
                self.native.message = format!("{label} could not finish.");
                self.error = Some(error);
                return;
            }
        };
        let summary = value
            .get("summary")
            .and_then(Value::as_str)
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .unwrap_or_else(|| value.clone());
        if self.notification_result(id, &summary, cx) {
            return;
        }
        if self.speech_result(id, name, &summary, window, cx) {
            return;
        }
        if self.learning_result(id, name, &summary, cx) {
            return;
        }
        if self.update_result(id, &summary, cx) {
            return;
        }
        if self.account_result(name, &summary, window, cx) {
            return;
        }
        if self.asset_result(name, &summary, cx) {
            return;
        }
        let request_output = value
            .get("artifacts")
            .and_then(Value::as_array)
            .and_then(|paths| paths.last())
            .and_then(Value::as_str)
            .map(PathBuf::from);
        if let Some(paths) = value.get("artifacts").and_then(Value::as_array) {
            self.native.output = None;
            self.native.preview = None;
            for path in paths.iter().filter_map(Value::as_str) {
                let path = PathBuf::from(path);
                self.record_artifact(path.clone());
                self.native.output = Some(path.clone());
                if matches!(
                    path.extension().and_then(|s| s.to_str()),
                    Some("png" | "jpg" | "jpeg")
                ) {
                    self.native.preview = Some(path);
                }
            }
        }
        if name == "local_inference_start"
            && let Some(endpoint) = summary.get("endpoint").and_then(Value::as_str)
        {
            let provider = ProviderConfig {
                name: "Local model".into(),
                managed_local: true,
                endpoint: endpoint.into(),
                ..Default::default()
            };
            self.settings.provider = provider.clone();
            self.provider_form(&provider, window, cx);
            self.native.select_local_model = true;
            self.host_call(cx, |h| h.discover_models(provider));
        }
        if name == "document_read" {
            self.native.text = document_text(&summary);
        }
        if name == "speech_transcribe"
            && let Some(path) = request_output
        {
            self.capability(
                "document_read",
                json!({"path":path}),
                "Reading transcript",
                None,
                cx,
            );
        }
        if name == "browser_navigate" {
            self.native.text = summary
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Page opened")
                .into();
        }
        self.native.message = match name {
            "local_inference_start" => "Your local model is ready.".into(),
            "local_inference_stop" => "Local model stopped.".into(),
            "helper_get_config" => "Saved setup loaded.".into(),
            "helper_patch_config" => "Setup saved.".into(),
            "document_export_docx" | "document_export_xlsx" => "Your document is ready.".into(),
            "desktop_screenshot" => "Screenshot saved.".into(),
            "speech_synthesize" => "Your audio is ready.".into(),
            "speech_transcribe" => "Transcript ready.".into(),
            _ => format!("{label} completed."),
        };
        if let Some((_, Some((next, args)))) = pending {
            self.capability(&next, args, "Starting local model", None, cx);
        }
    }
    pub(super) fn pick_asset(&mut self, asset: Asset, window: &mut Window, cx: &mut Context<Self>) {
        let directory = self.workspace.clone();
        cx.spawn_in(window, async move |view, cx| {
            let mut picker = rfd::AsyncFileDialog::new().set_title(match asset {
                Asset::Document => "Open a document from this project",
            });
            if let Some(directory) = directory {
                picker = picker.set_directory(directory);
            }
            if let Some(file) = picker.pick_file().await {
                let path = file.path().to_path_buf();
                let _ = view.update_in(cx, |view, window, cx| match asset {
                    Asset::Document => {
                        if path
                            .extension()
                            .and_then(|s| s.to_str())
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
                        {
                            view.capability(
                                "document_pdf_preview",
                                json!({"path":path}),
                                "Opening document",
                                None,
                                cx,
                            );
                        } else {
                            view.document_open(path, window, cx);
                        }
                    }
                });
            }
        })
        .detach();
    }
    pub(super) fn local_models_card(&self, cx: &mut Context<Self>) -> AnyElement {
        card().max_w(px(820.))
            .child(div().font_weight(FontWeight::MEDIUM).child("Local models"))
            .child(subtitle("Download a model to use on this computer, or connect an existing local provider above."))
            .child(div().flex().gap_2()
                .child(button("browse-local-models","Browse local models").on_click(cx.listener(|view, _, _, cx| view.open_assets(1,cx))))
                .child(primary("start-local-model","Start selected model").on_click(cx.listener(|view, _, _, cx| {
                    view.capability("local_inference_start",json!({}),"Starting local model",None,cx);
                })))
                .child(button("stop-local-model","Stop model").on_click(cx.listener(|view, _, _, cx| {
                    view.capability("local_inference_stop",json!({}),"Stopping local model",None,cx);
                }))))
            .into_any_element()
    }
    pub(super) fn native_tools_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let browser = card()
            .child(div().font_weight(FontWeight::MEDIUM).child("Browser"))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.native.url).aria_label("Page URL")),
                    )
                    .child(primary("open-web-page", "Open page").on_click(cx.listener(
                        |this, _, _, cx| {
                            let url = this.native.url.read(cx).value().to_string();
                            this.capability(
                                "browser_navigate",
                                json!({"url":url}),
                                "Opening page",
                                None,
                                cx,
                            );
                        },
                    ))),
            );
        let voice = self.speech_card(cx);
        let screen = card()
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .child("Screen capture"),
            )
            .child(subtitle(
                "Your computer may ask for permission to record the screen.",
            ))
            .child(
                button("capture-screen", "Capture screen").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.capability(
                            "desktop_screenshot",
                            json!({}),
                            "Capturing screen",
                            None,
                            cx,
                        )
                    },
                )),
            );
        div()
            .flex()
            .flex_col()
            .gap_4()
            .max_w(px(820.))
            .child(browser)
            .child(voice)
            .child(screen)
            .into_any_element()
    }
    pub(super) fn native_result_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut result = div().flex().flex_col().gap_3().max_w(px(820.));
        if !self.native.message.is_empty() {
            result = result.child(subtitle(self.native.message.clone()));
        }
        for (i, (id, (label, _))) in self.native.pending.iter().enumerate() {
            let id = *id;
            result = result.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(subtitle(format!("{label}…")))
                    .child(button(("cancel-native", i), "Cancel").on_click(cx.listener(
                        move |this, _, _, cx| this.host_call(cx, |h| h.cancel_capability(id)),
                    ))),
            );
        }
        if !self.native.text.is_empty() {
            result = result.child(
                TextView::markdown("native-result-text", truncate(&self.native.text, 16000))
                    .selectable(true),
            );
        }
        if let Some(path) = &self.native.preview {
            result = result.child(
                img(path.clone())
                    .max_w(px(800.))
                    .max_h(px(480.))
                    .object_fit(ObjectFit::Contain),
            );
        }
        if let Some(path) = &self.native.output {
            let path = path.clone();
            result = result.child(
                button("open-output", "Open file")
                    .on_click(move |_, _, cx| cx.open_with_system(&path)),
            );
        }
        result.into_any_element()
    }
}
fn document_text(summary: &Value) -> String {
    if let Some(text) = summary.get("text").and_then(Value::as_str) {
        return text.to_owned();
    }
    if let Some(rows) = summary.get("rows").and_then(Value::as_array) {
        return rows
            .iter()
            .filter_map(Value::as_array)
            .map(|cells| {
                cells
                    .iter()
                    .map(|cell| {
                        cell.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| cell.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(" | ")
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    if let Some(pages) = summary.get("pages").and_then(Value::as_array) {
        return pages
            .iter()
            .filter_map(|page| page.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n");
    }
    "Your document is ready.".into()
}
