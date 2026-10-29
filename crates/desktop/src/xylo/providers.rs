//! Provider settings use an isolated editor; viewing a connection never activates it.
use super::*;

pub(super) fn same_connection(a: &ProviderConfig, b: &ProviderConfig) -> bool {
    a.family == b.family
        && a.endpoint == b.endpoint
        && a.credential_ref == b.credential_ref
        && a.headers == b.headers
        && a.discovery_url == b.discovery_url
}
pub(super) fn provider_kind(provider: &ProviderConfig) -> &'static str {
    if provider.credential_ref.as_deref() == Some(CHATGPT_REF) {
        return "ChatGPT";
    }
    if !provider.headers.is_empty() || provider.discovery_url.is_some() {
        return "Custom";
    }
    match (&provider.family, provider.endpoint.trim_end_matches('/')) {
        (ApiFamily::OpenAiResponses | ApiFamily::OpenAiChat, "https://api.openai.com/v1") => {
            "OpenAI"
        }
        (ApiFamily::Anthropic, "https://api.anthropic.com") => "Anthropic",
        (ApiFamily::Gemini, "https://generativelanguage.googleapis.com") => "Gemini",
        (ApiFamily::OpenAiChat, endpoint)
            if endpoint.starts_with("http://127.0.0.1:")
                || endpoint.starts_with("http://localhost:") =>
        {
            "Local"
        }
        _ => "Custom",
    }
}
impl Xylo {
    pub(super) fn choose_provider_kind(
        &mut self,
        kind: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reset_provider_discovery(cx);
        self.cancel_account_command(cx);
        let (name, endpoint, family) = match kind {
            "OpenAI" => (
                "OpenAI",
                "https://api.openai.com/v1",
                ApiFamily::OpenAiResponses,
            ),
            "Anthropic" => (
                "Anthropic",
                "https://api.anthropic.com",
                ApiFamily::Anthropic,
            ),
            "Gemini" => (
                "Gemini",
                "https://generativelanguage.googleapis.com",
                ApiFamily::Gemini,
            ),
            "Local" => (
                "Local model",
                "http://127.0.0.1:11434/v1",
                ApiFamily::OpenAiChat,
            ),
            "ChatGPT" => (
                "ChatGPT",
                "https://api.openai.com/v1",
                ApiFamily::OpenAiResponses,
            ),
            _ => ("Custom provider", "", ApiFamily::OpenAiChat),
        };
        let provider = ProviderConfig {
            name: name.into(),
            endpoint: endpoint.into(),
            family,
            credential_ref: if kind == "ChatGPT" {
                Some(CHATGPT_REF.into())
            } else {
                None
            },
            ..Default::default()
        };
        self.provider_form(&provider, window, cx);
        self.provider_editor_kind = kind.into();
        if kind == "ChatGPT" {
            self.account_command("account_chatgpt_status", cx);
        }
        cx.notify();
    }
    fn provider_input(&self, label: &'static str, input: &Entity<InputState>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(subtitle(label))
            .child(
                Input::new(input)
                    .aria_label(label)
                    .disabled(self.provider_discovery.loading()),
            )
            .into_any_element()
    }
    fn use_discovered_model(&mut self, model: String, window: &mut Window, cx: &mut Context<Self>) {
        let result = (|| -> anyhow::Result<()> {
            let current = self.provider_from_form(cx)?;
            if !self.secret.read(cx).value().trim().is_empty() {
                anyhow::bail!("Save connection changes before switching models");
            }
            let provider = self
                .settings
                .providers
                .iter()
                .find(|provider| provider.model == model && same_connection(provider, &current))
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!("Reload this connection's models after saving changes")
                })?;
            if provider.credential_ref.as_deref() == Some(CHATGPT_REF)
                && !self.account_can_select_model(&model)
            {
                anyhow::bail!("Load an authorized model list before choosing a ChatGPT model");
            }
            provider.validate()?;
            let mut settings = self.settings.clone();
            settings.provider = provider.clone();
            settings.remember_model(provider.clone());
            let host = self
                .host
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))?;
            host.save_settings(settings.clone())?;
            if provider.credential_ref.as_deref() == Some(CHATGPT_REF) {
                self.account_model_selected();
            }
            self.settings = settings;
            self.provider_editor_source = Some(provider.clone());
            Self::set_input(&self.model, provider.model.clone(), window, cx);
            Self::set_input(
                &self.context,
                provider.context_tokens.to_string(),
                window,
                cx,
            );
            Self::set_input(&self.output, provider.output_tokens.to_string(), window, cx);
            self.provider_form_open = false;
            self.page = Page::Providers;
            Ok(())
        })();
        self.result(result, cx);
        cx.notify();
    }
    fn provider_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        let kind = self.provider_editor_kind.as_str();
        let mut body = div().w_full().max_w(px(760.)).flex().flex_col().gap_5();
        body = body.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(button("provider-back", "← Providers").on_click(cx.listener(
                    |view, _, window, cx| {
                        view.reset_provider_discovery(cx);
                        view.cancel_account_command(cx);
                        view.provider_form_open = false;
                        Self::set_input(&view.secret, "", window, cx);
                        cx.notify();
                    },
                )))
                .when(!kind.is_empty(), |row| {
                    row.child(
                        button("provider-change-type", "Change provider type").on_click(
                            cx.listener(|view, _, window, cx| {
                                view.reset_provider_discovery(cx);
                                view.cancel_account_command(cx);
                                view.provider_editor_kind.clear();
                                Self::set_input(&view.secret, "", window, cx);
                                cx.notify();
                            }),
                        ),
                    )
                }),
        );
        body = body.child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child(
            if kind.is_empty() {
                "Add provider".into()
            } else {
                format!("Configure {kind}")
            },
        ));
        let mut types = div().w_full().max_w(px(560.)).flex().flex_col()
            .border_1().border_color(rgb(border())).rounded_lg().overflow_hidden();
        for (id, label, detail, glyph) in [
            ("preset-chatgpt", "ChatGPT", "Use your ChatGPT account", gpui_kit::component::IconName::Bot),
            ("preset-openai", "OpenAI", "Connect with an API key", gpui_kit::component::IconName::Bot),
            ("preset-anthropic", "Anthropic", "Connect with an API key", gpui_kit::component::IconName::Bot),
            ("preset-gemini", "Gemini", "Connect with an API key", gpui_kit::component::IconName::Bot),
            ("preset-local", "Local", "Ollama or a local model server", gpui_kit::component::IconName::SquareTerminal),
            ("preset-custom", "Custom", "Your endpoint, protocol and headers", gpui_kit::component::IconName::Settings),
        ] {
            types = types.child(
                focus_ring(gpui_kit::base::Button::new(id))
                    .accessibility_label(label)
                    .w_full().h(px(57.)).px_4().flex().items_center().gap_3()
                    .text_color(rgb(text())).bg(rgb(panel()))
                    .border_b_1().border_color(rgb(border()))
                    .hover(|style|style.bg(rgb(hover())))
                    .child(icon(glyph).text_color(rgb(muted())))
                    .child(div().flex_1().min_w_0().flex().flex_col().items_start().gap_1()
                        .child(div().text_size(px(12.)).font_weight(FontWeight::MEDIUM).child(label))
                        .child(div().text_size(px(11.)).text_color(rgb(muted())).child(detail)))
                    .child(icon(gpui_kit::component::IconName::ChevronRight).text_color(rgb(muted())).size(px(13.)))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.choose_provider_kind(label, window, cx)
                    })),
            );
        }
        if kind.is_empty() {
            return body.child(types).into_any_element();
        }
        if kind == "ChatGPT" {
            return body
                .child(self.account_section(cx))
                .child(self.provider_discovery_feedback(cx))
                .child(self.provider_model_choices(cx))
                .into_any_element();
        }
        let custom = kind == "Custom";
        let local = kind == "Local";
        let mut form = div()
            .w_full()
            .max_w(px(560.))
            .flex()
            .flex_col()
            .gap_4();
        if custom {
            form = form.child(self.provider_input("Provider name", &self.provider_name));
        }
        if custom || local {
            form = form.child(self.provider_input("Base URL", &self.endpoint));
        }
        if custom {
            let source = self.provider_editor_source.as_ref();
            let mut protocols = div().flex().flex_wrap().gap_2();
            for (id, label, family) in [
                ("protocol-chat", "OpenAI Chat", ApiFamily::OpenAiChat),
                (
                    "protocol-responses",
                    "OpenAI Responses",
                    ApiFamily::OpenAiResponses,
                ),
                ("protocol-anthropic", "Anthropic", ApiFamily::Anthropic),
                ("protocol-gemini", "Gemini", ApiFamily::Gemini),
            ] {
                protocols = protocols.child(
                    button(id, label)
                        .disabled(self.provider_discovery.loading())
                        .bg(rgb(if source.is_some_and(|p| p.family == family) {
                            selected()
                        } else {
                            panel()
                        }))
                        .on_click(cx.listener(move |view, _, _, cx| {
                            if let Some(provider) = view.provider_editor_source.as_mut()
                                && provider.family != family
                            {
                                provider.family = family.clone();
                                provider.credential_ref = None;
                                view.models.clear();
                                view.model_context_limits.clear();
                            }
                            cx.notify();
                        })),
                );
            }
            form = form.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(subtitle("API protocol"))
                    .child(protocols),
            );
        }
        if !local
            || self
                .provider_editor_source
                .as_ref()
                .is_some_and(|p| p.credential_ref.is_some())
        {
            form = form.child(
                self.provider_input(
                    if self
                        .provider_editor_source
                        .as_ref()
                        .is_some_and(|p| p.credential_ref.is_some())
                    {
                        "API key (leave blank to keep saved key)"
                    } else if custom {
                        "API key (optional)"
                    } else {
                        "API key"
                    },
                    &self.secret,
                ),
            );
        }
        if custom {
            form = form.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(subtitle("Extra headers (JSON object)"))
                    .child(
                        Textarea::new(&self.provider_headers)
                            .disabled(self.provider_discovery.loading())
                            .aria_label("Extra headers (JSON object)"),
                    ),
            );
            form = form
                .child(self.provider_input(
                    "Model discovery URL (optional)",
                    &self.provider_discovery_url,
                ))
                .child(subtitle(
                    "Uses this connection’s authentication and headers.",
                ));
        }
        form = form.child(self.provider_input("Model ID (optional)", &self.model));
        form = form.child(
            primary("provider-save", "Save & discover models")
                .disabled(self.provider_discovery.loading())
                .on_click(cx.listener(|view, _, window, cx| view.save_provider(window, cx))),
        );
        body = body.child(form);
        body = body
            .child(self.provider_discovery_feedback(cx))
            .child(self.provider_model_choices(cx));
        body.into_any_element()
    }
    pub(super) fn providers_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut content = div().w_full().max_w(px(760.)).flex().flex_col().gap_4();
        if self.provider_form_open {
            content = content.child(self.provider_editor(cx));
        } else {
            content = content.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Model providers"),
                    )
                    .child(
                        button("add-provider", "+ Add provider").on_click(cx.listener(
                            |view, _, window, cx| {
                                view.provider_form(&ProviderConfig::default(), window, cx);
                                view.provider_editor_kind.clear();
                                view.provider_form_open = true;
                                cx.notify();
                            },
                        )),
                    ),
            );
            let mut providers: Vec<ProviderConfig> = Vec::new();
            for provider in self
                .settings
                .providers
                .iter()
                .chain(std::iter::once(&self.settings.provider))
            {
                if provider.name.is_empty() {
                    continue;
                }
                if let Some(existing) = providers
                    .iter_mut()
                    .find(|saved| same_connection(saved, provider))
                {
                    if same_connection(provider, &self.settings.provider) {
                        *existing = self.settings.provider.clone();
                    }
                } else {
                    providers.push(provider.clone());
                }
            }
            let mut list = div()
                .w_full()
                .flex()
                .flex_col()
                .rounded_lg()
                .border_1()
                .border_color(rgb(border()));
            if providers.is_empty() {
                list = list.child(
                    div()
                        .p_4()
                        .child(subtitle("Add a provider to start a conversation.")),
                );
            }
            for (index, provider) in providers.into_iter().enumerate() {
                list = list.child(
                    div()
                        .px_4()
                        .py_4()
                        .flex()
                        .items_center()
                        .gap_3()
                        .border_b_1()
                        .border_color(rgb(border()))
                        .child(icon(gpui_kit::component::IconName::Bot))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(provider.name.clone()),
                                )
                                .child(subtitle(provider.model.clone())),
                        )
                        .child(button(("configure-provider", index), "Configure").on_click(
                            cx.listener(move |view, _, window, cx| {
                                view.provider_form(&provider, window, cx);
                                view.provider_form_open = true;
                                if provider.credential_ref.as_deref() == Some(CHATGPT_REF) {
                                    view.account_command("account_chatgpt_status", cx);
                                }
                                cx.notify();
                            }),
                        )),
                );
            }
            content = content.child(list);
            content = content
                .child(
                    div()
                        .mt_4()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Image generation"),
                )
                .child(
                    div()
                        .px_4()
                        .py_4()
                        .flex()
                        .items_center()
                        .gap_3()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(border()))
                        .child(icon(gpui_kit::component::IconName::FileText))
                        .child(div().flex_1().text_sm().child("Image provider"))
                        .child(button("configure-image-provider", "Configure").on_click(
                            cx.listener(|view, _, _, cx| {
                                view.reset_provider_discovery(cx);
                                view.cancel_account_command(cx);
                                view.navigate(Page::Images, cx);
                            }),
                        )),
                );
        }
        div()
            .id("provider-scroll")
            .size_full()
            .overflow_y_scroll()
            .px(px(45.))
            .py(px(38.))
            .flex()
            .justify_center()
            .child(content)
            .into_any_element()
    }
}
#[derive(Default)]
pub(super) enum DiscoveryStatus {
    #[default]
    Idle,
    Loading,
    Ready,
    Empty,
    Failed(String),
    Cancelled,
}
#[derive(Default)]
pub(super) struct ProviderDiscoveryUi {
    current: Option<(u64, ProviderConfig)>,
    status: DiscoveryStatus,
    #[cfg(test)]
    pub(super) test_dispatch: bool,
    #[cfg(test)]
    pub(super) test_generation: u64,
    #[cfg(test)]
    pub(super) test_requests: Vec<(u64, ProviderConfig)>,
}
impl ProviderDiscoveryUi {
    pub(super) fn retry_state(&self) -> bool {
        matches!(
            self.status,
            DiscoveryStatus::Failed(_) | DiscoveryStatus::Empty | DiscoveryStatus::Cancelled
        )
    }
    pub(super) fn ready(&self) -> bool {
        matches!(self.status, DiscoveryStatus::Ready)
    }
    pub(super) fn failed(&self) -> bool {
        matches!(self.status, DiscoveryStatus::Failed(_))
    }
    pub(super) fn loading(&self) -> bool {
        matches!(self.status, DiscoveryStatus::Loading)
    }
}
impl Xylo {
    fn begin_provider_discovery(&mut self, provider: ProviderConfig, generation: u64) {
        self.models.clear();
        self.model_context_limits.clear();
        self.provider_discovery.current = Some((generation, provider));
        self.provider_discovery.status = DiscoveryStatus::Loading;
    }
    #[cfg(test)]
    fn test_discovery_dispatch(
        &mut self,
        provider: &ProviderConfig,
    ) -> anyhow::Result<Option<u64>> {
        if !self.provider_discovery.test_dispatch {
            return Ok(None);
        }
        let generation = if let Some(host) = &self.host {
            host.cancel_model_discovery()?;
            host.provider_discovery_generation()
        } else {
            self.provider_discovery.test_generation += 1;
            self.provider_discovery.test_generation
        };
        self.provider_discovery
            .test_requests
            .push((generation, provider.clone()));
        Ok(Some(generation))
    }
    pub(super) fn request_provider_models(
        &mut self,
        provider: ProviderConfig,
    ) -> anyhow::Result<()> {
        #[cfg(test)]
        if let Some(generation) = self.test_discovery_dispatch(&provider)? {
            self.begin_provider_discovery(provider, generation);
            return Ok(());
        }
        let host = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))?;
        host.discover_models(provider.clone())?;
        let generation = host.provider_discovery_generation();
        self.begin_provider_discovery(provider, generation);
        Ok(())
    }
    pub(super) fn dispatch_provider_save(
        &mut self,
        settings: AppSettings,
        provider: ProviderConfig,
        secret: Option<String>,
    ) -> anyhow::Result<()> {
        #[cfg(test)]
        if let Some(generation) = self.test_discovery_dispatch(&provider)? {
            if let Some(host) = &self.host {
                host.save_settings(settings)?;
            }
            self.begin_provider_discovery(provider, generation);
            return Ok(());
        }
        let host = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))?;
        host.save_provider_connection(settings, provider.clone(), secret)?;
        let generation = host.provider_discovery_generation();
        self.begin_provider_discovery(provider, generation);
        Ok(())
    }
    pub(super) fn provider_catalog_received(
        &mut self,
        provider: &ProviderConfig,
        generation: u64,
        models: &[String],
    ) -> bool {
        if provider.credential_ref.as_deref() == Some(CHATGPT_REF)
            && !self.account_can_load_models()
        {
            return false;
        }
        if let Some((expected, identity)) = &self.provider_discovery.current {
            if *expected != generation
                || !same_connection(identity, provider)
                || !self.provider_discovery.loading()
            {
                return false;
            }
        } else if !self.native.select_local_model {
            return false;
        }
        self.provider_discovery.status = if models.is_empty() {
            DiscoveryStatus::Empty
        } else {
            DiscoveryStatus::Ready
        };
        true
    }
    pub(super) fn provider_catalog_failed(
        &mut self,
        provider: &ProviderConfig,
        generation: u64,
        message: &str,
    ) -> bool {
        if let Some((expected, identity)) = &self.provider_discovery.current {
            if *expected != generation
                || !same_connection(identity, provider)
                || !self.provider_discovery.loading()
            {
                return false;
            }
        } else {
            return false;
        }
        self.models.clear();
        self.provider_discovery.status = DiscoveryStatus::Failed(message.into());
        true
    }
    pub(super) fn cancel_provider_discovery(&mut self, cx: &mut Context<Self>) {
        if self.provider_discovery.loading() {
            let result = if let Some(host) = &self.host {
                if self
                    .provider_discovery
                    .current
                    .as_ref()
                    .is_some_and(|(generation, _)| {
                        *generation == host.provider_discovery_generation()
                    })
                {
                    host.cancel_model_discovery()
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            };
            if let Err(error) = result {
                self.error = Some(error.to_string());
            }
        }
        self.models.clear();
        self.model_context_limits.clear();
        self.provider_discovery.current = None;
        self.provider_discovery.status = DiscoveryStatus::Cancelled;
        cx.notify();
    }
    pub(super) fn reset_provider_discovery(&mut self, cx: &mut Context<Self>) {
        self.cancel_provider_discovery(cx);
        self.provider_discovery.status = DiscoveryStatus::Idle;
    }
    pub(super) fn provider_discovery_feedback(&self, cx: &mut Context<Self>) -> AnyElement {
        let (label, retry, cancel) = match &self.provider_discovery.status {
            DiscoveryStatus::Idle | DiscoveryStatus::Ready => return div().into_any_element(),
            DiscoveryStatus::Loading => ("Loading models…".into(), false, true),
            DiscoveryStatus::Empty => (
                if self.provider_editor_kind == "ChatGPT" {
                    "No authorized models were returned. Retry or choose another provider.".into()
                } else {
                    "No models were returned. Retry or enter a model ID.".into()
                },
                true,
                false,
            ),
            DiscoveryStatus::Failed(error) => (
                format!("Could not load models: {}", product_error(error)),
                true,
                false,
            ),
            DiscoveryStatus::Cancelled => ("Model discovery cancelled.".into(), true, false),
        };
        div()
            .id("provider-discovery-feedback")
            .flex()
            .flex_col()
            .gap_2()
            .child(subtitle(label))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .when(retry, |row| {
                        row.child(button("provider-retry", "Retry").on_click(cx.listener(
                            |view, _, window, cx| {
                                if view.provider_editor_kind == "ChatGPT" {
                                    view.use_chatgpt(window, cx);
                                } else {
                                    view.save_provider(window, cx);
                                }
                            },
                        )))
                    })
                    .when(cancel, |row| {
                        row.child(button("provider-cancel-discovery", "Cancel").on_click(
                            cx.listener(|view, _, _, cx| view.cancel_provider_discovery(cx)),
                        ))
                    }),
            )
            .into_any_element()
    }
    pub(super) fn provider_model_choices(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.models.is_empty()
            || (self.provider_editor_kind == "ChatGPT" && !self.account_can_load_models())
        {
            return div().into_any_element();
        }
        let weak = cx.entity().downgrade();
        let rows = uniform_list(
            "available-models",
            self.models.len(),
            move |range, _, cx| {
                weak.update(cx, |view, cx| {
                    range
                        .filter_map(|index| {
                            let model = view.models.get(index)?.clone();
                            let active = view.settings.provider.model == model
                                && view.provider_editor_source.as_ref().is_some_and(|source| {
                                    same_connection(source, &view.settings.provider)
                                });
                            Some(
                                button(("model", index), model.clone())
                                    .selected(active)
                                    .bg(rgb(if active { selected() } else { panel() }))
                                    .w_full()
                                    .h(px(34.))
                                    .justify_start()
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        view.use_discovered_model(model.clone(), window, cx)
                                    }))
                                    .into_any_element(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
            },
        )
        .track_scroll(&self.model_scroll)
        .h(px(240.));
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(subtitle("Available models"))
            .child(rows)
            .into_any_element()
    }
}

impl Xylo {
    /// Reconcile one accepted catalog in bulk. Rendering never walks this registry.
    pub(super) fn register_discovered_models(
        &mut self,
        source: &ProviderConfig,
        catalog: &[String],
        context_limits: &BTreeMap<String, u32>,
        reasoning_options: &BTreeMap<String, Vec<String>>,
        default_reasoning_efforts: &BTreeMap<String, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        if catalog.is_empty() {
            return Ok(());
        }
        let mut seen = BTreeSet::new();
        let models: Vec<&String> = catalog
            .iter()
            .filter(|model| !model.trim().is_empty() && seen.insert(model.as_str()))
            .collect();
        if models.is_empty() {
            return Ok(());
        }
        let previous: BTreeMap<&str, &ProviderConfig> = self
            .settings
            .providers
            .iter()
            .filter(|provider| same_connection(provider, source))
            .map(|provider| (provider.model.as_str(), provider))
            .collect();
        let preferred = if seen.contains(source.model.as_str()) {
            source.model.as_str()
        } else if same_connection(&self.settings.provider, source)
            && seen.contains(self.settings.provider.model.as_str())
        {
            self.settings.provider.model.as_str()
        } else {
            models[0].as_str()
        };
        let mut registered = Vec::with_capacity(models.len());
        for model in models {
            let saved = previous.get(model.as_str()).copied();
            let mut provider = source.clone();
            provider.model = model.clone();
            provider.reasoning_options = reasoning_options.get(model).cloned();
            provider.default_reasoning_effort = default_reasoning_efforts.get(model).cloned();
            provider.reasoning_effort = saved
                .and_then(|saved| saved.reasoning_effort.clone())
                .or_else(|| {
                    (source.model == *model)
                        .then(|| source.reasoning_effort.clone())
                        .flatten()
                });
            provider.reasoning_effort = provider.effective_reasoning_effort().map(str::to_owned);
            if let Some(limit) = context_limits
                .get(model)
                .copied()
                .filter(|limit| *limit >= 1024)
            {
                provider.context_tokens = limit;
                provider.output_tokens = provider.output_tokens.min(limit / 4);
            } else if let Some(saved) = saved {
                provider.context_tokens = saved.context_tokens;
                provider.output_tokens = saved.output_tokens;
            }
            registered.push(provider);
        }
        let selected = registered
            .iter()
            .find(|provider| provider.model == preferred)
            .unwrap()
            .clone();
        let mut settings = self.settings.clone();
        settings
            .providers
            .retain(|provider| !same_connection(provider, source));
        settings.providers.extend(registered);
        settings.provider = selected.clone();
        if let Some(host) = &self.host {
            host.save_settings(settings.clone())?;
        } else {
            #[cfg(test)]
            if !self.provider_discovery.test_dispatch {
                anyhow::bail!("Application service unavailable");
            }
            #[cfg(not(test))]
            anyhow::bail!("Application service unavailable");
        }
        if selected.credential_ref.as_deref() == Some(CHATGPT_REF) {
            self.account_model_selected();
        }
        self.settings = settings;
        self.provider_editor_source = Some(selected.clone());
        Self::set_input(&self.model, selected.model.clone(), window, cx);
        Self::set_input(
            &self.context,
            selected.context_tokens.to_string(),
            window,
            cx,
        );
        Self::set_input(&self.output, selected.output_tokens.to_string(), window, cx);
        Ok(())
    }
}
