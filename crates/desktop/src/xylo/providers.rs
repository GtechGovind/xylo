//! Provider settings use an isolated editor. Every model a connection
//! discovers becomes available in the model picker automatically; the
//! provider page never asks the user to choose one. Discovery activates a
//! model only when no usable model is active yet, or when the active model
//! disappeared from its own connection's catalog.
//!
//! A save is not trusted until the host answers. The host stores the secret,
//! then the settings, then runs discovery, so a catalog (even an empty one)
//! confirms the save, while a failure is a save failure unless the stored
//! settings already contain the connection. Until then the editor keeps its
//! previous credential reference and the typed key, so a failed save can be
//! retried and never leaves a provider whose secret was not stored.
use super::*;
use crate::motion;
use gpui_kit::TestSupportExt;
use std::cell::Cell;
use std::time::Instant;

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
/// The add-provider presets: element id, kind and one-line description.
const PROVIDER_PRESETS: [(&str, &str, &str); 6] = [
    (
        "preset-chatgpt",
        "ChatGPT",
        "Use the models included with your ChatGPT plan",
    ),
    ("preset-openai", "OpenAI", "Connect with an OpenAI API key"),
    (
        "preset-anthropic",
        "Anthropic",
        "Connect with an Anthropic API key",
    ),
    (
        "preset-gemini",
        "Gemini",
        "Connect with a Google AI Studio API key",
    ),
    (
        "preset-local",
        "Local",
        "Ollama or another model server on this computer",
    ),
    (
        "preset-custom",
        "Custom",
        "Your endpoint, protocol and headers",
    ),
];
/// What a saved connection's row reports. `account` is whether the ChatGPT
/// account may load models, or `None` while its status has not been read this
/// session; an unread status is not a missing setup, so the saved models
/// decide.
fn connection_status(
    kind: &str,
    has_credential: bool,
    models: usize,
    failed: bool,
    account: Option<bool>,
) -> settings_ui::Status {
    if failed {
        settings_ui::Status::Error
    } else if (kind == "ChatGPT" && account == Some(false))
        || (requires_api_key(kind) && !has_credential)
        || models == 0
    {
        settings_ui::Status::NeedsSetup
    } else {
        settings_ui::Status::Connected
    }
}
/// The description under a provider page's title.
fn provider_kind_description(kind: &str) -> &'static str {
    match kind {
        "" => "Choose how Xylo connects to a model.",
        "ChatGPT" => "Use the models included with your ChatGPT plan.",
        "Local" => "Ollama or another OpenAI-compatible server on this computer.",
        "Custom" => "Any compatible endpoint, with your protocol and headers.",
        _ => "Connect with an API key. Its models load automatically.",
    }
}
/// Hosted presets that cannot work without an API key.
pub(super) fn requires_api_key(kind: &str) -> bool {
    matches!(kind, "OpenAI" | "Anthropic" | "Gemini")
}
/// Saved connections, one row per connection, the active one reflecting its
/// current model.
fn saved_connections(settings: &AppSettings) -> Vec<ProviderConfig> {
    let mut connections: Vec<ProviderConfig> = Vec::new();
    for provider in settings
        .providers
        .iter()
        .chain(std::iter::once(&settings.provider))
    {
        if provider.name.is_empty() {
            continue;
        }
        if let Some(existing) = connections
            .iter_mut()
            .find(|saved| same_connection(saved, provider))
        {
            if same_connection(provider, &settings.provider) {
                *existing = settings.provider.clone();
            }
        } else {
            connections.push(provider.clone());
        }
    }
    connections
}
/// Builds one catalog entry per discovered model, keeping saved choices
/// (reasoning effort, limits) for models that were already known.
fn catalog_entries(
    source: &ProviderConfig,
    saved: &[ProviderConfig],
    models: &[&String],
    context_limits: &BTreeMap<String, u32>,
    reasoning_options: &BTreeMap<String, Vec<String>>,
    default_reasoning_efforts: &BTreeMap<String, String>,
) -> Vec<ProviderConfig> {
    let previous: BTreeMap<&str, &ProviderConfig> = saved
        .iter()
        .filter(|provider| same_connection(provider, source))
        .map(|provider| (provider.model.as_str(), provider))
        .collect();
    models
        .iter()
        .map(|model| {
            let saved = previous.get(model.as_str()).copied();
            let mut provider = source.clone();
            provider.model = (*model).clone();
            provider.reasoning_options = reasoning_options.get(*model).cloned();
            provider.default_reasoning_effort = default_reasoning_efforts.get(*model).cloned();
            provider.reasoning_effort = saved
                .and_then(|saved| saved.reasoning_effort.clone())
                .or_else(|| {
                    (source.model == **model)
                        .then(|| source.reasoning_effort.clone())
                        .flatten()
                });
            provider.reasoning_effort = provider.effective_reasoning_effort().map(str::to_owned);
            if let Some(limit) = context_limits
                .get(*model)
                .copied()
                .filter(|limit| *limit >= 1024)
            {
                provider.context_tokens = limit;
                provider.output_tokens = provider.output_tokens.min(limit / 4);
            } else if let Some(saved) = saved {
                provider.context_tokens = saved.context_tokens;
                provider.output_tokens = saved.output_tokens;
            }
            provider
        })
        .collect()
}
/// What a catalog did to the active model.
#[derive(Debug, PartialEq, Eq)]
enum Activation {
    /// The active model is untouched (or only its metadata refreshed).
    Kept,
    /// No usable model was active; the connection's first model now is.
    First,
    /// The active model is no longer offered; the named model replaced it.
    Replaced { previous: String },
}
/// Makes every model of `registered` (one connection's catalog, in catalog
/// order, non-empty) available and drops that connection's models that are no
/// longer offered.
fn apply_catalog(
    settings: &mut AppSettings,
    source: &ProviderConfig,
    registered: Vec<ProviderConfig>,
) -> Activation {
    settings
        .providers
        .retain(|provider| !same_connection(provider, source));
    let first = registered[0].clone();
    let active = registered
        .iter()
        .find(|provider| {
            same_connection(provider, &settings.provider)
                && provider.model == settings.provider.model
        })
        .cloned();
    for provider in registered {
        settings.remember_model(provider);
    }
    if let Some(active) = active {
        settings.provider = active;
        Activation::Kept
    } else if settings.provider.model.is_empty() {
        settings.provider = first;
        Activation::First
    } else if same_connection(&settings.provider, source) {
        let previous = std::mem::replace(&mut settings.provider, first).model;
        Activation::Replaced { previous }
    } else {
        Activation::Kept
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
        settings_ui::field(
            label,
            Input::new(input)
                .aria_label(label)
                .disabled(self.provider_discovery.loading()),
            None,
        )
        .into_any_element()
    }
    /// Asks before removing a saved connection and, when no other connection
    /// shares it, its stored secret.
    fn confirm_remove_provider(
        &mut self,
        provider: ProviderConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let detail = if same_connection(&provider, &self.settings.provider) {
            "Its models and saved key are removed from Xylo. Choose another model before your next chat."
        } else {
            "Its models and saved key are removed from Xylo."
        };
        self.confirm_destructive(
            &format!("Remove {}?", provider.name),
            detail,
            "Remove provider",
            window,
            cx,
            move |view, _, cx| {
                view.provider_discovery.page_error = None;
                view.provider_form_open = false;
                let result = view
                    .host
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
                    .and_then(|host| host.remove_provider(provider));
                if let Err(error) = result {
                    view.provider_discovery.page_error = Some(error.to_string());
                }
                cx.notify();
            },
        );
    }
    fn close_provider_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reset_provider_discovery(cx);
        self.cancel_account_command(cx);
        self.provider_form_open = false;
        self.provider_discovery.page_error = None;
        Self::set_input(&self.secret, "", window, cx);
        cx.notify();
    }
    /// Returns to the add-provider picker from a preset's form.
    fn open_provider_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reset_provider_discovery(cx);
        self.cancel_account_command(cx);
        self.provider_discovery.page_error = None;
        self.provider_form(&ProviderConfig::default(), window, cx);
        self.provider_editor_kind.clear();
        self.provider_form_open = true;
        cx.notify();
    }
    /// The saved connection the editor is showing, if it is saved.
    fn edited_connection(&self) -> Option<ProviderConfig> {
        let source = self.provider_editor_source.as_ref()?;
        self.settings
            .providers
            .iter()
            .find(|saved| same_connection(saved, source))
            .cloned()
    }
    /// The add-provider picker: one list item per preset.
    fn provider_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let presets =
            PROVIDER_PRESETS.iter().map(|(id, label, detail)| {
                settings_ui::list_item(
                    *id,
                    Some(settings_ui::monogram(label, &label.to_lowercase()).into_any_element()),
                    *label,
                    Some((*detail).into()),
                )
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.choose_provider_kind(label, window, cx)
                }))
                .into_any_element()
            });
        settings_ui::list_container(presets).into_any_element()
    }
    /// The connection form of an API-key, local or custom provider.
    fn provider_connection_form(&self, kind: &str, cx: &mut Context<Self>) -> AnyElement {
        let custom = kind == "Custom";
        let local = kind == "Local";
        let busy = self.provider_discovery.loading();
        let saved_key = self
            .provider_editor_source
            .as_ref()
            .is_some_and(|p| p.credential_ref.is_some());
        let mut fields: Vec<AnyElement> = Vec::new();
        if custom {
            fields.push(self.provider_input("Provider name", &self.provider_name));
        }
        if custom || local {
            fields.push(self.provider_input("Base URL", &self.endpoint));
        }
        if custom {
            let current = self
                .provider_editor_source
                .as_ref()
                .map(|p| p.family.clone());
            let weak = cx.entity().downgrade();
            let options = [
                ("OpenAI Chat", ApiFamily::OpenAiChat),
                ("OpenAI Responses", ApiFamily::OpenAiResponses),
                ("Anthropic", ApiFamily::Anthropic),
                ("Gemini", ApiFamily::Gemini),
            ]
            .into_iter()
            .map(|(label, family)| {
                let weak = weak.clone();
                let selected = current.as_ref() == Some(&family);
                settings_ui::SelectOption::new(label, selected, move |_, cx| {
                    let family = family.clone();
                    let _ = weak.update(cx, |view, cx| {
                        if view.provider_discovery.loading() {
                            return;
                        }
                        if let Some(provider) = view.provider_editor_source.as_mut()
                            && provider.family != family
                        {
                            provider.family = family;
                            provider.credential_ref = None;
                            view.models.clear();
                            view.model_context_limits.clear();
                        }
                        cx.notify();
                    });
                })
            })
            .collect();
            fields.push(
                settings_ui::field(
                    "API protocol",
                    settings_ui::select("provider-protocol", "API protocol", options),
                    None,
                )
                .into_any_element(),
            );
        }
        if !local || saved_key {
            fields.push(self.provider_input(
                if saved_key {
                    "API key (leave blank to keep saved key)"
                } else if custom {
                    "API key (optional)"
                } else {
                    "API key"
                },
                &self.secret,
            ));
        }
        if custom {
            fields.push(
                settings_ui::field(
                    "Extra headers (JSON object)",
                    Textarea::new(&self.provider_headers)
                        .disabled(busy)
                        .aria_label("Extra headers (JSON object)"),
                    None,
                )
                .into_any_element(),
            );
            fields.push(
                settings_ui::field(
                    "Model discovery URL (optional)",
                    Input::new(&self.provider_discovery_url)
                        .aria_label("Model discovery URL (optional)")
                        .disabled(busy),
                    Some("Uses this connection’s authentication and headers.".into()),
                )
                .into_any_element(),
            );
        }
        fields.push(self.provider_input("Model ID (optional)", &self.model));
        let key_typed = !self.secret.read(cx).value().trim().is_empty();
        let saved = self.edited_connection().is_some();
        // Which discovery (not a save) is running, to mark its button busy.
        let loading = matches!(self.provider_discovery.status, DiscoveryStatus::Loading)
            .then_some(self.provider_discovery.purpose);
        fields.push(
            settings_ui::form_actions([
                settings_ui::busy(
                    settings_ui::secondary_button("provider-test", "Test connection")
                        .disabled(busy || key_typed)
                        .on_click(cx.listener(|view, _, _, cx| view.test_provider_connection(cx))),
                    loading == Some(DiscoveryPurpose::Test),
                )
                .into_any_element(),
                settings_ui::busy(
                    settings_ui::secondary_button("provider-refresh-models", "Refresh models")
                        .disabled(busy || key_typed || !saved)
                        .on_click(cx.listener(|view, _, _, cx| view.refresh_provider_models(cx))),
                    loading == Some(DiscoveryPurpose::Catalog),
                )
                .into_any_element(),
                settings_ui::busy(
                    settings_ui::primary_button("provider-save", "Save provider")
                        .disabled(busy)
                        .on_click(
                            cx.listener(|view, _, window, cx| view.save_provider(window, cx)),
                        ),
                    matches!(self.provider_discovery.status, DiscoveryStatus::Saving),
                )
                .into_any_element(),
            ])
            .into_any_element(),
        );
        fields.push(
            settings_ui::caption(if key_typed {
                "Saving stores the key and loads this provider's models. Save the new key before testing it."
            } else {
                "Saving loads this provider's models. Testing loads them without saving changes."
            })
            .into_any_element(),
        );
        settings_ui::section("Connection", None, settings_ui::form_column(fields))
            .into_any_element()
    }
    /// The picker (no preset chosen yet) or one provider's detail page.
    fn provider_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        let kind = self.provider_editor_kind.as_str();
        let saved_connection = self.edited_connection();
        let back = settings_ui::crumb("provider-back", "Providers")
            .on_click(cx.listener(|view, _, window, cx| view.close_provider_editor(window, cx)));
        let title: SharedString = if kind.is_empty() {
            "Add provider".into()
        } else {
            self.provider_editor_source
                .as_ref()
                .map(|provider| provider.name.clone())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| kind.to_owned())
                .into()
        };
        let trail = if kind.is_empty() || saved_connection.is_some() {
            settings_ui::breadcrumb([back], title.clone())
        } else {
            let picker = settings_ui::crumb("provider-back-to-picker", "Add provider")
                .on_click(cx.listener(|view, _, window, cx| view.open_provider_picker(window, cx)));
            settings_ui::breadcrumb([back, picker], title.clone())
        };
        let mut page = settings_ui::settings_page("provider-editor", title)
            .description(provider_kind_description(kind))
            .breadcrumb(trail);
        if let Some(error) = &self.provider_discovery.page_error {
            page = page.child(settings_ui::inline_error(error));
        }
        if kind.is_empty() {
            return page.child(self.provider_picker(cx)).into_any_element();
        }
        page = if kind == "ChatGPT" {
            page.child(self.account_section(cx))
        } else {
            page.child(self.provider_connection_form(kind, cx))
        };
        page = page
            .child(self.provider_discovery_feedback(cx))
            .child(self.provider_model_list());
        if let Some(connection) = saved_connection {
            let busy = self.provider_discovery.loading();
            page = page.child(settings_ui::danger_zone([settings_ui::setting_row(
                "Remove provider",
                Some("Its models and saved key are removed from Xylo.".into()),
                Some(
                    settings_ui::destructive_button("provider-remove", "Remove")
                        .disabled(busy)
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.confirm_remove_provider(connection.clone(), window, cx)
                        }))
                        .into_any_element(),
                ),
            )
            .into_any_element()]));
        }
        page.into_any_element()
    }
    /// What a saved connection's row reports.
    fn provider_status(&self, provider: &ProviderConfig, models: usize) -> settings_ui::Status {
        let failed = matches!(
            self.provider_discovery.status,
            DiscoveryStatus::Failed(_) | DiscoveryStatus::SaveFailed(_)
        ) && self
            .provider_discovery
            .current
            .as_ref()
            .is_some_and(|(_, current)| same_connection(current, provider));
        connection_status(
            provider_kind(provider),
            provider.credential_ref.is_some(),
            models,
            failed,
            self.account_authorized(),
        )
    }
    fn provider_row(
        &self,
        index: usize,
        provider: ProviderConfig,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = same_connection(&provider, &self.settings.provider);
        let configure = provider.clone();
        let count = self
            .settings
            .providers
            .iter()
            .filter(|saved| !saved.model.is_empty() && same_connection(saved, &provider))
            .count();
        let models = if count == 1 {
            "1 model".to_owned()
        } else {
            format!("{count} models")
        };
        let detail = if active && !provider.model.is_empty() {
            format!("{models} · chats use {}", provider.model)
        } else {
            models
        };
        let kind = provider_kind(&provider);
        let status = self.provider_status(&provider, count);
        settings_ui::list_item(
            ("configure-provider", index),
            Some(settings_ui::monogram(kind, &kind.to_lowercase()).into_any_element()),
            provider.name.clone(),
            Some(detail.into()),
        )
        .trailing(settings_ui::status_pill(status))
        .on_click(cx.listener(move |view, _, window, cx| {
            view.provider_discovery.page_error = None;
            view.provider_form(&configure, window, cx);
            view.provider_form_open = true;
            if configure.credential_ref.as_deref() == Some(CHATGPT_REF) {
                view.account_command("account_chatgpt_status", cx);
            }
            cx.notify();
        }))
        .into_any_element()
    }
    pub(super) fn providers_page(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.provider_form_open {
            return self.provider_editor(cx);
        }
        let add = |id: &'static str, cx: &mut Context<Self>| {
            settings_ui::primary_button(id, "Add provider")
                .on_click(cx.listener(|view, _, window, cx| view.open_provider_picker(window, cx)))
        };
        let providers = saved_connections(&self.settings);
        let mut page = settings_ui::settings_page("provider-scroll", "Providers").description(
            "The model services Xylo can use. New models appear in the composer's model picker.",
        );
        if !providers.is_empty() {
            page = page.action(add("add-provider", cx));
        }
        if let Some(error) = &self.provider_discovery.page_error {
            page = page.child(settings_ui::inline_error(error));
        }
        if providers.is_empty() {
            page = page.child(settings_ui::empty_state(
                AppIcon::Model,
                "No providers yet",
                "Connect a model service to start a conversation.",
                Some(add("add-provider", cx).into_any_element()),
            ));
        } else {
            let rows: Vec<AnyElement> = providers
                .into_iter()
                .enumerate()
                .map(|(index, provider)| self.provider_row(index, provider, cx))
                .collect();
            page = page.child(settings_ui::list_container(rows));
        }
        page.into_any_element()
    }
}
#[derive(Default)]
pub(super) enum DiscoveryStatus {
    #[default]
    Idle,
    /// The host has not yet confirmed a connection save.
    Saving,
    Loading,
    Ready,
    Empty,
    /// The connection was not saved; the typed key stays in the form.
    SaveFailed(String),
    Failed(String),
    Cancelled,
}
/// Whether a catalog is persisted with its connection or only shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum DiscoveryPurpose {
    #[default]
    Catalog,
    /// "Test connection": the catalog stays in memory and nothing is saved.
    Test,
}
#[derive(Default)]
pub(super) struct ProviderDiscoveryUi {
    current: Option<(u64, ProviderConfig)>,
    status: DiscoveryStatus,
    purpose: DiscoveryPurpose,
    /// The connection being saved, carrying the credential reference its new
    /// key is stored under. It becomes the editor's connection only once the
    /// host confirms the save.
    saving: Option<ProviderConfig>,
    /// Entries from the last successful test, for picking a model of an
    /// already saved connection without saving the catalog.
    tested: Vec<ProviderConfig>,
    /// Failure of an action outside discovery (removing a provider), shown
    /// at the top of the page.
    page_error: Option<String>,
    /// One-line notice about a model switch made by the last catalog.
    notice: Option<String>,
    /// When the running discovery or save began, for the slow-wait caption.
    since: Option<Instant>,
    /// The wait whose slow-caption redraw is already scheduled. Saves start
    /// where no view context is at hand, so the first render of each wait
    /// schedules it instead.
    slow_armed: Cell<Option<Instant>>,
    #[cfg(test)]
    pub(super) test_dispatch: bool,
    #[cfg(test)]
    pub(super) test_generation: u64,
    #[cfg(test)]
    pub(super) test_requests: Vec<(u64, ProviderConfig)>,
}
impl ProviderDiscoveryUi {
    pub(super) fn failed(&self) -> bool {
        matches!(self.status, DiscoveryStatus::Failed(_))
    }
    pub(super) fn loading(&self) -> bool {
        matches!(
            self.status,
            DiscoveryStatus::Saving | DiscoveryStatus::Loading
        )
    }
    /// The connection whose save awaits the host, if any.
    pub(super) fn saving(&self) -> Option<&ProviderConfig> {
        self.saving.as_ref()
    }
}
impl Xylo {
    fn begin_provider_discovery(
        &mut self,
        provider: ProviderConfig,
        generation: u64,
        purpose: DiscoveryPurpose,
    ) {
        self.models.clear();
        self.model_context_limits.clear();
        self.provider_discovery.tested.clear();
        self.provider_discovery.page_error = None;
        self.provider_discovery.notice = None;
        self.provider_discovery.current = Some((generation, provider));
        self.provider_discovery.purpose = purpose;
        self.provider_discovery.since = Some(Instant::now());
        self.provider_discovery.status = if self.provider_discovery.saving.is_some() {
            DiscoveryStatus::Saving
        } else {
            DiscoveryStatus::Loading
        };
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
    fn dispatch_discovery(
        &mut self,
        provider: ProviderConfig,
        purpose: DiscoveryPurpose,
    ) -> anyhow::Result<()> {
        self.provider_discovery.saving = None;
        #[cfg(test)]
        if let Some(generation) = self.test_discovery_dispatch(&provider)? {
            self.begin_provider_discovery(provider, generation, purpose);
            return Ok(());
        }
        let host = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))?;
        host.discover_models(provider.clone())?;
        let generation = host.provider_discovery_generation();
        self.begin_provider_discovery(provider, generation, purpose);
        Ok(())
    }
    /// Loads and saves the catalog of a connection that needs no form save
    /// (the ChatGPT account).
    pub(super) fn request_provider_models(
        &mut self,
        provider: ProviderConfig,
    ) -> anyhow::Result<()> {
        self.dispatch_discovery(provider, DiscoveryPurpose::Catalog)
    }
    /// Reloads a saved connection's catalog; new models become available and
    /// vanished ones are dropped.
    pub(super) fn refresh_provider_models(&mut self, cx: &mut Context<Self>) {
        let result = (|| -> anyhow::Result<()> {
            anyhow::ensure!(
                self.secret.read(cx).value().trim().is_empty(),
                "Save the new API key first; saving also refreshes the models"
            );
            let provider = self.provider_from_form(cx)?;
            provider.validate_connection()?;
            self.dispatch_discovery(provider, DiscoveryPurpose::Catalog)
        })();
        if let Err(error) = result {
            self.provider_discovery.status = DiscoveryStatus::Failed(error.to_string());
        }
        cx.notify();
    }
    /// Runs discovery for the form's connection with its saved credentials,
    /// without saving anything.
    pub(super) fn test_provider_connection(&mut self, cx: &mut Context<Self>) {
        let result = (|| -> anyhow::Result<()> {
            anyhow::ensure!(
                self.secret.read(cx).value().trim().is_empty(),
                "Save the new API key before testing this connection"
            );
            let provider = self.provider_from_form(cx)?;
            provider.validate_connection()?;
            self.dispatch_discovery(provider, DiscoveryPurpose::Test)
        })();
        if let Err(error) = result {
            self.provider_discovery.status = DiscoveryStatus::Failed(error.to_string());
        }
        cx.notify();
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
            self.provider_discovery.saving = Some(provider.clone());
            self.begin_provider_discovery(provider, generation, DiscoveryPurpose::Catalog);
            return Ok(());
        }
        let host = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))?;
        host.save_provider_connection(settings, provider.clone(), secret)?;
        let generation = host.provider_discovery_generation();
        self.provider_discovery.saving = Some(provider.clone());
        self.begin_provider_discovery(provider, generation, DiscoveryPurpose::Catalog);
        Ok(())
    }
    /// Shows a provider save failure inline in the form.
    pub(super) fn provider_save_failed(&mut self, error: &anyhow::Error) {
        self.provider_discovery.saving = None;
        self.provider_discovery.status = DiscoveryStatus::SaveFailed(error.to_string());
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
        // The host runs discovery only after it stored the secret and settings.
        if let Some(saved) = self.provider_discovery.saving.take() {
            self.provider_editor_source = Some(saved);
        }
        self.provider_discovery.since = None;
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
        self.provider_discovery.since = None;
        self.provider_discovery.status = match self.provider_discovery.saving.take() {
            // Settings arrive before discovery starts: a stored connection
            // means only discovery failed.
            Some(saved)
                if self.settings.providers.iter().any(|stored| {
                    same_connection(stored, &saved) && stored.model == saved.model
                }) =>
            {
                self.provider_editor_source = Some(saved);
                DiscoveryStatus::Failed(message.into())
            }
            Some(_) => DiscoveryStatus::SaveFailed(message.into()),
            None => DiscoveryStatus::Failed(message.into()),
        };
        true
    }
    pub(super) fn cancel_provider_discovery(&mut self, cx: &mut Context<Self>) {
        let mut failure = None;
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
                failure = Some(error.to_string());
            }
        }
        self.models.clear();
        self.model_context_limits.clear();
        self.provider_discovery.current = None;
        self.provider_discovery.saving = None;
        self.provider_discovery.since = None;
        self.provider_discovery.tested.clear();
        self.provider_discovery.status = match failure {
            Some(error) => DiscoveryStatus::Failed(error),
            None => DiscoveryStatus::Cancelled,
        };
        cx.notify();
    }
    pub(super) fn reset_provider_discovery(&mut self, cx: &mut Context<Self>) {
        self.cancel_provider_discovery(cx);
        self.provider_discovery.status = DiscoveryStatus::Idle;
    }
    /// The slow-wait caption for the running discovery or save. The first
    /// render of each wait schedules the redraw that shows it on time.
    fn discovery_slow_caption(&self, cx: &mut Context<Self>) -> Option<Div> {
        let since = self
            .provider_discovery
            .since
            .filter(|_| self.provider_discovery.loading())?;
        if self.provider_discovery.slow_armed.get() != Some(since) {
            self.provider_discovery.slow_armed.set(Some(since));
            let delay = (since + motion::SLOW_AFTER).saturating_duration_since(Instant::now())
                + std::time::Duration::from_millis(50);
            cx.spawn(async move |view, cx| {
                cx.background_executor().timer(delay).await;
                let _ = view.update(cx, |_, cx| cx.notify());
            })
            .detach();
        }
        settings_ui::slow_caption(since)
    }
    pub(super) fn provider_discovery_feedback(&self, cx: &mut Context<Self>) -> AnyElement {
        let chatgpt = self.provider_editor_kind == "ChatGPT";
        let (message, tone, retry, cancel) = match &self.provider_discovery.status {
            DiscoveryStatus::Idle => return div().into_any_element(),
            DiscoveryStatus::Ready => {
                if self.provider_discovery.purpose == DiscoveryPurpose::Test {
                    (
                        "Connection works.".to_owned(),
                        BadgeTone::Success,
                        false,
                        false,
                    )
                } else if let Some(notice) = &self.provider_discovery.notice {
                    (notice.clone(), BadgeTone::Neutral, false, false)
                } else {
                    return div().into_any_element();
                }
            }
            DiscoveryStatus::Saving => (
                "Saving provider and loading models…".into(),
                BadgeTone::Neutral,
                false,
                true,
            ),
            DiscoveryStatus::Loading => ("Loading models…".into(), BadgeTone::Neutral, false, true),
            DiscoveryStatus::Empty => (
                if chatgpt {
                    "No authorized models were returned. Retry or choose another provider.".into()
                } else {
                    "No models were returned. Retry or enter a model ID.".into()
                },
                BadgeTone::Neutral,
                true,
                false,
            ),
            DiscoveryStatus::SaveFailed(error) => (
                format!(
                    "Provider not saved: {} Your API key is still in the form.",
                    product_error(error)
                ),
                BadgeTone::Danger,
                true,
                false,
            ),
            DiscoveryStatus::Failed(error) => (
                format!("Could not load models: {}", product_error(error)),
                BadgeTone::Danger,
                true,
                false,
            ),
            DiscoveryStatus::Cancelled => (
                "Model discovery cancelled.".into(),
                BadgeTone::Neutral,
                true,
                false,
            ),
        };
        let slow = self.discovery_slow_caption(cx);
        div()
            .id("provider-discovery-feedback")
            .flex()
            .flex_col()
            .gap_2()
            .child(motion::fade_on_change(
                "provider-discovery-status",
                &message,
                notice(tone, message.clone()),
            ))
            .children(slow)
            .child(
                div()
                    .flex()
                    .gap_2()
                    .when(retry, |row| {
                        row.child(
                            settings_ui::secondary_button("provider-retry", "Retry").on_click(
                                cx.listener(|view, _, window, cx| {
                                    if view.provider_editor_kind == "ChatGPT" {
                                        view.use_chatgpt(window, cx);
                                    } else if view.provider_discovery.purpose
                                        == DiscoveryPurpose::Test
                                        && !matches!(
                                            view.provider_discovery.status,
                                            DiscoveryStatus::SaveFailed(_)
                                        )
                                    {
                                        view.test_provider_connection(cx);
                                    } else {
                                        view.save_provider(window, cx);
                                    }
                                }),
                            ),
                        )
                    })
                    .when(cancel, |row| {
                        row.child(
                            settings_ui::secondary_button("provider-cancel-discovery", "Cancel")
                                .on_click(
                                    cx.listener(|view, _, _, cx| {
                                        view.cancel_provider_discovery(cx)
                                    }),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }
    /// The connection's models as the editor shows them: the last test's
    /// catalog, otherwise the saved entries of the edited connection.
    fn connection_models(&self) -> Vec<ProviderConfig> {
        if self.provider_discovery.purpose == DiscoveryPurpose::Test
            && !self.provider_discovery.tested.is_empty()
        {
            return self.provider_discovery.tested.clone();
        }
        let Some(source) = &self.provider_editor_source else {
            return Vec::new();
        };
        self.settings
            .providers
            .iter()
            .filter(|provider| !provider.model.is_empty() && same_connection(provider, source))
            .cloned()
            .collect()
    }
    /// Read-only list of the connection's models; choosing happens in the
    /// model picker.
    pub(super) fn provider_model_list(&self) -> AnyElement {
        if self.provider_editor_kind == "ChatGPT" && !self.account_can_load_models() {
            return div().into_any_element();
        }
        if self.provider_discovery.loading() {
            return div()
                .id("provider-model-list")
                .w_full()
                .child(settings_ui::section(
                    "Models",
                    Some("Loading this connection's models…".into()),
                    // The caption is drawn by the discovery feedback above.
                    settings_ui::loading_list(3, None),
                ))
                .into_any_element();
        }
        let models = self.connection_models();
        if models.is_empty() {
            return div().into_any_element();
        }
        let count = models.len();
        // Every row is laid out (the card grows), so no model is clipped.
        let rows = uniform_list("available-models", count, move |range, _, _| {
            range
                .filter_map(|index| {
                    let model = models.get(index)?;
                    Some(
                        div()
                            .id(("model", index))
                            .test_support()
                            .w_full()
                            .h(px(MODEL_ROW))
                            .px(px(settings_ui::LIST_PADDING_X))
                            .flex()
                            .items_center()
                            .gap_3()
                            .when(index + 1 < count, |row| {
                                row.border_b_1().border_color(rgb(border()))
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(13.5))
                                    .text_color(rgb(text()))
                                    .single_line()
                                    .child(model.model.clone()),
                            )
                            .child(
                                settings_ui::caption(context_size(model.context_tokens))
                                    .flex_shrink_0(),
                            )
                            .into_any_element(),
                    )
                })
                .collect()
        })
        .track_scroll(&self.model_scroll)
        .w_full()
        .h(px(count as f32 * MODEL_ROW));
        let tested = self.provider_discovery.purpose == DiscoveryPurpose::Test
            && !self.provider_discovery.tested.is_empty();
        div()
            .id("provider-model-list")
            .w_full()
            .child(settings_ui::section(
                format!("Models · {count}"),
                Some(
                    if tested {
                        "Offered by this connection. Save the provider to make them available."
                    } else {
                        "Pick the model for a chat from the model picker in the composer."
                    }
                    .into(),
                ),
                settings_ui::list_frame().child(rows),
            ))
            .into_any_element()
    }
}

/// Height of one row in a connection's model list: compact, since the rows
/// are read-only names.
const MODEL_ROW: f32 = 40.;
/// Context window size in product units ("128K context").
fn context_size(tokens: u32) -> String {
    if tokens >= 1_000_000 && tokens.is_multiple_of(1_000_000) {
        format!("{}M context", tokens / 1_000_000)
    } else if tokens >= 1_000 {
        format!("{}K context", tokens / 1_000)
    } else {
        format!("{tokens} tokens context")
    }
}

impl Xylo {
    /// Reconciles one accepted catalog in bulk. Rendering never walks this
    /// registry. A saved catalog makes every offered model available and
    /// drops the connection's models that are gone; a test catalog is only
    /// kept in memory. See [`apply_catalog`] for when the active model changes.
    // The arguments mirror `HostEvent::Models`, which the event loop forwards.
    #[allow(clippy::too_many_arguments)]
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
        // A catalog answer means any key in the form is stored.
        Self::set_input(&self.secret, "", window, cx);
        let mut seen = BTreeSet::new();
        let models: Vec<&String> = catalog
            .iter()
            .filter(|model| !model.trim().is_empty() && seen.insert(model.as_str()))
            .collect();
        if models.is_empty() {
            return Ok(());
        }
        let registered = catalog_entries(
            source,
            &self.settings.providers,
            &models,
            context_limits,
            reasoning_options,
            default_reasoning_efforts,
        );
        if self.provider_discovery.purpose == DiscoveryPurpose::Test {
            self.provider_discovery.tested = registered;
            return Ok(());
        }
        let mut settings = self.settings.clone();
        let activation = apply_catalog(&mut settings, source, registered.clone());
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
        if activation != Activation::Kept
            && settings.provider.credential_ref.as_deref() == Some(CHATGPT_REF)
        {
            self.account_model_selected();
        }
        self.provider_discovery.notice = match activation {
            Activation::Replaced { previous } => Some(format!(
                "{previous} is no longer offered; chats now use {}.",
                settings.provider.model
            )),
            Activation::First => Some(format!("Chats now use {}.", settings.provider.model)),
            Activation::Kept => None,
        };
        let shown = if same_connection(&settings.provider, source) {
            settings.provider.clone()
        } else {
            registered[0].clone()
        };
        self.settings = settings;
        self.provider_editor_source = Some(shown.clone());
        Self::set_input(&self.model, shown.model.clone(), window, cx);
        Self::set_input(&self.context, shown.context_tokens.to_string(), window, cx);
        Self::set_input(&self.output, shown.output_tokens.to_string(), window, cx);
        Ok(())
    }
}

#[cfg(test)]
#[path = "providers/tests.rs"]
mod tests;
