//! Explicit image generation through the selected image API. Configuration holds
//! credential references; prompts and provider responses stay out of settings.
use super::*;

pub(super) struct ImageGenerationUi {
    pub(super) name: Entity<InputState>,
    pub(super) endpoint: Entity<InputState>,
    pub(super) model: Entity<InputState>,
    pub(super) prompt: Entity<TextareaState>,
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
    pub(super) fn new(window: &mut Window, cx: &mut Context<Workbench>) -> Self {
        let mut input = |placeholder: &str, cx: &mut Context<Workbench>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.to_owned()))
        };
        Self {
            name: input("Image provider name", cx),
            endpoint: input("OpenAI-compatible or native Gemini API endpoint", cx),
            model: input("Choose an image-generation model explicitly", cx),
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
        ImageGenerationProvider {
            name: self.name.read(cx).value().trim().to_owned(),
            endpoint: self.endpoint.read(cx).value().trim().to_owned(),
            model: self.model.read(cx).value().trim().to_owned(),
            protocol: self.protocol,
            credential_ref: self.credential_ref.clone(),
            headers: self.headers.clone(),
            timeout_seconds: self.timeout_seconds,
        }
    }
    fn apply_provider(
        &mut self,
        provider: ImageGenerationProvider,
        window: &mut Window,
        cx: &mut Context<Workbench>,
    ) {
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
impl Workbench {
    pub(super) fn load_image_generation_provider(&mut self, cx: &mut Context<Self>) {
        if self.image_generation.loaded {
            return;
        }
        self.refresh_image_generation_history(cx);
        let id = Uuid::new_v4();
        let snapshot = self.image_generation.provider(cx);
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.image_generation_provider(id));
        if result.is_ok() {
            self.image_generation.loaded = true;
            self.image_generation
                .provider_requests
                .insert(id, (snapshot, false));
        }
        self.result(result, cx);
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
        if self.image_generation.provider_requests.len() >= 8 {
            return;
        }
        let id = Uuid::new_v4();
        let provider = self.image_generation.provider(cx);
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.save_image_generation_provider(id, provider.clone()));
        if result.is_ok() {
            self.image_generation
                .provider_requests
                .insert(id, (provider, true));
            self.image_generation.notice = "Saving image provider…".into();
        }
        self.result(result, cx);
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
        match result {
            Ok(provider) => {
                if !saving
                    && self.image_generation.provider(cx) == snapshot
                    && let Some(provider) = provider
                {
                    self.image_generation.apply_provider(provider, window, cx);
                }
                self.image_generation.notice = if saving {
                    "Saved image provider. Agent image generation uses this connection and model."
                } else {
                    "Choose a model and image size, then generate when ready."
                }
                .into();
            }
            Err(error) => self.image_generation.notice = error,
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
    pub(super) fn image_generation_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.load_image_generation_provider(cx);
        let busy = self.image_generation.busy();
        let mut page = div()
            .id("image-generation-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(heading(
                "Images",
                "Generate one image with an explicitly chosen provider and model.",
            ));
        let mut form = card().max_w(px(900.)).child(subtitle("IMAGE CONNECTION"));
        let mut connections = div().flex().flex_wrap().gap_2();
        for (index, provider) in self.settings.providers.iter().enumerate() {
            if !matches!(
                provider.family,
                ApiFamily::OpenAiChat
                    | ApiFamily::OpenAiResponses
                    | ApiFamily::Gemini
                    | ApiFamily::Azure
            ) {
                continue;
            }
            let provider = provider.clone();
            let plan_credential = provider.credential_ref.as_deref() == Some(CHATGPT_REF);
            connections = connections.child(
                button(("image-use-connection", index), format!("Use {} connection", provider.name))
                    .disabled(busy || plan_credential)
                    .on_click(cx.listener(move |view, _, window, cx| {
                        let protocol = if provider.family == ApiFamily::Gemini {
                            ImageGenerationProtocol::GeminiGenerateContent
                        } else {
                            ImageGenerationProtocol::OpenAiImages
                        };
                        view.image_generation.apply_provider(
                            ImageGenerationProvider {
                                name: provider.name.clone(), endpoint: provider.endpoint.clone(),
                                model: String::new(), protocol,
                                credential_ref: provider.credential_ref.clone(),
                                headers: provider.headers.clone(), timeout_seconds: 180,
                            }, window, cx,
                        );
                        view.image_generation.notice = "Connection selected. Enter an image-generation model; the chat model was not selected automatically.".into();
                        cx.notify();
                    })),
            );
        }
        form = form.child(connections).child(subtitle(
            "Saved credentials stay in the OS credential store. Local compatible servers can run without a key. This Images API uses API credentials; ChatGPT plan image generation through Responses is a separate operation."
        ));
        for (label, input) in [
            ("Image provider name", &self.image_generation.name),
            ("Image API endpoint", &self.image_generation.endpoint),
            ("Image-generation model", &self.image_generation.model),
        ] {
            form = form.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(subtitle(label))
                    .child(Input::new(input).aria_label(label)),
            );
        }
        let mut protocols = div().flex().gap_2();
        for (id, label, protocol) in [
            (
                "image-api-openai",
                "OpenAI-compatible Images",
                ImageGenerationProtocol::OpenAiImages,
            ),
            (
                "image-api-gemini",
                "Gemini native",
                ImageGenerationProtocol::GeminiGenerateContent,
            ),
        ] {
            let selected = self.image_generation.protocol == protocol;
            protocols = protocols.child(
                button(
                    id,
                    if selected {
                        format!("✓ {label}")
                    } else {
                        label.into()
                    },
                )
                .disabled(busy)
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.image_generation.protocol = protocol;
                    cx.notify();
                })),
            );
        }
        form = form.child(protocols).child(
            button("image-save-provider", "Save image provider for agents")
                .disabled(busy)
                .on_click(cx.listener(|view, _, _, cx| view.save_image_generation_provider(cx))),
        );
        let mut sizes = div().flex().flex_wrap().gap_2();
        for (index, size) in [
            "256x256",
            "512x512",
            "1024x1024",
            "1024x1536",
            "1536x1024",
            "auto",
        ]
        .into_iter()
        .enumerate()
        {
            let selected = self.image_generation.size == size;
            let unsupported = self.image_generation.protocol
                == ImageGenerationProtocol::GeminiGenerateContent
                && matches!(size, "256x256" | "512x512");
            sizes = sizes.child(
                button(
                    ("image-size", index),
                    if selected {
                        format!("✓ {size}")
                    } else {
                        size.into()
                    },
                )
                .disabled(busy || unsupported)
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.image_generation.size = size.into();
                    cx.notify();
                })),
            );
        }
        page = page.child(form).child(
            card()
                .max_w(px(900.))
                .child(subtitle("PROMPT AND OUTPUT SIZE"))
                .child(Textarea::new(&self.image_generation.prompt))
                .child(sizes)
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            button("generate-image", "Generate image")
                                .disabled(busy || self.image_generation.needs_reconciliation)
                                .on_click(
                                    cx.listener(|view, _, _, cx| view.start_image_generation(cx)),
                                ),
                        )
                        .child(
                            button("cancel-image-generation", "Cancel request")
                                .disabled(!busy)
                                .on_click(
                                    cx.listener(|view, _, _, cx| view.cancel_image_generation(cx)),
                                ),
                        )
                        .child(
                            button("refresh-image-generation", "Check saved status")
                                .disabled(self.image_generation.latest.is_none())
                                .on_click(
                                    cx.listener(|view, _, _, cx| view.refresh_image_generation(cx)),
                                ),
                        ),
                )
                .child(subtitle(self.image_generation.notice.clone())),
        );
        if self.image_generation.needs_reconciliation
            && !busy
            && self
                .image_generation
                .receipt
                .as_ref()
                .is_none_or(|receipt| receipt.state == ImageGenerationState::OutcomeUnknown)
        {
            page = page.child(
                button(
                    "ack-image-uncertainty",
                    "I checked the provider; allow a new request",
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    view.image_generation.needs_reconciliation = false;
                    view.image_generation.notice =
                        "A new request will have a new ID and may incur a new charge.".into();
                    cx.notify();
                })),
            );
        }
        if let Some(receipt) = &self.image_generation.receipt {
            let mut output = card().max_w(px(900.)).child(subtitle(format!(
                "{} · {} · {:?}",
                receipt.provider_name, receipt.model, receipt.state
            )));
            for image in &receipt.images {
                let attachment = image.attachment.clone();
                output=output.child(img(image.preview_path.clone()).w(px(256.)).h(px(256.)).object_fit(ObjectFit::Contain))
                    .child(subtitle(image.artifact_path.to_string_lossy().into_owned()))
                    .child(button("attach-generated-image","Attach to this message").disabled(attachment.session_id!=self.session||self.draft_attachments().len()>=MAX_ATTACHMENTS_PER_TURN)
                        .on_click(cx.listener(move|view,_,_,cx|{if attachment.session_id==view.session && view.add_draft_attachment(attachment.clone(),cx){view.attachment_previews(std::slice::from_ref(&attachment),cx);view.image_generation.notice="Generated image attached to your draft. Review it before sending.".into();}cx.notify();})));
            }
            page = page.child(output);
        }
        let mut history = card()
            .max_w(px(900.))
            .child(subtitle("RECENT IMAGE REQUESTS"))
            .child(
                button("refresh-image-history", "Refresh recent requests")
                    .disabled(self.image_generation.history_request.is_some())
                    .on_click(
                        cx.listener(|view, _, _, cx| view.refresh_image_generation_history(cx)),
                    ),
            );
        for (index, receipt) in self.image_generation.history.iter().enumerate() {
            let receipt = receipt.clone();
            history = history.child(
                button(
                    ("image-history", index),
                    format!(
                        "{} · {} · {:?} · {}",
                        receipt.provider_name, receipt.model, receipt.state, receipt.id
                    ),
                )
                .disabled(busy)
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.image_generation.latest = Some((receipt.id, receipt.session_id));
                    view.image_generation.needs_reconciliation = matches!(
                        receipt.state,
                        ImageGenerationState::Prepared
                            | ImageGenerationState::Submitted
                            | ImageGenerationState::OutcomeUnknown
                    );
                    view.image_generation.notice = receipt.message.clone();
                    view.remember_generated_artifacts(&receipt);
                    view.image_generation.receipt = Some(receipt.clone());
                    cx.notify();
                })),
            );
        }
        page = page.child(history);
        page.into_any_element()
    }
}

#[cfg(test)]
#[allow(dead_code)] // Exercised by the separate native validation test target.
impl ImageGenerationUi {
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
        self.provider_requests
            .insert(id, (self.provider(cx), false));
    }
    pub(super) fn validation_needs_reconciliation(&self) -> bool {
        self.needs_reconciliation
    }
}
