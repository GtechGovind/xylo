//! Explicit image generation through the selected image API. Configuration holds
//! credential references; prompts and provider responses stay out of settings.
use super::*;
use gpui_kit::base::Disableable;
use gpui_kit::{
    TestSupportExt,
    component::{
        button::Button as MenuButton,
        menu::{DropdownMenu, PopupMenuItem},
    },
};

/// ChatGPT plan credentials and unrelated provider protocols cannot authorize
/// generic image endpoints. Reuse the connection, never its chat model.
fn compatible_image_connection(provider: &ProviderConfig) -> Option<ImageGenerationProvider> {
    if provider.endpoint.is_empty()
        || provider
            .credential_ref
            .as_deref()
            .is_some_and(|reference| reference.starts_with("account:"))
    {
        return None;
    }
    let protocol = match provider.family {
        ApiFamily::OpenAiChat | ApiFamily::OpenAiResponses => ImageGenerationProtocol::OpenAiImages,
        ApiFamily::Gemini => ImageGenerationProtocol::GeminiGenerateContent,
        _ => return None,
    };
    Some(ImageGenerationProvider {
        name: provider.name.clone(),
        endpoint: provider.endpoint.clone(),
        model: String::new(),
        protocol,
        credential_ref: provider.credential_ref.clone(),
        headers: provider.headers.clone(),
        timeout_seconds: 180,
    })
}

fn detach_image_credential(provider: &mut ImageGenerationProvider) {
    provider.credential_ref = Some(format!("image-provider:{}", Uuid::new_v4()));
}

