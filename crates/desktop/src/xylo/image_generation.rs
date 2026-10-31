//! Explicit image generation through the selected image API. Configuration holds
//! credential references; prompts and provider responses stay out of settings.
use super::*;
use crate::motion;
use gpui_kit::base::Disableable;
use gpui_kit::{
    TestSupportExt,
    component::{
        button::Button as MenuButton,
        menu::{DropdownMenu, PopupMenuItem},
    },
};
use std::time::Instant;

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

/// Which image-provider form is open over the list.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProviderForm {
    Add,
    Edit,
}
pub(super) struct ImageGenerationUi {
    pub(super) name: Entity<InputState>,
    pub(super) endpoint: Entity<InputState>,
    pub(super) model: Entity<InputState>,
    secret: Entity<InputState>,
    connection_source: Option<(ImageGenerationProtocol, String)>,
    load_error: bool,
    save_error: bool,
    provider_latest: Option<Uuid>,
    /// When the latest provider load or save was sent, for the slow-wait
    /// caption.
    provider_since: Option<Instant>,
    /// The provider image tools use, as last read or saved.
    configured: Option<ImageGenerationProvider>,
    /// The open add or edit form; the list shows when none is.
    form: Option<ProviderForm>,
    protocol: ImageGenerationProtocol,
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
            provider_since: None,
            configured: None,
            form: None,
            protocol: ImageGenerationProtocol::OpenAiImages,
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
            self.image_generation.provider_since = Some(self.arm_slow_hint(cx));
            self.image_generation
                .provider_requests
                .insert(id, (snapshot, false));
        }
        if let Err(error) = result {
            self.image_generation.load_error = true;
            self.image_generation.notice = product_error(&error.to_string());
        }
        cx.notify();
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
            Err(error) => self.image_generation.notice = product_error(&error),
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
        if provider.model.is_empty() {
            self.image_generation.save_error = true;
            self.image_generation.notice = "Enter the image model this provider should use.".into();
            cx.notify();
            return;
        }
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
            self.image_generation.provider_since = Some(self.arm_slow_hint(cx));
            self.image_generation
                .provider_requests
                .insert(id, (provider, true));
            self.image_generation.notice = "Saving image provider…".into();
        } else if let Err(error) = result {
            self.image_generation.save_error = true;
            self.image_generation.notice = product_error(&error.to_string());
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
        self.image_generation.provider_since = None;
        match result {
            Ok(provider) => {
                self.image_generation.configured = provider
                    .clone()
                    .or_else(|| saving.then(|| snapshot.clone()));
                if (saving || self.image_generation.provider(cx) == snapshot)
                    && let Some(provider) = provider
                {
                    self.image_generation.apply_provider(provider, window, cx);
                }
                if saving {
                    self.image_generation
                        .secret
                        .update(cx, |input, cx| input.set_value("", window, cx));
                }
                self.image_generation.load_error = false;
                self.image_generation.save_error = false;
                self.image_generation.notice = if saving {
                    "Saved image provider. New image requests in chat use it.".into()
                } else {
                    String::new()
                };
            }
            Err(error) => {
                self.image_generation.load_error = !saving;
                self.image_generation.save_error = saving;
                self.image_generation.notice = product_error(&error);
            }
        }
        cx.notify();
        true
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
    /// Opens the form empty, for a new image provider.
    fn open_image_provider_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ui = &mut self.image_generation;
        for input in [&ui.name, &ui.endpoint, &ui.model, &ui.secret] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        ui.connection_source = None;
        ui.protocol = ImageGenerationProtocol::OpenAiImages;
        ui.credential_ref = None;
        ui.headers.clear();
        ui.notice.clear();
        ui.save_error = false;
        ui.form = Some(ProviderForm::Add);
        cx.notify();
    }
    /// Opens the form on the configured provider, dropping unsaved edits.
    fn open_image_provider_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(provider) = self.image_generation.configured.clone() else {
            return;
        };
        self.image_generation
            .secret
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.image_generation.apply_provider(provider, window, cx);
        self.image_generation.notice.clear();
        self.image_generation.save_error = false;
        self.image_generation.form = Some(ProviderForm::Edit);
        cx.notify();
    }
    /// Leaves the form; unsaved edits and the typed key are discarded.
    fn close_image_provider_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.image_generation
            .secret
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.image_generation.form = None;
        if !self.image_generation.save_error {
            self.image_generation.notice.clear();
        }
        cx.notify();
    }
    /// The saved chat connections whose credentials can also generate images.
    fn image_connection_menu(&self, disabled: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
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
        if connections.is_empty() {
            return None;
        }
        let weak = cx.entity().downgrade();
        Some(
            MenuButton::new("image-saved-connection")
                .outline()
                .h(px(settings_ui::CONTROL_HEIGHT))
                .px(px(10.))
                .gap(px(6.))
                .accessibility_label("Use a saved provider")
                .child(div().single_line().child("Use a saved provider"))
                .child(icons::small(AppIcon::ChevronDown).text_color(rgb(icons::rest_color())))
                .disabled(disabled)
                .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, _| {
                    for provider in &connections {
                        let provider = provider.clone();
                        let view = weak.clone();
                        menu = menu.item(PopupMenuItem::new(provider.name.clone()).on_click(
                            move |_, window, cx| {
                                let _ = view.update(cx, |view, cx| {
                                    view.image_generation
                                        .secret
                                        .update(cx, |input, cx| input.set_value("", window, cx));
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
                })
                .into_any_element(),
        )
    }
    /// Pure render: loading starts from navigation, never from here. The
    /// configured provider is listed; adding or editing opens a sub-page.
    pub(super) fn image_provider_settings_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let page = match self.image_generation.form {
            Some(form) => self.image_provider_form(form, cx),
            None => self.image_provider_list(cx),
        };
        // The id the native validation suite observes.
        div()
            .id("image-provider-settings")
            .test_support()
            .size_full()
            .child(page)
            .into_any_element()
    }
    /// Whether a provider load or save is in flight: (saving, loading).
    fn image_provider_requests(&self) -> (bool, bool) {
        let requests = self.image_generation.provider_requests.values();
        let saving = requests.clone().any(|(_, saving)| *saving);
        let loading = requests.clone().any(|(_, saving)| !*saving);
        (saving, loading)
    }
    fn image_provider_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let (_, loading) = self.image_provider_requests();
        let add = settings_ui::primary_button("add-image-provider", "Add image provider")
            .on_click(cx.listener(|view, _, window, cx| view.open_image_provider_add(window, cx)));
        let mut page = settings_ui::settings_page("image-provider-list", "Image generation")
            .description("The provider image tools in chat use to create pictures.");
        if self.image_generation.load_error {
            page = page.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(settings_ui::inline_error(&self.image_generation.notice)),
                    )
                    .child(
                        settings_ui::secondary_button("image-provider-retry-load", "Retry loading")
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.image_generation.loaded = false;
                                view.image_generation.load_error = false;
                                view.load_image_generation_provider(cx);
                                cx.notify();
                            })),
                    ),
            );
        }
        match &self.image_generation.configured {
            Some(provider) => {
                let protocol = match provider.protocol {
                    ImageGenerationProtocol::OpenAiImages => "OpenAI Images",
                    ImageGenerationProtocol::GeminiGenerateContent => "Gemini",
                };
                let name = if provider.name.is_empty() {
                    "Image provider".to_owned()
                } else {
                    provider.name.clone()
                };
                page = page.child(settings_ui::list_container([settings_ui::list_item(
                    "image-provider-item",
                    Some(settings_ui::monogram(&name, protocol).into_any_element()),
                    name,
                    Some(format!("{} · {protocol}", provider.model).into()),
                )
                .trailing(settings_ui::status_pill(settings_ui::Status::Ready))
                .on_click(
                    cx.listener(|view, _, window, cx| view.open_image_provider_edit(window, cx)),
                )
                .into_any_element()]));
            }
            None if loading => {
                page = page.action(add).child(settings_ui::loading_list(
                    1,
                    self.image_generation.provider_since,
                ));
            }
            None => {
                page = page.child(settings_ui::empty_state(
                    AppIcon::Image,
                    "No image provider",
                    "Add one so image tools in chat can create pictures.",
                    Some(add.into_any_element()),
                ));
            }
        }
        page.into_any_element()
    }
    fn image_provider_form(&self, form: ProviderForm, cx: &mut Context<Self>) -> AnyElement {
        let (saving, loading) = self.image_provider_requests();
        let disabled = saving || loading;
        let title: SharedString = match (form, &self.image_generation.configured) {
            (ProviderForm::Edit, Some(provider)) if !provider.name.is_empty() => {
                provider.name.clone().into()
            }
            (ProviderForm::Edit, _) => "Image provider".into(),
            (ProviderForm::Add, _) => "Add image provider".into(),
        };
        let back = settings_ui::crumb("image-provider-back", "Image generation").on_click(
            cx.listener(|view, _, window, cx| view.close_image_provider_form(window, cx)),
        );
        let mut page = settings_ui::settings_page("image-provider-form", title.clone())
            .description(
                if form == ProviderForm::Add && self.image_generation.configured.is_some() {
                    "Saving replaces the current image provider."
                } else {
                    "Image tools in chat send their requests here."
                },
            )
            .breadcrumb(settings_ui::breadcrumb([back], title));
        if let Some(menu) = self.image_connection_menu(disabled, cx) {
            page = page.action(menu);
        }
        let status = if saving {
            "Saving image provider…"
        } else {
            &self.image_generation.notice
        };
        let slow = self
            .image_generation
            .provider_since
            .filter(|_| saving)
            .and_then(settings_ui::slow_caption);
        if !status.is_empty() {
            let failed = self.image_generation.save_error;
            page = page.child(
                div()
                    .id("image-provider-status")
                    .role(Role::Status)
                    .aria_label(status.to_owned())
                    .test_support()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(motion::fade_on_change(
                        "image-provider-status-text",
                        &status,
                        if failed {
                            settings_ui::inline_error(status)
                        } else {
                            notice(BadgeTone::Neutral, status.to_owned())
                        },
                    ))
                    .children(slow),
            );
        }
        let input = |label: &'static str, state: &Entity<InputState>| {
            settings_ui::field(
                label,
                Input::new(state)
                    .disabled(disabled)
                    .aria_label(format!("Image provider {label}")),
                None,
            )
            .into_any_element()
        };
        let mut protocols = segments();
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
                segment(id, label, self.image_generation.protocol == protocol)
                    .disabled(disabled)
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.image_generation.protocol = protocol;
                        cx.notify();
                    })),
            );
        }
        let model_missing = self
            .image_generation
            .model
            .read(cx)
            .value()
            .trim()
            .is_empty();
        let form = settings_ui::form_column([
            input("Name", &self.image_generation.name),
            input("Endpoint", &self.image_generation.endpoint),
            settings_ui::field("API", protocols.self_start(), None).into_any_element(),
            input("Model", &self.image_generation.model),
            settings_ui::field(
                "API key",
                Input::new(&self.image_generation.secret)
                    .disabled(disabled)
                    .aria_label("Image provider API key"),
                Some(
                    "Kept in Xylo's secret storage. Leave blank to keep a saved key; local servers can use no key."
                        .into(),
                ),
            )
            .into_any_element(),
            settings_ui::form_actions([
                settings_ui::busy(
                    settings_ui::primary_button("image-save-provider", "Save image provider")
                        .disabled(disabled || model_missing)
                        .on_click(
                            cx.listener(|view, _, _, cx| view.save_image_generation_provider(cx)),
                        ),
                    saving,
                )
                .into_any_element(),
            ])
            .into_any_element(),
        ]);
        page.child(settings_ui::section("Provider", None, form))
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
            status: Default::default(),
        };
        div()
            .max_w(px(440.))
            .pl_2()
            .child(self.message_images(&output, cx))
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "image_generation/tests.rs"]
mod tests;
