//! Browser sign-in stays in the host. The UI receives account status, never an
//! access token, refresh token, verifier or authorization callback URL.
use super::*;
use serde_json::{Value, json};
pub(super) const CHATGPT_REF: &str = "account:chatgpt";
#[derive(Default)]
pub(super) struct AccountUi {
    connected: bool,
    email: Option<String>,
}
impl Workbench {
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
                    .unwrap_or(false)
                    && value
                        .get("plan_usage_authorized")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                self.account.email = value
                    .get("email")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if name == "account_chatgpt_sign_in" && self.account.connected {
                    self.use_chatgpt(window, cx);
                    self.native.message = "Account connected. Choose a model to start.".into();
                }
            }
            "account_chatgpt_sign_out" => {
                self.account = AccountUi::default();
                self.native.message = if value
                    .get("warning")
                    .is_some_and(|warning| !warning.is_null())
                {
                    "Your local connection was removed. Remote sign-out could not be confirmed. You can disconnect this app in ChatGPT Settings."
                        .into()
                } else {
                    "Signed out.".into()
                };
                if self.settings.provider.credential_ref.as_deref() == Some(CHATGPT_REF) {
                    self.settings.provider.credential_ref = None;
                    let settings = self.settings.clone();
                    self.host_call(cx, |host| host.save_settings(settings));
                }
            }
            _ => return false,
        }
        cx.notify();
        true
    }
    fn use_chatgpt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let provider = ProviderConfig {
            name: "ChatGPT".into(),
            family: ApiFamily::OpenAiResponses,
            endpoint: "https://api.openai.com/v1".into(),
            credential_ref: Some(CHATGPT_REF.into()),
            ..Default::default()
        };
        self.settings.provider = provider.clone();
        self.provider_form(&provider, window, cx);
        let settings = self.settings.clone();
        self.host_call(cx, |host| host.save_settings(settings));
        self.host_call(cx, |host| host.discover_models(provider));
    }
    pub(super) fn account_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut card = card().max_w(px(820.)).child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .child("ChatGPT account"),
        );
        let pending = self.native.pending_id("Waiting for browser sign-in");
        if let Some(id) = pending {
            return card
                .child(subtitle("Finish signing in through your browser."))
                .child(
                    button("chatgpt-cancel-sign-in", "Cancel sign-in").on_click(cx.listener(
                        move |view, _, _, cx| view.host_call(cx, |host| host.cancel_capability(id)),
                    )),
                )
                .into_any_element();
        }
        if self.account.connected {
            card = card
                .child(subtitle(
                    self.account
                        .email
                        .as_ref()
                        .map(|email| format!("Connected as {email}"))
                        .unwrap_or_else(|| "Connected".into()),
                ))
                .child(
                    primary("chatgpt-use-account", "Use account")
                        .on_click(cx.listener(|view, _, window, cx| view.use_chatgpt(window, cx))),
                )
                .child(button("chatgpt-sign-out", "Sign out").on_click(cx.listener(
                    |view, _, _, cx| {
                        view.capability(
                            "account_chatgpt_sign_out",
                            json!({"credential_ref":CHATGPT_REF}),
                            "Signing out",
                            None,
                            cx,
                        );
                    },
                )));
        } else {
            card=card.child(subtitle("Sign in through your browser. Available models depend on the access your account authorizes."))
                .child(primary("chatgpt-sign-in","Continue with ChatGPT").on_click(cx.listener(|view,_,_,cx| {
                    view.capability("account_chatgpt_sign_in",json!({"credential_ref":CHATGPT_REF}),"Waiting for browser sign-in",None,cx);
                })));
        }
        card.into_any_element()
    }
}