pub(super) struct ImageGenerationUi {
    pub(super) name: Entity<InputState>,
    pub(super) endpoint: Entity<InputState>,
    pub(super) model: Entity<InputState>,
    pub(super) prompt: Entity<TextareaState>,
    secret: Entity<InputState>,
    connection_source: Option<(ImageGenerationProtocol, String)>,
    load_error: bool,
    save_error: bool,
    provider_latest: Option<Uuid>,
    protocol: ImageGenerationProtocol,
    size: String,
    credential_ref: Option<String>,
    headers: BTreeMap<String, String>,
    timeout_seconds: u64,
    loaded: bool,
    history_request: Option<Uuid>,
    history: Vec<ImageGenerationReceipt>,
    provider_requests: BTreeMap<Uuid, (ImageGenerationProvider, bool)>,
    pending: BTreeMap<Uuid, (Uuid, Uuid)>,
    active: Option<(Uuid, Uuid)>,
    latest: Option<(Uuid, Uuid)>,
    receipt: Option<ImageGenerationReceipt>,
    needs_reconciliation: bool,
    pub(super) notice: String,
}
impl ImageGenerationUi {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Xylo>) -> Self {
        let mut input = |placeholder: &str, cx: &mut Context<Xylo>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.to_owned()))
        };
        Self {
            name: input("Image provider name", cx),
            endpoint: input("OpenAI-compatible or native Gemini API endpoint", cx),
            model: input("Choose an image-generation model explicitly", cx),
            secret: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("API key")
                    .masked(true)
            }),
            connection_source: None,
            load_error: false,
            save_error: false,
            provider_latest: None,
            prompt: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder("Describe one image to generate")
                    .rows(4)
            }),
            protocol: ImageGenerationProtocol::OpenAiImages,
            size: "1024x1024".into(),
            credential_ref: None,
            headers: BTreeMap::new(),
            timeout_seconds: 180,
            loaded: false,
            history_request: None,
            history: Vec::new(),
            provider_requests: BTreeMap::new(),
            pending: BTreeMap::new(),
            active: None,
            latest: None,
            receipt: None,
            needs_reconciliation: false,
            notice: String::new(),
        }
    }
    pub(super) fn busy(&self) -> bool {
        self.active.is_some()
    }
    fn provider(&self, cx: &App) -> ImageGenerationProvider {
        let same_connection =
            self.connection_source
                .as_ref()
                .is_some_and(|(protocol, endpoint)| {
                    *protocol == self.protocol && endpoint == self.endpoint.read(cx).value().trim()
                });
        ImageGenerationProvider {
            name: self.name.read(cx).value().trim().to_owned(),
            endpoint: self.endpoint.read(cx).value().trim().to_owned(),
            model: self.model.read(cx).value().trim().to_owned(),
            protocol: self.protocol,
            credential_ref: if same_connection {
                self.credential_ref.clone()
            } else {
                None
            },
            headers: if same_connection {
                self.headers.clone()
            } else {
                Default::default()
            },
            timeout_seconds: self.timeout_seconds,
        }
    }
    fn apply_provider(
        &mut self,
        provider: ImageGenerationProvider,
        window: &mut Window,
        cx: &mut Context<Xylo>,
    ) {
        self.connection_source = Some((provider.protocol, provider.endpoint.clone()));
        self.name
            .update(cx, |input, cx| input.set_value(provider.name, window, cx));
        self.endpoint.update(cx, |input, cx| {
            input.set_value(provider.endpoint, window, cx)
        });
        self.model
            .update(cx, |input, cx| input.set_value(provider.model, window, cx));
        self.protocol = provider.protocol;
        self.credential_ref = provider.credential_ref;
        self.headers = provider.headers;
        self.timeout_seconds = provider.timeout_seconds;
    }
}
impl Xylo {
    pub(super) fn load_image_generation_provider(&mut self, cx: &mut Context<Self>) {
        if self.image_generation.loaded {
            return;
        }
        self.image_generation.load_error = false;
        let id = Uuid::new_v4();
        let snapshot = self.image_generation.provider(cx);
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.image_generation_provider(id));
        if result.is_ok() {
            self.image_generation.loaded = true;
            self.image_generation.provider_latest = Some(id);
            self.image_generation
                .provider_requests
                .insert(id, (snapshot, false));
        }
        if let Err(error) = result {
            self.image_generation.load_error = true;
            self.image_generation.notice = error.to_string();
        }
        cx.notify();
    }
    fn refresh_image_generation_history(&mut self, cx: &mut Context<Self>) {
        if self.image_generation.history_request.is_some() {
            return;
        }
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.image_generation_history(id));
        if result.is_ok() {
            self.image_generation.history_request = Some(id);
        }
        self.result(result, cx);
    }
    pub(super) fn image_generation_history_received(
        &mut self,
        id: Uuid,
        result: Result<Vec<ImageGenerationReceipt>, String>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.image_generation.history_request != Some(id) {
            return false;
        }
        self.image_generation.history_request = None;
        match result {
            Ok(history) if history.len() <= 16 => {
                if self.image_generation.latest.is_none()
                    && !self.image_generation.busy()
                    && let Some(receipt) = history.first()
                {
                    self.image_generation.latest = Some((receipt.id, receipt.session_id));
                    self.image_generation.needs_reconciliation = matches!(
                        receipt.state,
                        ImageGenerationState::Prepared
                            | ImageGenerationState::Submitted
                            | ImageGenerationState::OutcomeUnknown
                    );
                    self.image_generation.notice = receipt.message.clone();
                    self.remember_generated_artifacts(receipt);
                    self.image_generation.receipt = Some(receipt.clone());
                }
                self.image_generation.history = history;
            }
            Ok(_) => {
                self.image_generation.notice =
                    "Image request history exceeded its display limit.".into()
            }
            Err(error) => self.image_generation.notice = error,
        }
        cx.notify();
        true
    }
    fn save_image_generation_provider(&mut self, cx: &mut Context<Self>) {
        if self
            .image_generation
            .provider_requests
            .values()
            .any(|(_, saving)| *saving)
        {
            return;
        }
        let id = Uuid::new_v4();
        let mut provider = self.image_generation.provider(cx);
        let secret = self
            .image_generation
            .secret
            .read(cx)
            .value()
            .trim()
            .to_owned();
        if !secret.is_empty() {
            // A new image key cannot replace shared chat credentials or keys
            // referenced by an already admitted image-tool turn.
            detach_image_credential(&mut provider);
        }
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| {
                host.save_image_generation_connection(
                    id,
                    provider.clone(),
                    (!secret.is_empty()).then_some(secret),
                )
            });
        self.image_generation.save_error = false;
        if result.is_ok() {
            self.image_generation.provider_latest = Some(id);
            self.image_generation
                .provider_requests
                .insert(id, (provider, true));
            self.image_generation.notice = "Saving image provider…".into();
        } else if let Err(error) = result {
            self.image_generation.save_error = true;
            self.image_generation.notice = error.to_string();
        }
        cx.notify();
    }
    pub(super) fn image_generation_provider_received(
        &mut self,
        id: Uuid,
        result: Result<Option<ImageGenerationProvider>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((snapshot, saving)) = self.image_generation.provider_requests.remove(&id) else {
            return false;
        };
        if self.image_generation.provider_latest != Some(id) {
            cx.notify();
            return true;
        }
        self.image_generation.provider_latest = None;
        match result {
            Ok(provider) => {
                if saving || self.image_generation.provider(cx) == snapshot {
                    if let Some(provider) = provider {
                        self.image_generation.apply_provider(provider, window, cx);
                    }
                }
                if saving {
                    self.image_generation
                        .secret
                        .update(cx, |input, cx| input.set_value("", window, cx));
                }
                self.image_generation.load_error = false;
                self.image_generation.save_error = false;
                self.image_generation.notice = if saving {
                    "Saved image provider. New image requests in chat use this connection.".into()
                } else {
                    String::new()
                };
            }
            Err(error) => {
                self.image_generation.load_error = !saving;
                self.image_generation.save_error = saving;
                self.image_generation.notice = error;
            }
        }
        cx.notify();
        true
    }
    fn start_image_generation(&mut self, cx: &mut Context<Self>) {
        if self.image_generation.busy() || self.image_generation.needs_reconciliation {
            return;
        }
        let id = Uuid::new_v4();
        let provider = self.image_generation.provider(cx);
        let prompt = self.image_generation.prompt.read(cx).value().to_string();
        if provider.name.trim().is_empty()
            || provider.endpoint.trim().is_empty()
            || provider.model.trim().is_empty()
            || prompt.trim().is_empty()
        {
            self.image_generation.notice =
                "Choose an image provider, endpoint, model, and prompt before generating.".into();
            cx.notify();
            return;
        }
        let request = ImageGenerationRequest {
            id,
            session_id: self.session,
            provider,
            prompt,
            size: self.image_generation.size.clone(),
        };
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.generate_image(id, request));
        if result.is_ok() {
            self.image_generation.active = Some((id, self.session));
            self.image_generation.latest = Some((id, self.session));
            self.image_generation.pending.insert(id, (id, self.session));
            self.image_generation.receipt = None;
            self.image_generation.needs_reconciliation = true;
            self.image_generation.notice = "Generating with your selected provider…".into();
        }
        self.result(result, cx);
        cx.notify();
    }
    fn cancel_image_generation(&mut self, cx: &mut Context<Self>) {
        if let Some((id, _)) = self.image_generation.active {
            if let Some(host) = &self.host {
                let result = host.cancel_capability(id);
                self.result(result, cx);
            }
            self.image_generation.notice="Cancellation requested. A submitted image request may still complete or be charged by the provider.".into();
        }
        cx.notify();
    }
    fn refresh_image_generation(&mut self, cx: &mut Context<Self>) {
        if self.image_generation.pending.len() >= 8 {
            return;
        }
        let target = self.image_generation.latest;
        if let Some((job, session)) = target {
            let id = Uuid::new_v4();
            let result = self
                .host
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
                .and_then(|host| host.image_generation_receipt(id, job));
            if result.is_ok() {
                self.image_generation.pending.insert(id, (job, session));
            }
            self.result(result, cx);
        }
    }
    pub(super) fn image_generation_received(
        &mut self,
        id: Uuid,
        result: Result<ImageGenerationReceipt, String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((job, session)) = self.image_generation.pending.remove(&id) else {
            return false;
        };
        let current = self.image_generation.latest.map(|latest| latest.0);
        if current != Some(job) {
            return true;
        }
        match result {
            Ok(receipt) if receipt.id == job && receipt.session_id == session => {
                if let Some(previous) = &self.image_generation.receipt {
                    let previous_terminal = !matches!(
                        previous.state,
                        ImageGenerationState::Prepared | ImageGenerationState::Submitted
                    );
                    if receipt.updated_at < previous.updated_at
                        || (previous_terminal
                            && matches!(
                                receipt.state,
                                ImageGenerationState::Prepared | ImageGenerationState::Submitted
                            ))
                    {
                        return true;
                    }
                }
                let terminal = !matches!(
                    receipt.state,
                    ImageGenerationState::Prepared | ImageGenerationState::Submitted
                );
                if terminal {
                    self.image_generation.active = None;
                }
                self.image_generation.needs_reconciliation =
                    receipt.state == ImageGenerationState::OutcomeUnknown;
                self.image_generation.notice = receipt.message.clone();
                self.image_generation
                    .history
                    .retain(|previous| previous.id != receipt.id);
                self.image_generation.history.insert(0, receipt.clone());
                self.image_generation.history.truncate(16);
                self.remember_generated_artifacts(&receipt);
                self.image_generation.receipt = Some(receipt);
            }
            Ok(_) => {
                self.image_generation.active = None;
                self.image_generation.needs_reconciliation = true;
                self.image_generation.notice =
                    "Generation response did not match this request.".into()
            }
            Err(error) => {
                self.image_generation.active = None;
                self.image_generation.needs_reconciliation = true;
                // The durable receipt is the authority after transport/storage errors.
                self.image_generation.notice =
                    format!("{error}. Check this request's saved status before generating again.");
            }
        }
        cx.notify();
        true
    }
    fn remember_generated_artifacts(&mut self, receipt: &ImageGenerationReceipt) {
        if receipt.session_id == self.session && receipt.state == ImageGenerationState::Succeeded {
            for image in &receipt.images {
                self.record_artifact(image.artifact_path.clone());
            }
        }
    }
    pub(super) fn image_provider_settings_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.settings_open = true;
        if !self.image_generation.loaded && !self.image_generation.load_error {
            self.load_image_generation_provider(cx);
        }
        let saving = self
            .image_generation
            .provider_requests
            .values()
            .any(|(_, saving)| *saving);
        let loading = self
            .image_generation
            .provider_requests
            .values()
            .any(|(_, saving)| !*saving);
        let disabled = saving || loading;
        let mut content = div()
            .w_full()
            .max_w(px(760.))
            .flex()
            .flex_col()
            .gap_4()
            .child(
                button("image-provider-back", "← Providers").on_click(cx.listener(
                    |view, _, window, cx| {
                        view.image_generation
                            .secret
                            .update(cx, |input, cx| input.set_value("", window, cx));
                        view.image_generation.loaded = false;
                        view.image_generation.load_error = false;
                        view.navigate(Page::Providers, cx);
                    },
                )),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Image generation"),
            )
            .child(subtitle(
                "Configure the connection used by image tools in chat.",
            ));
        let mut seen = BTreeSet::new();
        let connections: Vec<_> = self
            .settings
            .providers
            .iter()
            .chain(std::iter::once(&self.settings.provider))
            .filter_map(compatible_image_connection)
            .filter(|provider| {
                seen.insert((provider.endpoint.clone(), provider.credential_ref.clone()))
            })
            .collect();
        if !connections.is_empty() {
            let weak = cx.entity().downgrade();
            content = content.child(
                MenuButton::new("image-saved-connection")
                    .label("Use saved connection")
                    .disabled(disabled)
                    .dropdown_menu(move |mut menu, _, _| {
                        for provider in &connections {
                            let provider = provider.clone();
                            let view = weak.clone();
                            menu = menu.item(PopupMenuItem::new(provider.name.clone()).on_click(
                                move |_, window, cx| {
                                    let _ = view.update(cx, |view, cx| {
                                        view.image_generation.secret.update(cx, |input, cx| {
                                            input.set_value("", window, cx)
                                        });
                                        view.image_generation.apply_provider(
                                            provider.clone(),
                                            window,
                                            cx,
                                        );
                                        view.image_generation.notice.clear();
                                        cx.notify();
                                    });
                                },
                            ));
                        }
                        menu
                    }),
            );
        }
        let mut form = div()
            .border_1()
            .border_color(rgb(border()))
            .rounded_lg()
            .overflow_hidden();
        for (label, input) in [
            ("Name", &self.image_generation.name),
            ("Endpoint", &self.image_generation.endpoint),
            ("Model", &self.image_generation.model),
            ("API key", &self.image_generation.secret),
        ] {
            form = form.child(
                div()
                    .p_4()
                    .flex()
                    .items_center()
                    .gap_4()
                    .border_b_1()
                    .border_color(rgb(border()))
                    .child(div().w(px(110.)).flex_shrink_0().text_sm().child(label))
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(input)
                                .disabled(disabled)
                                .aria_label(format!("Image provider {label}")),
                        ),
                    ),
            );
        }
        let mut protocols = div().flex().gap_2();
        for (id, label, protocol) in [
            (
                "image-api-openai",
                "OpenAI Images",
                ImageGenerationProtocol::OpenAiImages,
            ),
            (
                "image-api-gemini",
                "Gemini",
                ImageGenerationProtocol::GeminiGenerateContent,
            ),
        ] {
            protocols = protocols.child(
                button(id, label)
                    .bg(rgb(if self.image_generation.protocol == protocol {
                        selected()
                    } else {
                        panel()
                    }))
                    .disabled(disabled)
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.image_generation.protocol = protocol;
                        cx.notify();
                    })),
            );
        }
        form = form.child(
            div()
                .p_4()
                .flex()
                .items_center()
                .gap_4()
                .child(div().w(px(110.)).text_sm().child("API"))
                .child(protocols),
        );
        content = content.child(form).child(subtitle("Keys stay in the OS credential store. Leave blank to keep a saved key; local servers can use no key."));
        let status = if saving {
            "Saving image provider…"
        } else if loading {
            "Loading connection…"
        } else {
            &self.image_generation.notice
        };
        if !status.is_empty() {
            content = content.child(
                div()
                    .id("image-provider-status")
                    .role(Role::Status)
                    .aria_label(status.to_owned())
                    .test_support()
                    .text_sm()
                    .text_color(rgb(
                        if self.image_generation.load_error || self.image_generation.save_error {
                            danger()
                        } else {
                            muted()
                        },
                    ))
                    .child(status.to_owned()),
            );
        }
        if self.image_generation.load_error {
            content = content.child(
                button("image-provider-retry-load", "Retry loading").on_click(cx.listener(
                    |view, _, _, cx| {
                        view.image_generation.loaded = false;
                        view.image_generation.load_error = false;
                        view.load_image_generation_provider(cx);
                        cx.notify();
                    },
                )),
            );
        }
        content = content.child(
            div().flex().justify_end().child(
                primary(
                    "image-save-provider",
                    if saving { "Saving…" } else { "Save" },
                )
                .disabled(disabled)
                .on_click(cx.listener(|view, _, _, cx| view.save_image_generation_provider(cx))),
            ),
        );
        div()
            .id("image-provider-settings")
            .test_support()
            .size_full()
            .overflow_y_scroll()
            .p_8()
            .flex()
            .justify_center()
            .child(content)
            .into_any_element()
    }
}

