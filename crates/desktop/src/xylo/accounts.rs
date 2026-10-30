//! Account identity is distinct from authorization and usable model access.
//! Tokens stay in the host; each visible action is UUID correlated.
use super::*;
use serde_json::{Value, json};
pub(super) const CHATGPT_REF: &str = "account:chatgpt";
#[derive(Default)]
pub(super) struct AccountUi {
    connected: bool,
    signed_out: bool,
    plan_authorized: bool,
    email: Option<String>,
    pending: Option<(Uuid, String)>,
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
                self.account.notice = None;
            }
            Err(error) => self.account.notice = Some(error.to_string()),
        }
        cx.notify();
    }
    pub(super) fn cancel_account_command(&mut self, cx: &mut Context<Self>) {
        if let Some((id, _)) = self.account.pending.take() {
            if let Some(host) = &self.host {
                if let Err(error) = host.cancel_capability(id) {
                    self.account.notice = Some(error.to_string());
                }
            }
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
                self.account.notice = None;
                if !self.account.connected || !self.account.plan_authorized {
                    if self.provider_editor_kind == "ChatGPT" {
                        self.reset_provider_discovery(cx);
                    }
                } else if name == "account_chatgpt_sign_in"
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
    pub(super) fn account_can_load_models(&self) -> bool {
        self.account.connected && self.account.plan_authorized
    }
    pub(super) fn account_can_select_model(&self, model: &str) -> bool {
        self.account.connected
            && self.account.plan_authorized
            && self.provider_discovery.ready()
            && self.models.iter().any(|candidate| candidate == model)
    }
    pub(super) fn account_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut section = div().flex().flex_col().gap_3();
        let identity = if self.account.connected {
            self.account
                .email
                .clone()
                .unwrap_or_else(|| "Signed in".into())
        } else {
            "Not signed in".into()
        };
        let mut row = div()
            .flex()
            .items_center()
            .gap_3()
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(rgb(border()))
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
                            .child("Account"),
                    )
                    .child(subtitle(identity)),
            );
        if let Some((_, command)) = &self.account.pending {
            let label = match command.as_str() {
                "account_chatgpt_sign_in" => "Finish signing in through your browser…",
                "account_chatgpt_sign_out" => "Signing out…",
                _ => "Checking sign-in…",
            };
            row = row.child(subtitle(label)).child(
                button("chatgpt-cancel-sign-in", "Cancel").on_click(cx.listener(
                    |view, _, _, cx| {
                        view.cancel_account_command(cx);
                        view.account.notice = Some("Account request cancelled.".into());
                        cx.notify();
                    },
                )),
            );
        } else if !self.account.connected {
            row = row.child(primary("chatgpt-sign-in", "Sign in").on_click(
                cx.listener(|view, _, _, cx| view.account_command("account_chatgpt_sign_in", cx)),
            ));
        } else {
            if self.account.plan_authorized && !self.provider_discovery.retry_state() {
                row = row.child(
                    primary(
                        "chatgpt-use-account",
                        if self.models.is_empty() {
                            "Use account"
                        } else {
                            "Refresh models"
                        },
                    )
                    .disabled(self.provider_discovery.loading())
                    .on_click(cx.listener(|view, _, window, cx| view.use_chatgpt(window, cx))),
                );
            } else if !self.account.plan_authorized {
                row = row.child(
                    primary("chatgpt-authorize-plan", "Authorize plan access").on_click(
                        cx.listener(|view, _, _, cx| {
                            view.account_command("account_chatgpt_sign_in", cx)
                        }),
                    ),
                );
            }
            row = row.child(button("chatgpt-sign-out", "Sign out").on_click(
                cx.listener(|view, _, _, cx| view.account_command("account_chatgpt_sign_out", cx)),
            ));
        }
        section = section.child(row);
        if self.account.connected && !self.account.plan_authorized {
            section = section.child(subtitle(
                "Your identity is linked. Plan access has not been authorized.",
            ));
        }
        if self.provider_discovery.failed() && self.account.connected {
            section = section.child(button("chatgpt-reconnect", "Reconnect account").on_click(
                cx.listener(|view, _, _, cx| view.account_command("account_chatgpt_sign_in", cx)),
            ));
        }
        if let Some(notice) = &self.account.notice {
            section = section.child(div().id("account-notice").text_sm().child(notice.clone()));
        }
        section.into_any_element()
    }
}

#[cfg(test)]
impl AccountUi {
    pub(super) fn validation_pending(&mut self, id: Uuid, name: &str) {
        self.pending = Some((id, name.into()));
    }
}
