//! Account identity is distinct from authorization and usable model access.
//! Tokens stay in the host; each visible action is UUID correlated.
use super::*;
use crate::motion;
use serde_json::{Value, json};
use std::time::Instant;
pub(super) const CHATGPT_REF: &str = "account:chatgpt";
#[derive(Default)]
pub(super) struct AccountUi {
    connected: bool,
    signed_out: bool,
    plan_authorized: bool,
    /// Whether the account status was read this session (or the account was
    /// signed out here); until then `connected` is only a default.
    status_known: bool,
    email: Option<String>,
    pending: Option<(Uuid, String)>,
    /// When the pending command was sent, for the slow-wait caption.
    pending_since: Option<Instant>,
    notice: Option<String>,
}
impl Xylo {
    pub(super) fn account_command(&mut self, name: &str, cx: &mut Context<Self>) {
        self.cancel_account_command(cx);
        if name == "account_chatgpt_sign_in" {
            self.reset_provider_discovery(cx);
        }
        if name == "account_chatgpt_sign_out" {
            self.cancel_provider_discovery(cx);
        }
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| {
                host.capability(id, name.into(), json!({"credential_ref":CHATGPT_REF}))
            });
        match result {
            Ok(()) => {
                self.account.pending = Some((id, name.into()));
                self.account.pending_since = Some(self.arm_slow_hint(cx));
                self.account.notice = None;
            }
            Err(error) => self.account.notice = Some(error.to_string()),
        }
        cx.notify();
    }
    pub(super) fn cancel_account_command(&mut self, cx: &mut Context<Self>) {
        self.account.pending_since = None;
        if let Some((id, _)) = self.account.pending.take()
            && let Some(host) = &self.host
            && let Err(error) = host.cancel_capability(id)
        {
            self.account.notice = Some(error.to_string());
        }
        cx.notify();
    }
    /// Returns true for account responses, including superseded/cancelled ones.
    pub(super) fn account_capability_result(
        &mut self,
        id: Uuid,
        name: &str,
        result: &Result<Value, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !name.starts_with("account_chatgpt_") {
            return false;
        }
        if self
            .account
            .pending
            .as_ref()
            .is_none_or(|(expected, command)| *expected != id || command != name)
        {
            return true;
        }
        self.account.pending = None;
        self.account.pending_since = None;
        match result {
            Ok(value) => {
                let summary = value
                    .get("summary")
                    .and_then(Value::as_str)
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .unwrap_or_else(|| value.clone());
                self.account_result(name, &summary, window, cx);
            }
            Err(error) => {
                self.account.notice = Some(product_error(error));
                cx.notify();
            }
        }
        true
    }
    pub(super) fn account_result(
        &mut self,
        name: &str,
        value: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match name {
            "account_chatgpt_sign_in" | "account_chatgpt_status" => {
                self.account.connected = value
                    .get("connected")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.account.plan_authorized = value
                    .get("plan_usage_authorized")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.account.email = value
                    .get("email")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.account.status_known = true;
                self.account.notice = None;
                if !self.account.connected || !self.account.plan_authorized {
                    if self.provider_editor_kind == "ChatGPT" {
                        self.reset_provider_discovery(cx);
                    }
                } else if (name == "account_chatgpt_sign_in"
                    || !self
                        .settings
                        .providers
                        .iter()
                        .any(|provider| provider.credential_ref.as_deref() == Some(CHATGPT_REF)))
                    && self.provider_editor_kind == "ChatGPT"
                    && self.provider_form_open
                    && self.page == Page::Providers
                {
                    self.use_chatgpt(window, cx);
                }
            }
            "account_chatgpt_sign_out" => {
                self.account = AccountUi::default();
                self.account.signed_out = true;
                self.account.status_known = true;
                self.account.notice = Some(
                    if value
                        .get("warning")
                        .is_some_and(|warning| !warning.is_null())
                    {
                        "Disconnected locally. Remote sign-out could not be confirmed.".into()
                    } else {
                        "Signed out.".into()
                    },
                );
                self.settings
                    .providers
                    .retain(|provider| provider.credential_ref.as_deref() != Some(CHATGPT_REF));
                if self.settings.provider.credential_ref.as_deref() == Some(CHATGPT_REF) {
                    self.settings.provider = ProviderConfig::default();
                }
                let settings = self.settings.clone();
                self.host_call(cx, |host| host.save_settings(settings));
                self.reset_provider_discovery(cx);
                let provider = ProviderConfig {
                    name: "ChatGPT".into(),
                    family: ApiFamily::OpenAiResponses,
                    endpoint: "https://api.openai.com/v1".into(),
                    credential_ref: Some(CHATGPT_REF.into()),
                    ..Default::default()
                };
                self.provider_form(&provider, window, cx);
            }
            _ => return false,
        }
        cx.notify();
        true
    }
    pub(super) fn use_chatgpt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.account.connected {
            self.account.notice = Some("Sign in to load your models.".into());
            cx.notify();
            return;
        }
        if !self.account.plan_authorized {
            self.account.notice = Some("Authorize plan access before loading models.".into());
            cx.notify();
            return;
        }
        if self.provider_discovery.loading() {
            return;
        }
        let provider = ProviderConfig {
            name: "ChatGPT".into(),
            family: ApiFamily::OpenAiResponses,
            endpoint: "https://api.openai.com/v1".into(),
            credential_ref: Some(CHATGPT_REF.into()),
            ..Default::default()
        };
        self.provider_form(&provider, window, cx);
        self.account.notice = None;
        if let Err(error) = self.request_provider_models(provider) {
            self.account.notice = Some(error.to_string());
        }
        cx.notify();
    }
    /// Ignore late configuration acknowledgements that resurrect a signed-out
    /// account. Corrected acknowledgements contain no account refs, so resaving
    /// this sanitized state cannot echo indefinitely.
    pub(super) fn account_sanitize_settings(&self, settings: &mut AppSettings) -> bool {
        if !self.account.signed_out {
            return false;
        }
        let before = settings.providers.len();
        settings
            .providers
            .retain(|provider| provider.credential_ref.as_deref() != Some(CHATGPT_REF));
        let active = settings.provider.credential_ref.as_deref() == Some(CHATGPT_REF);
        if active {
            settings.provider = ProviderConfig::default();
        }
        active || before != settings.providers.len()
    }
    pub(super) fn account_model_selected(&mut self) {
        self.account.signed_out = false;
    }
    /// The signed-in ChatGPT account's email, when the account reported one.
    pub(super) fn account_email(&self) -> Option<&str> {
        self.account
            .email
            .as_deref()
            .filter(|_| self.account.connected)
    }
    pub(super) fn account_can_load_models(&self) -> bool {
        self.account.connected && self.account.plan_authorized
    }
    /// Whether the ChatGPT account may load models, or `None` before its
    /// status was read this session.
    pub(super) fn account_authorized(&self) -> Option<bool> {
        self.account
            .status_known
            .then(|| self.account_can_load_models())
    }
    /// The ChatGPT account as a settings section: who is signed in, the
    /// account actions, and any request in flight.
    pub(super) fn account_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut description: SharedString = if self.account.connected {
            self.account
                .email
                .clone()
                .unwrap_or_else(|| "Signed in".into())
                .into()
        } else {
            "Not signed in".into()
        };
        let mut actions = div().flex().items_center().gap(px(8.));
        if let Some((_, command)) = &self.account.pending {
            description = match command.as_str() {
                "account_chatgpt_sign_in" => "Finish signing in through your browser…",
                "account_chatgpt_sign_out" => "Signing out…",
                _ => "Checking sign-in…",
            }
            .into();
            actions = actions.child(motion::spinner_inline()).child(
                settings_ui::secondary_button("chatgpt-cancel-sign-in", "Cancel").on_click(
                    cx.listener(|view, _, _, cx| {
                        view.cancel_account_command(cx);
                        view.account.notice = Some("Account request cancelled.".into());
                        cx.notify();
                    }),
                ),
            );
        } else if !self.account.connected {
            actions = actions.child(
                settings_ui::primary_button("chatgpt-sign-in", "Sign in").on_click(cx.listener(
                    |view, _, _, cx| view.account_command("account_chatgpt_sign_in", cx),
                )),
            );
        } else {
            if self.provider_discovery.failed() {
                actions = actions.child(
                    settings_ui::secondary_button("chatgpt-reconnect", "Reconnect account")
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.account_command("account_chatgpt_sign_in", cx)
                        })),
                );
            }
            if self.account.plan_authorized {
                actions = actions.child(settings_ui::busy(
                    settings_ui::secondary_button("chatgpt-refresh-models", "Refresh models")
                        .on_click(cx.listener(|view, _, window, cx| view.use_chatgpt(window, cx))),
                    self.provider_discovery.loading(),
                ));
            } else {
                description = format!(
                    "{description} · Your identity is linked. Plan access has not been authorized."
                )
                .into();
                actions = actions.child(
                    settings_ui::primary_button("chatgpt-authorize-plan", "Authorize plan access")
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.account_command("account_chatgpt_sign_in", cx)
                        })),
                );
            }
            actions = actions.child(
                settings_ui::text_button("chatgpt-sign-out", "Sign out").on_click(cx.listener(
                    |view, _, _, cx| view.account_command("account_chatgpt_sign_out", cx),
                )),
            );
        }
        let mut rows = vec![
            settings_ui::setting_row(
                "ChatGPT account",
                Some(description),
                Some(actions.into_any_element()),
            )
            .feedback(
                self.account
                    .pending_since
                    .filter(|since| {
                        self.account.pending.is_some() && motion::is_slow(*since, Instant::now())
                    })
                    .map(|_| settings_ui::Feedback::Pending(motion::SLOW_HINT.into())),
            )
            .into_any_element(),
        ];
        if let Some(notice) = &self.account.notice {
            rows.push(
                div()
                    .id("account-notice")
                    .py(px(10.))
                    .child(settings_ui::caption(notice.clone()))
                    .into_any_element(),
            );
        }
        settings_ui::section("Account", None, settings_ui::rows(rows)).into_any_element()
    }
}

#[cfg(test)]
#[allow(dead_code)] // Exercised by the separate native validation test target.
impl AccountUi {
    pub(super) fn validation_pending(&mut self, id: Uuid, name: &str) {
        self.pending = Some((id, name.into()));
    }
}