#[cfg(test)]
#[allow(dead_code)] // Exercised by the separate native validation test target.
impl ImageGenerationUi {
    pub(super) fn validation_provider_pending(&self) -> bool {
        !self.provider_requests.is_empty()
    }
    pub(super) fn validation_prepare(&mut self, id: Uuid, session: Uuid) {
        self.latest = Some((id, session));
        self.active = Some((id, session));
        self.pending.insert(id, (id, session));
        self.needs_reconciliation = true;
    }
    pub(super) fn validation_receipt(&self) -> Option<&ImageGenerationReceipt> {
        self.receipt.as_ref()
    }
    pub(super) fn validation_config_read(&mut self, id: Uuid, cx: &App) {
        self.provider_latest = Some(id);
        self.provider_requests
            .insert(id, (self.provider(cx), false));
    }
    pub(super) fn validation_needs_reconciliation(&self) -> bool {
        self.needs_reconciliation
    }
}

/// Tool results carry managed references only. Preview paths are requested from
/// the host after a user action, never read from a tool-provided artifact path.
pub(super) fn generated_tool_attachments(
    tool: &crate::state::ToolCard,
    session: Uuid,
) -> Vec<ImageAttachment> {
    if tool.name != "image_generate" || tool.success != Some(true) {
        return Vec::new();
    }
    let Some(output) = tool.output() else {
        return Vec::new();
    };
    output
        .get("summary")
        .and_then(|summary| summary.get("images"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_ATTACHMENTS_PER_TURN)
        .filter_map(|value| serde_json::from_value::<ImageAttachment>(value.clone()).ok())
        .filter(|image| image.session_id == session && image.source == AttachmentSource::Generated)
        .collect()
}

impl Xylo {
    pub(super) fn generated_tool_image_card(
        &self,
        tool: &crate::state::ToolCard,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let images = generated_tool_attachments(tool, self.session);
        if images.is_empty() {
            return div().into_any_element();
        }
        let output = ChatMessage {
            id: tool.id,
            session_id: self.session,
            role: "assistant".into(),
            content: String::new(),
            created_at: tool.finished_at.unwrap_or(tool.started_at),
            attachments: images,
        };
        div()
            .max_w(px(440.))
            .pl_2()
            .child(self.message_images(&output, cx))
            .into_any_element()
    }
}

#[cfg(test)]
mod chat_result_tests {
    use super::generated_tool_attachments;
    use uuid::Uuid;
    use xylo_core::{AttachmentSource, ImageAttachment, MAX_ATTACHMENTS_PER_TURN};
    fn attachment(session: Uuid) -> ImageAttachment {
        ImageAttachment {
            id: Uuid::new_v4(),
            session_id: session,
            name: "generated.png".into(),
            media_type: "image/png".into(),
            sha256: "a".repeat(64),
            bytes: 100,
            width: 16,
            height: 16,
            source: AttachmentSource::Generated,
            imported_at: 0,
        }
    }
    fn tool(images: Vec<ImageAttachment>) -> crate::state::ToolCard {
        let mut tool = crate::state::ToolCard { presentation:Default::default(), id:Uuid::new_v4(), name:"image_generate".into(), input:serde_json::json!({}), result:Some(serde_json::json!({"summary":{"images":images},"artifacts":["/untrusted/ignored.png"]}).to_string()), approval:false, rule:None, success:Some(true), started_at:0, finished_at:Some(1) };
        tool.refresh_presentation();
        tool
    }
    #[test]
    fn generated_chat_images_are_bounded_and_session_scoped() {
        let session = Uuid::new_v4();
        let images = (0..10).map(|_| attachment(session)).collect();
        assert_eq!(
            generated_tool_attachments(&tool(images), session).len(),
            MAX_ATTACHMENTS_PER_TURN
        );
        assert!(
            generated_tool_attachments(&tool(vec![attachment(Uuid::new_v4())]), session).is_empty()
        );
        let mut invalid = tool(vec![attachment(session)]);
        invalid.success = Some(false);
        assert!(generated_tool_attachments(&invalid, session).is_empty());
    }
    #[test]
    fn generated_chat_images_never_use_artifact_paths_as_previews() {
        let session = Uuid::new_v4();
        let mut invalid = tool(vec![attachment(session)]);
        invalid.result=Some(serde_json::json!({"summary":{"images":[{"path":"/untrusted/ignored.png"}]},"artifacts":["/untrusted/ignored.png"]}).to_string());
        invalid.refresh_presentation();
        assert!(generated_tool_attachments(&invalid, session).is_empty());
        invalid.name = "other_tool".into();
        invalid.result = tool(vec![attachment(session)]).result;
        invalid.refresh_presentation();
        assert!(generated_tool_attachments(&invalid, session).is_empty());
    }
}

#[cfg(test)]
mod image_connection_tests {
    use super::{ApiFamily, CHATGPT_REF, ProviderConfig, compatible_image_connection, detach_image_credential};
    #[test]
    fn new_image_key_detaches_from_saved_chat_credentials_and_existing_turn_references() {
        let chat = ProviderConfig {
            endpoint: "https://api.example/v1".into(),
            credential_ref: Some("provider:shared-chat".into()),
            ..Default::default()
        };
        let mut image = compatible_image_connection(&chat).unwrap();
        let previous = image.credential_ref.clone();
        detach_image_credential(&mut image);
        assert_eq!(chat.credential_ref, previous);
        assert_ne!(image.credential_ref, previous);
        assert!(
            image
                .credential_ref
                .as_deref()
                .unwrap()
                .starts_with("image-provider:")
        );
        let admitted_turn_reference = image.credential_ref.clone();
        detach_image_credential(&mut image);
        assert_ne!(image.credential_ref, admitted_turn_reference);
    }
    #[test]
    fn saved_image_connections_reuse_only_compatible_api_credentials_and_never_a_chat_model() {
        for family in [
            ApiFamily::OpenAiChat,
            ApiFamily::OpenAiResponses,
            ApiFamily::Gemini,
        ] {
            let provider = ProviderConfig {
                name: "API fixture".into(),
                family,
                endpoint: "https://api.example/v1".into(),
                model: "a-chat-model".into(),
                credential_ref: Some("provider:fixture".into()),
                ..Default::default()
            };
            let image = compatible_image_connection(&provider).unwrap();
            assert!(image.model.is_empty());
            assert_eq!(image.credential_ref, provider.credential_ref);
            assert_eq!(image.endpoint, provider.endpoint);
            for account in [CHATGPT_REF, "account:another-plan"] {
                let mut account_provider = provider.clone();
                account_provider.credential_ref = Some(account.into());
                assert!(compatible_image_connection(&account_provider).is_none());
            }
        }
        for family in [
            ApiFamily::Anthropic,
            ApiFamily::Azure,
            ApiFamily::Bedrock,
            ApiFamily::Vertex,
        ] {
            let provider = ProviderConfig {
                family,
                endpoint: "https://api.example/v1".into(),
                ..Default::default()
            };
            assert!(compatible_image_connection(&provider).is_none());
        }
    }
}
