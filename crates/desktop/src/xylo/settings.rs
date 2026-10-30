//! Focused settings destinations. Every control uses an existing host boundary.
use super::*;
use gpui_kit::TestSupportExt;

pub(super) fn settings_content(id: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .size_full()
        .overflow_y_scroll()
        .px(px(48.))
        .pt(px(28.))
        .pb(px(64.))
        .flex()
        .flex_col()
        .items_center()
}
fn settings_column() -> Div {
    div()
        .w_full()
        .max_w(px(720.))
        .flex()
        .flex_col()
        .gap(px(18.))
}
fn setting_title(title: &'static str) -> Div {
    section_title(title).pt(px(10.))
}
fn setting_row(title: impl Into<SharedString>, detail: impl Into<SharedString>) -> Div {
    crate::ui::setting_row(title, detail)
}
impl Xylo {
    pub(super) fn save_connector(&mut self, cx: &mut Context<Self>) {
        if self.connector_pending.is_some() {
            return;
        }
        let name = self.connector_name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            self.connector_error = Some("A connection needs a name.".into());
            cx.notify();
            return;
        }
        let id = self.connector_editing.unwrap_or_else(Uuid::new_v4);
        let secret = self.connector_credential.read(cx).value().to_string();
        let credential_ref = if !secret.is_empty() {
            Some(format!("connector-{id}-{}", Uuid::new_v4()))
        } else {
            self.connectors
                .iter()
                .find(|config| config.id == id)
                .and_then(|config| config.credential_ref.clone())
        };
        let command = self.connector_command.read(cx).value().trim().to_owned();
        let config = ConnectorConfig {
            id,
            name,
            transport: self.connector_transport.clone(),
            endpoint: self.connector_endpoint.read(cx).value().trim().into(),
            command: (!command.is_empty()).then(|| PathBuf::from(command)),
            args: self
                .connector_args
                .read(cx)
                .value()
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_owned)
                .collect(),
            credential_ref,
        };
        let request_id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| {
                host.save_connector_authenticated(
                    request_id,
                    config,
                    (!secret.is_empty()).then_some(secret),
                )
            });
        match result {
            Ok(()) => {
                self.connector_pending = Some(request_id);
                self.connector_pending_save = true;
                self.connector_error = None;
            }
            Err(error) => self.connector_error = Some(error.to_string()),
        }
        cx.notify();
    }
    pub(super) fn connector_mutation_result(
        &mut self,
        request_id: Uuid,
        result: Result<xylo_runtime::host::ConnectorMutationReceipt, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.connector_pending != Some(request_id) {
            return;
        }
        self.connector_pending = None;
        match result {
            Ok(receipt) => {
                let invalidated = receipt
                    .removed
                    .or_else(|| receipt.saved.as_ref().map(|config| config.id));
                if let Some(id) = receipt.removed {
                    self.connectors.retain(|config| config.id != id);
                }
                if let Some(config) = receipt.saved {
                    self.connectors.retain(|existing| existing.id != config.id);
                    self.connectors.push(config);
                    self.connectors
                        .sort_by(|left, right| left.name.cmp(&right.name));
                }
                self.connector_error = None;
                if self.connector_inspected.is_some_and(|id| {
                    Some(id) == invalidated || !self.connectors.iter().any(|config| config.id == id)
                }) {
                    self.connector_inspection_request = None;
                    self.connector_inspected = None;
                    self.connector_loading = false;
                    self.connector_tools.clear();
                }
                if !self.connector_pending_save {
                    if self
                        .connector_editing
                        .is_some_and(|id| !self.connectors.iter().any(|config| config.id == id))
                    {
                        self.connector_editing = None;
                        self.connector_error = Some("Connection removed. The retained form will add a new connection; enter its access token again if needed.".into());
                    }
                    cx.notify();
                    return;
                }
                self.connector_editing = None;
                self.connector_name
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.connector_command
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.connector_args
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.connector_endpoint
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.connector_credential
                    .update(cx, |input, cx| input.set_value("", window, cx));
            }
            Err(error) => {
                self.connector_error = Some(format!(
                    "Connection change failed: {error}. Your form is retained; retry when ready."
                ))
            }
        }
        cx.notify();
    }
    fn edit_connector(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if self.connector_pending.is_some() {
            return;
        }
        let Some(config) = self
            .connectors
            .iter()
            .find(|config| config.id == id)
            .cloned()
        else {
            return;
        };
        self.connector_editing = Some(id);
        self.connector_transport = config.transport;
        self.connector_name
            .update(cx, |input, cx| input.set_value(config.name, window, cx));
        self.connector_command.update(cx, |input, cx| {
            input.set_value(
                config
                    .command
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                window,
                cx,
            )
        });
        self.connector_args.update(cx, |input, cx| {
            input.set_value(config.args.join("\n"), window, cx)
        });
        self.connector_endpoint
            .update(cx, |input, cx| input.set_value(config.endpoint, window, cx));
        self.connector_credential
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }
    fn remove_connector(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if self.connector_pending.is_some() {
            return;
        }
        let request_id = Uuid::new_v4();
        match self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.remove_connector(request_id, id))
        {
            Ok(()) => {
                self.connector_pending = Some(request_id);
                self.connector_pending_save = false;
            }
            Err(error) => self.connector_error = Some(error.to_string()),
        }
        cx.notify();
    }

    pub(super) fn inspect_connector(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let request_id = Uuid::new_v4();
        self.connector_inspection_request = Some(request_id);
        self.connector_inspected = Some(id);
        self.connector_loading = true;
        self.connector_error = None;
        self.connector_tools.clear();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.connector_tools_scoped(request_id, id, self.workspace.clone()));
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
            .rounded(px(12.))
            .bg(rgb(panel()))
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
            Some(
                if error.contains("401")
                    || error.contains("403")
                    || error.contains("credential is missing")
                {
                    format!(
                        "Authentication failed. Edit this connection to add or replace its access token, save, then retry. {error}"
                    )
                } else {
                    error.clone()
                },
            )
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
        let mut themes = segments();
        for (key, label) in [("light", "Light"), ("dark", "Dark")] {
            themes = themes.child(
                segment(
                    SharedString::from(format!("theme-{key}")),
                    label,
                    self.settings.theme == key,
                )
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.settings.theme = key.into();
                    view.apply_theme(window, cx);
                    view.persist_ui(cx);
                })),
            );
        }
        let modifier = if cfg!(target_os = "macos") {
            "⌘"
        } else {
            "Ctrl"
        };
        let mut shortcuts = setting_group();
        for (index, (label, key)) in [
            ("New chat", "N"),
            ("Search chats", "K"),
            ("Toggle terminal", "J"),
            ("Save file", "S"),
        ]
        .into_iter()
        .enumerate()
        {
            shortcuts = shortcuts.child(
                div()
                    .px(px(18.))
                    .py(px(12.))
                    .when(index > 0, |row| {
                        row.border_t_1().border_color(rgb(border()))
                    })
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(13.5)).child(label))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .child(kbd(modifier))
                            .child(kbd(key)),
                    ),
            );
        }
        settings_content("general-settings")
            .child(
                settings_column()
                    .child(heading(
                        "General",
                        "Appearance and keyboard shortcuts for your workspace.",
                    ))
                    .child(setting_title("Appearance"))
                    .child(
                        setting_group().child(
                            setting_row("Theme", "Choose the appearance of your workspace.")
                                .child(themes),
                        ),
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
                    match seconds {
                        120 => "2 min".into(),
                        300 => "5 min".into(),
                        _ => format!("{seconds}s"),
                    },
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
        if ![30, 120, 300].contains(&self.settings.provider.timeout_seconds) {
            timeouts = timeouts.child(subtitle(format!(
                "Current: {} seconds",
                self.settings.provider.timeout_seconds
            )));
        }
        settings_content("agent-settings")
            .child(
                settings_column()
                    .child(setting_title("Agent"))
                    .child(
                        div()
                            .rounded(px(12.))
                            .bg(rgb(panel()))
                            .border_1()
                            .border_color(rgb(border()))
                            .child(
                                setting_row(
                                    "Response timeout",
                                    "Time to first response, then the maximum pause between streaming updates.",
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
                        .rounded(px(12.))
                        .bg(rgb(panel()))
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
            .rounded(px(12.))
            .bg(rgb(panel()))
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
                    .child(setting_title("Saved procedures"))
                    .child(subtitle("Procedures learned from completed work. Install and manage portable skills in Plugins."))
                    .child(button("manage-portable-skills", "Manage portable skills").on_click(cx.listener(|view, _, _, cx| view.navigate(Page::Plugins, cx))))
                    .child(self.skills_settings_content(cx)),
            )
            .into_any_element()
    }

    pub(super) fn connections_settings_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut transport = div().flex().flex_wrap().gap_2();
        for (value, label) in [("stdio", "Local command"), ("http", "Remote server")] {
            transport = transport.child(
                button(SharedString::from(format!("transport-{value}")), label)
                    .accessibility_label(value)
                    .disabled(self.connector_pending.is_some())
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
            .rounded(px(12.))
            .bg(rgb(panel()))
            .border_1()
            .border_color(rgb(border()))
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .child(subtitle("Save the configuration first, then choose Inspect tools to request discovery. Saving does not start a server or verify its availability."))
            .child(transport)
            .child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).child("Connection name"))
            .child(
                Input::new(&self.connector_name)
                    .readonly(self.connector_pending.is_some())
                    .aria_label("Connection name"),
            );
        if self.connector_transport == "stdio" {
            form = form
                .child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).child("Server command"))
                .child(subtitle("Path to an installed server executable, for example /absolute/path/to/server. Example only; nothing runs when you save."))
                .child(
                    Input::new(&self.connector_command)
                        .readonly(self.connector_pending.is_some())
                        .aria_label("Server command"),
                )
                .child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).child("Arguments, one per line"))
                .child(subtitle("Optional: enter each argument on its own line, for example --workspace and /absolute/path/to/project on separate lines. These are arguments, not a shell command."))
                .child(
                    Textarea::new(&self.connector_args)
                        .readonly(self.connector_pending.is_some())
                        .aria_label("Arguments, one per line"),
                );
        } else {
            form = form
                .child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).child("Server endpoint"))
                .child(subtitle("Use the endpoint supplied by your server administrator, for example https://tools.example.com/mcp. Example only; saving does not contact it."))
                .child(Input::new(&self.connector_endpoint).readonly(self.connector_pending.is_some()).aria_label("Server endpoint"))
                .child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).child("Access token (optional)"))
                .child(Input::new(&self.connector_credential).readonly(self.connector_pending.is_some()).aria_label("Access token (optional)"))
                .child(subtitle("Access token is stored in the system credential store. Leave blank when editing to keep the saved token."));
        }
        form = form.child(
            div().flex().flex_wrap().gap_2().justify_end().child(
                primary(
                    "connector-save",
                    if self.connector_pending.is_some() {
                        "Saving…"
                    } else if self.connector_editing.is_some() {
                        "Save connection"
                    } else {
                        "Add connection"
                    },
                )
                .disabled(self.connector_pending.is_some())
                .on_click(cx.listener(|view, _, _, cx| view.save_connector(cx))),
            ),
        );
        let mut column = settings_column()
            .child(setting_title("Connection setup"))
            .child(form);
        if let Some(error) = &self.connector_error {
            column = column.child(subtitle(error.clone()));
        }
        if !self.connectors.is_empty() {
            column = column.child(setting_title("Connections"));
            for (index, connection) in self.connectors.iter().enumerate() {
                let id = connection.id;
                column = column.child(
                    setting_row(connection.name.clone(), connection.transport.clone())
                        .child(
                            button(("connector-edit", index), "Edit")
                                .disabled(self.connector_pending.is_some())
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.edit_connector(id, window, cx)
                                })),
                        )
                        .child(
                            button(("connector-remove", index), "Remove")
                                .disabled(self.connector_pending.is_some())
                                .on_click(
                                    cx.listener(move |view, _, _, cx| {
                                        view.remove_connector(id, cx)
                                    }),
                                ),
                        )
                        .rounded(px(12.))
                        .bg(rgb(panel()))
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
            .rounded(px(12.))
            .bg(rgb(panel()))
            .border_1()
            .border_color(rgb(border()))
            .flex()
            .flex_col();
        if self.connectors.is_empty() {
            connections = connections.child(setting_row(
                "No connections",
                "Add a server, review its access, then discover its tools.",
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
                    .rounded(px(12.))
                    .bg(rgb(panel()))
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
