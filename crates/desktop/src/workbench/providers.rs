use super::*;

impl Workbench {
    pub(super) fn providers_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut page = div()
            .id("provider-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(heading(
                "Model",
                "Add a model provider to start a conversation.",
            ));
        page = page.child(
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .child(
                    button("preset-local", "Local / Ollama")
                        .border_1()
                        .border_color(rgb(border()))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.preset(
                                "Local model",
                                "http://127.0.0.1:11434/v1",
                                ApiFamily::OpenAiChat,
                                window,
                                cx,
                            )
                        })),
                )
                .child(
                    button("preset-openai", "Custom provider")
                        .border_1()
                        .border_color(rgb(border()))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.preset(
                                "Custom provider",
                                "https://api.openai.com/v1",
                                ApiFamily::OpenAiChat,
                                window,
                                cx,
                            )
                        })),
                )
                .child(
                    button("preset-anthropic", "Anthropic")
                        .border_1()
                        .border_color(rgb(border()))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.preset(
                                "Anthropic",
                                "https://api.anthropic.com",
                                ApiFamily::Anthropic,
                                window,
                                cx,
                            )
                        })),
                )
                .child(
                    button("preset-gemini", "Gemini")
                        .border_1()
                        .border_color(rgb(border()))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.preset(
                                "Gemini",
                                "https://generativelanguage.googleapis.com",
                                ApiFamily::Gemini,
                                window,
                                cx,
                            )
                        })),
                ),
        );
        let mut local_presets = div().flex().flex_wrap().gap_2();
        for (id, name, endpoint) in [
            ("preset-lmstudio", "LM Studio", "http://127.0.0.1:1234/v1"),
            ("preset-llama", "llama.cpp", "http://127.0.0.1:8080/v1"),
        ] {
            local_presets = local_presets.child(
                button(id, name)
                    .border_1()
                    .border_color(rgb(border()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.preset(name, endpoint, ApiFamily::OpenAiChat, window, cx)
                    })),
            );
        }
        page = page.child(local_presets).child(self.account_card(cx));
        let mut cloud = div().flex().flex_wrap().gap_2();
        for (id, name, endpoint, family) in [
            (
                "preset-openai-responses",
                "OpenAI",
                "https://api.openai.com/v1",
                ApiFamily::OpenAiResponses,
            ),
            (
                "preset-azure",
                "Azure OpenAI",
                "https://YOUR-RESOURCE.openai.azure.com/openai/v1",
                ApiFamily::Azure,
            ),
            (
                "preset-vertex",
                "Vertex AI",
                "https://LOCATION-aiplatform.googleapis.com/v1/projects/PROJECT/locations/LOCATION/publishers/google",
                ApiFamily::Vertex,
            ),
            (
                "preset-bedrock",
                "Amazon Bedrock",
                "https://bedrock-runtime.us-east-1.amazonaws.com",
                ApiFamily::Bedrock,
            ),
        ] {
            cloud = cloud.child(
                button(id, name)
                    .border_1()
                    .border_color(rgb(border()))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.preset(name, endpoint, family.clone(), window, cx)
                    })),
            );
        }
        page = page.child(cloud);

        let mut form = card().max_w(px(820.));
        form = form.child(subtitle(match self.settings.provider.family {
            ApiFamily::Azure => "Use your Azure resource endpoint and deployment name.",
            ApiFamily::Vertex => "Enter your project and location in the endpoint, model ID, and current access token.",
            ApiFamily::Bedrock => "Use a model or inference profile ID. Add a Bedrock API key, or use the AWS profile configured on this computer.",
            _ => "Available models and features depend on your provider account.",
        }));
        for (label, input) in [
            ("Provider name", &self.provider_name),
            ("Base URL", &self.endpoint),
            ("Model identifier", &self.model),
            (
                if self.settings.provider.family == ApiFamily::Vertex {
                    "Access token (leave blank to keep saved token)"
                } else {
                    "API key or access token (leave blank to keep saved credential)"
                },
                &self.secret,
            ),
        ] {
            if input.entity_id() == self.secret.entity_id()
                && self.settings.provider.credential_ref.as_deref() == Some(CHATGPT_REF)
            {
                continue;
            }
            form = form.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(subtitle(label))
                    .child(Input::new(input).aria_label(label)),
            );
        }
        page = page.child(
            form.child(
                div()
                    .flex()
                    .gap_2()
                    .child(primary("provider-save", "Save & use provider").on_click(
                        cx.listener(|this, _, window, cx| this.save_provider(window, cx)),
                    ))
                    .child(button("provider-discover", "Discover models").on_click(
                        cx.listener(|this, _, window, cx| this.save_provider(window, cx)),
                    )),
            ),
        );
        if !self.models.is_empty() {
            let weak = cx.entity().downgrade();
            let rows = uniform_list(
                "available-models",
                self.models.len(),
                move |range, _, cx| {
                    weak.update(cx, |view, cx| {
                        range
                            .filter_map(|i| {
                                let model = view.models.get(i)?.clone();
                                Some(
                                    button(("model", i), model.clone())
                                        .h(px(34.))
                                        .w_full()
                                        .justify_start()
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            Self::set_input(&this.model, model.clone(), window, cx);
                                            cx.notify();
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
            page = page.child(
                card()
                    .child(subtitle(format!(
                        "AVAILABLE MODELS · {}",
                        self.models.len()
                    )))
                    .child(rows),
            );
        }
        if !self.settings.providers.is_empty() {
            let mut saved = card().child(subtitle("SAVED PROVIDERS"));
            for (i, provider) in self.settings.providers.iter().enumerate() {
                let provider = provider.clone();
                saved = saved.child(
                    button(
                        ("saved-provider", i),
                        format!("{} · {}", provider.name, provider.model),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.settings.provider = provider.clone();
                        this.provider_form(&provider, window, cx);
                        let settings = this.settings.clone();
                        this.host_call(cx, |h| h.save_settings(settings));
                    })),
                );
            }
            page = page.child(saved);
        }
        page.child(self.local_models_card(cx))
            .child(self.native_result_card(cx))
            .into_any_element()
    }
}
