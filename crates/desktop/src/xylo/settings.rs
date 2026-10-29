//! Focused settings destinations. Every control uses an existing host boundary.
use super::*;
use gpui_kit::TestSupportExt;

fn settings_content(id: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .size_full()
        .overflow_y_scroll()
        .px(px(45.))
        .py(px(38.))
        .flex()
        .flex_col()
        .items_center()
}
fn settings_column() -> Div {
    div().w_full().max_w(px(760.)).flex().flex_col().gap_5()
}
fn setting_title(title: &'static str) -> Div {
    div()
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(title)
}
fn setting_row(title: impl Into<SharedString>, detail: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .py_3()
        .px_4()
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::MEDIUM)
                        .child(title.into()),
                )
                .child(subtitle(detail).text_size(px(11.))),
        )
}
impl Xylo {
    pub(super) fn inspect_connector(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.connector_inspected = Some(id);
        self.connector_loading = true;
        self.connector_error = None;
        self.connector_tools.clear();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.connector_tools(id));
        if let Err(error) = result {
            self.connector_loading = false;
            self.connector_error = Some(error.to_string());
        }
        cx.notify();
    }
    fn connector_inspection_panel(&self, cx: &Context<Self>) -> AnyElement {
        let Some(id) = self.connector_inspected else {
            return div().into_any_element();
        };
        let name = self
            .connectors
            .iter()
            .find(|connection| connection.id == id)
            .map(|connection| connection.name.clone())
            .unwrap_or_else(|| "Connection".into());
        let mut panel = div()
            .rounded_lg()
            .border_1()
            .border_color(rgb(border()))
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(format!("{name} tools")),
            );
        let status = if self.connector_loading {
            Some("Loading tools…".to_owned())
        } else if let Some(error) = &self.connector_error {
            Some(error.clone())
        } else if self.connector_tools.is_empty() {
            Some("No tools returned".into())
        } else {
            None
        };
        if let Some(status) = status {
            panel = panel.child(
                div()
                    .id("connector-inspection-status")
                    .role(Role::Status)
                    .aria_label(status.clone())
                    .test_support()
                    .text_sm()
                    .text_color(rgb(muted()))
                    .child(status),
            );
        }
        if self.connector_error.is_some() {
            panel = panel.child(
                button("retry-connector-inspection", "Try again")
                    .on_click(cx.listener(move |view, _, _, cx| view.inspect_connector(id, cx))),
            );
        }
        if !self.connector_loading
            && self.connector_error.is_none()
            && !self.connector_tools.is_empty()
        {
            let weak = cx.entity().downgrade();
            panel = panel.child(
                uniform_list(
                    "connector-tool-results",
                    self.connector_tools.len(),
                    move |range, _, cx| {
                        weak.update(cx, |view, _| {
                            range
                                .filter_map(|index| {
                                    view.connector_tools.get(index).map(|tool| {
                                        div()
                                            .h(px(52.))
                                            .flex()
                                            .flex_col()
                                            .justify_center()
                                            .gap_1()
                                            .child(
                                                div().text_sm().truncate().child(tool.name.clone()),
                                            )
                                            .child(subtitle(tool.description.clone()).truncate())
                                            .into_any_element()
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                    },
                )
                .h(px((self.connector_tools.len() as f32 * 52.).min(280.))),
            );
        }
        panel.into_any_element()
    }
    pub(super) fn general_settings_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut themes = div().flex().items_center().gap_2();
        for (key, label) in [("light", "Light"), ("dark", "Dark")] {
            themes = themes.child(
                button(SharedString::from(format!("theme-{key}")), label)
                    .bg(rgb(if self.settings.theme == key {
                        selected()
                    } else {
                        panel()
                    }))
                    .border_1()
                    .border_color(rgb(border()))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.settings.theme = key.into();
                        view.apply_theme(window, cx);
                        view.persist_ui(cx);
                    })),
            );
        }
        let mut shortcuts = div().rounded_lg().border_1().border_color(rgb(border()));
        for (label, key) in [
            ("New chat", "⌘ / Ctrl N"),
            ("Search chats", "⌘ / Ctrl K"),
            ("Toggle terminal", "⌘ / Ctrl J"),
            ("Save file", "⌘ / Ctrl S"),
        ] {
            shortcuts = shortcuts.child(
                div()
                    .px_4()
                    .py_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_sm().child(label))
                    .child(div().text_xs().text_color(rgb(muted())).child(key)),
            );
        }
        settings_content("general-settings")
            .child(
                settings_column()
                    .child(setting_title("Appearance"))
                    .child(
                        setting_row("Theme", "Choose the appearance of your workspace.")
                            .child(themes)
                            .rounded_lg()
                            .border_1()
                            .border_color(rgb(border())),
                    )
                    .child(setting_title("Keyboard shortcuts"))
                    .child(shortcuts),
            )
            .into_any_element()
    }

    pub(super) fn agent_settings_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut timeouts = div().flex().items_center().gap_2();
        for seconds in [30u64, 120, 300] {
            timeouts = timeouts.child(
                button(
                    SharedString::from(format!("agent-timeout-{seconds}")),
                    match seconds { 120 => "2 min".into(), 300 => "5 min".into(), _ => format!("{seconds}s") },
                )
                .bg(rgb(if self.settings.provider.timeout_seconds == seconds {
                    selected()
                } else {
                    panel()
                }))
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.settings.provider.timeout_seconds = seconds;
                    let provider = view.settings.provider.clone();
                    view.settings.remember_model(provider);
                    let settings = view.settings.clone();
                    view.host_call(cx, |host| host.save_settings(settings));
                    cx.notify();
                })),
            );
        }
        settings_content("agent-settings")
            .child(
                settings_column()
                    .child(setting_title("Agent"))
                    .child(
                        div()
                            .rounded_lg()
                            .border_1()
                            .border_color(rgb(border()))
                            .child(
                                setting_row(
                                    "Request timeout",
                                    "Limit for one provider request, including streaming.",
                                )
                                .child(timeouts),
                            )
                            .child(
                                setting_row(
                                    "Remote environments",
                                    "SSH and container connections used by tools.",
                                )
                                .child(
                                    button("agent-environments", "Configure").on_click(
                                        cx.listener(|view, _, _, cx| {
                                            view.navigate(Page::Environments, cx)
                                        }),
                                    ),
                                ),
                            )
                            .when(self.recovery_blocks_submit(), |section| section.child(
                                setting_row(
                                    "Interrupted actions",
                                    "Review actions with an uncertain result before resuming.",
                                )
                                .child(
                                    button("agent-recovery", "Review").on_click(cx.listener(
                                        |view, _, window, cx| {
                                            view.settings_open = false;
                                            view.page = Page::Chat;
                                            view.recovery_activate(window, cx);
                                            cx.notify();
                                        },
                                    )),
                                ),
                            )),
                    )
                    .child(setting_title("Model"))
                    .child(
                        setting_row(
                            self.settings.provider.model.clone(),
                            self.settings.provider.name.clone(),
                        )
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(border()))
                        .child(
                            button("agent-model-settings", "Configure").on_click(
                                cx.listener(|view, _, _, cx| view.navigate(Page::Providers, cx)),
                            ),
                        ),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn permission_settings_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut modes = div()
            .rounded_lg()
            .border_1()
            .border_color(rgb(border()))
            .flex()
            .flex_col();
        for (index, mode) in PermissionMode::ALL.into_iter().enumerate() {
            modes = modes.child(
                focus_ring(gpui_kit::base::Button::new(("permission-setting", index)))
                    .accessibility_label(format!("{}: {}", mode.label(), mode.description()))
                    .w_full()
                    .px_4()
                    .py_3()
                    .flex()
                    .items_center()
                    .justify_start()
                    .gap_3()
                    .text_color(rgb(text()))
                    .bg(rgb(if self.settings.permissions == mode {
                        selected()
                    } else {
                        panel()
                    }))
                    .hover(|style| style.bg(rgb(hover())))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .items_start()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(mode.label()),
                            )
                            .child(subtitle(mode.description())),
                    )
                    .child(div().text_color(rgb(muted())).child(
                        if self.settings.permissions == mode {
                            "●"
                        } else {
                            "○"
                        },
                    ))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.settings.permissions = mode;
                        let settings = view.settings.clone();
                        view.host_call(cx, |host| host.save_settings(settings));
                        cx.notify();
                    })),
            );
        }
        settings_content("permissions-settings").child(settings_column()
            .child(setting_title("Tool approvals"))
            .child(modes)
            .child(subtitle("Policy applies to new requests. External actions and uncertain effects retain their approval and recovery checks.")))
            .into_any_element()
    }

    pub(super) fn skills_settings_page(&self, cx: &mut Context<Self>) -> AnyElement {
        settings_content("skills-settings")
            .child(
                settings_column()
                    .child(setting_title("Skills"))
                    .child(self.skills_settings_content(cx)),
            )
            .into_any_element()
    }

    pub(super) fn connections_settings_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut transport = div().flex().gap_2();
        for value in ["stdio", "http"] {
            transport = transport.child(
                button(SharedString::from(format!("transport-{value}")), value)
                    .bg(rgb(if self.connector_transport == value {
                        selected()
                    } else {
                        panel()
                    }))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.connector_transport = value.into();
                        cx.notify();
                    })),
            );
        }
        let mut form = div()
            .rounded_lg()
            .border_1()
            .border_color(rgb(border()))
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .child(transport)
            .child(Input::new(&self.connector_name).aria_label("Connection name"));
        if self.connector_transport == "stdio" {
            form = form
                .child(Input::new(&self.connector_command).aria_label("Server command"))
                .child(Input::new(&self.connector_args).aria_label("Arguments, one per line"));
        } else {
            form = form.child(Input::new(&self.connector_endpoint).aria_label("Server endpoint"));
        }
        form = form.child(
            div().flex().justify_end().child(
                primary("connector-save", "Add connection")
                    .on_click(cx.listener(|view, _, _, cx| view.save_connector(cx))),
            ),
        );
        let mut column = settings_column()
            .child(setting_title("Add connection"))
            .child(form);
        if !self.connectors.is_empty() {
            column = column.child(setting_title("Connections"));
            for (index, connection) in self.connectors.iter().enumerate() {
                let id = connection.id;
                column = column.child(
                    setting_row(connection.name.clone(), connection.transport.clone())
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(border()))
                        .child(
                            button(("connector-tools", index), "Inspect tools")
                                .disabled(
                                    self.connector_loading && self.connector_inspected == Some(id),
                                )
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    view.inspect_connector(id, cx);
                                })),
                        ),
                );
            }
        }
        column = column.child(self.connector_inspection_panel(cx));
        settings_content("connections-settings")
            .child(column)
            .into_any_element()
    }

    pub(super) fn context_settings_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut connections = div()
            .rounded_lg()
            .border_1()
            .border_color(rgb(border()))
            .flex()
            .flex_col();
        if self.connectors.is_empty() {
            connections = connections.child(setting_row(
                "No connections",
                "Add an MCP server to make its tools available to the agent.",
            ));
        }
        for (index, connection) in self.connectors.iter().enumerate() {
            let id = connection.id;
            connections = connections.child(
                setting_row(connection.name.clone(), connection.transport.clone()).child(
                    button(("context-tools", index), "Inspect tools")
                        .disabled(self.connector_loading && self.connector_inspected == Some(id))
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.inspect_connector(id, cx);
                        })),
                ),
            );
        }
        connections = connections.child(
            div().px_4().py_3().child(
                button("manage-context-connections", "Manage connections")
                    .on_click(cx.listener(|view, _, _, cx| view.navigate(Page::Connectors, cx))),
            ),
        );
        let mut column = settings_column()
            .child(setting_title("Context sources"))
            .child(
                div()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(border()))
                    .flex()
                    .flex_col()
                    .child(setting_row(
                        "Project files",
                        "Files read through tools are scoped to the active project.",
                    ))
                    .child(
                        setting_row(
                            "Memory",
                            "Saved facts are included according to their scope.",
                        )
                        .child(
                            button("context-memory", "Manage").on_click(
                                cx.listener(|view, _, _, cx| view.navigate(Page::Memory, cx)),
                            ),
                        ),
                    )
                    .child(
                        setting_row("Skills", "Reviewed procedures provide reusable guidance.")
                            .child(button("context-skills", "Manage").on_click(
                                cx.listener(|view, _, _, cx| view.navigate(Page::Skills, cx)),
                            )),
                    ),
            )
            .child(setting_title("Connections"))
            .child(connections);
        column = column.child(self.connector_inspection_panel(cx));
        settings_content("context-settings")
            .child(column)
            .into_any_element()
    }
}
