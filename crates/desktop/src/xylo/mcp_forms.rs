//! Inline, user-owned MCP forms. Fields are never filled by the model.
use super::*;
use anyhow::Context as _;
use xylo_core::{McpFormAction, McpFormRequest};

pub(super) struct Form {
    session: Uuid,
    request: McpFormRequest,
    inputs: BTreeMap<String, Entity<InputState>>,
    error: Option<String>,
}
impl Xylo {
    pub(super) fn mcp_form_open(
        &mut self,
        session: Uuid,
        request: McpFormRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let inputs = request.schema["properties"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, schema)| {
                let placeholder = if let Some(values) = schema["enum"].as_array() {
                    values
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" / ")
                } else {
                    schema["type"].as_str().unwrap_or("Text").to_owned()
                };
                (
                    name.clone(),
                    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder)),
                )
            })
            .collect();
        self.mcp_forms.insert(
            request.id,
            Form {
                session,
                request,
                inputs,
                error: None,
            },
        );
        cx.notify();
    }
    pub(super) fn mcp_form_cards(&self, cx: &mut Context<Self>) -> AnyElement {
        let cards = self.mcp_forms.iter()
            .filter(|(_, form)| form.session == self.session)
            .map(|(id, form)| {
                let id = *id;
                let mut card = div().p_4().border_1().border_color(rgb(border()))
                    .rounded_lg().bg(rgb(panel())).flex().flex_col().gap_2()
                    .child(format!("{} is asking for information", form.request.server_name))
                    .child(form.request.message.clone())
                    .child("Only share information you want sent to this server. Do not enter passwords or access tokens.");
                for (name, input) in &form.inputs {
                    let property = &form.request.schema["properties"][name];
                    let title = property["title"].as_str().unwrap_or(name);
                    let help = property["description"].as_str().map(|text| text.to_owned().into());
                    card = card.child(settings_ui::field(title.to_owned(), Input::new(input).aria_label(title.to_owned()), help));
                }
                if let Some(error) = &form.error {
                    card = card.child(settings_ui::inline_error(error));
                }
                let submit = settings_ui::primary_button(
                    SharedString::from(format!("mcp-submit-{id}")), "Send response",
                ).on_click(cx.listener(move |view, _, _, cx| {
                    view.mcp_form_answer(id, McpFormAction::Accept, cx)
                }));
                let decline = settings_ui::secondary_button(
                    SharedString::from(format!("mcp-decline-{id}")), "Decline",
                ).on_click(cx.listener(move |view, _, _, cx| {
                    view.mcp_form_answer(id, McpFormAction::Decline, cx)
                }));
                let cancel = settings_ui::text_button(
                    SharedString::from(format!("mcp-cancel-{id}")), "Cancel",
                ).on_click(cx.listener(move |view, _, _, cx| {
                    view.mcp_form_answer(id, McpFormAction::Cancel, cx)
                }));
                card.child(div().flex().gap_2().child(submit).child(decline).child(cancel))
                    .into_any_element()
            });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_6()
            .children(cards)
            .into_any_element()
    }
    fn mcp_form_answer(&mut self, id: Uuid, action: McpFormAction, cx: &mut Context<Self>) {
        let Some(form) = self.mcp_forms.get_mut(&id) else {
            return;
        };
        let mut values = serde_json::Map::new();
        if action == McpFormAction::Accept {
            for (name, input) in &form.inputs {
                let text = input.read(cx).value().to_string();
                let required = form.request.schema["required"]
                    .as_array()
                    .is_some_and(|required| required.iter().any(|value| value == name));
                if text.is_empty() && !required {
                    continue;
                }
                let schema = &form.request.schema["properties"][name];
                let value = match schema["type"].as_str() {
                    Some("string") => serde_json::Value::String(text),
                    _ => match serde_json::from_str(&text) {
                        Ok(value) => value,
                        Err(_) => {
                            form.error = Some(format!(
                                "Enter a valid {} for {name}",
                                schema["type"].as_str().unwrap_or("value")
                            ));
                            cx.notify();
                            return;
                        }
                    },
                };
                values.insert(name.clone(), value);
            }
            if let Err(error) = xylo_core::validate_mcp_form(
                &form.request.schema,
                &serde_json::Value::Object(values.clone()),
            ) {
                form.error = Some(error);
                cx.notify();
                return;
            }
        }
        let session = form.session;
        let content =
            (action == McpFormAction::Accept).then_some(serde_json::Value::Object(values));
        match self
            .host
            .as_ref()
            .context("Application service unavailable")
            .and_then(|host| {
                host.send_to(
                    session,
                    WorkerCommand::RespondMcpElicitation {
                        request_id: id,
                        action,
                        content,
                    },
                )
            }) {
            Ok(()) => {
                self.mcp_forms.remove(&id);
            }
            Err(error) => {
                if let Some(form) = self.mcp_forms.get_mut(&id) {
                    form.error = Some(error.to_string());
                }
            }
        }
        cx.notify();
    }
}
