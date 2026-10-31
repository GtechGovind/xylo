//! Focused native setup for executable plugin components.
use super::*;
use std::collections::BTreeMap;
use xylo_runtime::extensions::{ComponentRuntimeStatus, ComponentSetup};

pub(in super::super) struct SetupUi {
    identity: Uuid,
    initial: String,
    package: String,
    package_name: String,
    component: String,
    kind: ComponentKind,
    digest: String,
    workspace: Option<PathBuf>,
    environment: Entity<TextareaState>,
    domains: Entity<InputState>,
    variable: Entity<InputState>,
    secret: Entity<InputState>,
    credentials: BTreeMap<String, String>,
    events: Vec<String>,
    timeout: u64,
    advanced: bool,
    saved: Option<ComponentSetup>,
    status: Option<ComponentRuntimeStatus>,
    _subscriptions: Vec<Subscription>,
}
impl SetupUi {
    pub(in super::super) fn dirty(&self, cx: &App) -> bool {
        if let Some(saved) = &self.saved {
            !self.secret.read(cx).value().is_empty()
                || !self.variable.read(cx).value().is_empty()
                || self.value(cx).ok().as_ref() != Some(saved)
        } else {
            self.revision(cx) != self.initial
        }
    }
    pub(in super::super) fn revision(&self, cx: &App) -> String {
        serde_json::to_string(&(
            self.workspace.clone(),
            self.environment.read(cx).value().to_string(),
            self.domains.read(cx).value().to_string(),
            self.variable.read(cx).value().to_string(),
            self.secret.read(cx).value().to_string(),
            &self.credentials,
            &self.events,
            self.timeout,
        ))
        .unwrap_or_default()
    }
    pub(in super::super) fn identity(&self) -> Uuid {
        self.identity
    }
    /// The saved setup meets every requirement of the component's entries.
    fn setup_complete(&self) -> bool {
        self.status
            .as_ref()
            .is_some_and(|status| status.entries.iter().all(|e| e.requirements.is_empty()))
    }
    /// Events this form would turn on beyond the saved setup.
    fn newly_enabled_events(&self) -> bool {
        let saved = self
            .saved
            .as_ref()
            .map(|setup| setup.enabled_events.as_slice())
            .unwrap_or_default();
        self.events.iter().any(|event| !saved.contains(event))
    }
    fn value(&self, cx: &App) -> anyhow::Result<ComponentSetup> {
        let mut environment = BTreeMap::new();
        for line in self
            .environment
            .read(cx)
            .value()
            .lines()
            .filter(|line| !line.trim().is_empty())
        {
            let (name, value) = line
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("Use NAME=value for each environment variable"))?;
            anyhow::ensure!(
                !environment.contains_key(name.trim()),
                "Environment variable {} is repeated",
                name.trim()
            );
            environment.insert(name.trim().to_owned(), value.to_owned());
        }
        Ok(ComponentSetup {
            workspace: self
                .workspace
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Choose a project folder"))?,
            environment,
            credential_refs: self.credentials.clone(),
            allowed_domains: self
                .domains
                .read(cx)
                .value()
                .split([',', ' ', '\n'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
            enabled_events: self.events.clone(),
            timeout_seconds: self.timeout,
        })
    }
}
impl Xylo {
    pub(in super::super) fn extension_leave_setup(
        &mut self,
        tab: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dirty = self
            .extensions
            .setup
            .as_ref()
            .is_some_and(|form| form.dirty(cx));
        if !dirty {
            self.focus_plugins_page(window, cx);
            self.extensions.setup = None;
            if let Some(tab) = tab {
                self.extensions.tab = tab;
            }
            cx.notify();
            return;
        }
        let identity = self.extensions.setup.as_ref().map(|form| form.identity);
        let revision = self.extensions.setup.as_ref().map(|form| form.revision(cx));
        let answer = window.prompt(
            PromptLevel::Warning,
            "Discard component setup changes?",
            Some("Unsaved grants, fields and entered secrets will be discarded."),
            &[
                PromptButton::cancel("Keep editing"),
                PromptButton::new("Discard changes"),
            ],
            cx,
        );
        cx.spawn_in(window, async move |view, cx| {
            let answer = answer.await;
            let _ = view.update_in(cx, |view, window, cx| {
                if matches!(answer, Ok(1))
                    && view.extensions.setup.as_ref().map(|form| form.identity) == identity
                    && view.extensions.setup.as_ref().map(|form| form.revision(cx)) == revision
                {
                    view.focus_plugins_page(window, cx);
                    view.extensions.setup = None;
                    if let Some(tab) = tab {
                        view.extensions.tab = tab;
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }
    pub(in super::super) fn extension_open_setup(
        &mut self,
        package: &ExtensionPackage,
        component: &str,
        kind: ComponentKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut form = SetupUi {
            identity: Uuid::new_v4(),
            initial: String::new(),
            package: package.id.clone(),
            package_name: package.name.clone(),
            component: component.into(),
            kind,
            digest: package.digest.clone(),
            workspace: self.workspace.clone(),
            environment: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(3)
                    .placeholder("NAME=value")
            }),
            domains: cx.new(|cx| InputState::new(window, cx).placeholder("api.example.com")),
            variable: cx.new(|cx| InputState::new(window, cx).placeholder("API_KEY")),
            secret: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Secret value")
                    .masked(true)
            }),
            credentials: BTreeMap::new(),
            events: vec![],
            timeout: 60,
            advanced: false,
            saved: None,
            status: None,
            _subscriptions: vec![],
        };
        form._subscriptions.push(cx.subscribe(
            &form.environment,
            |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            },
        ));
        form._subscriptions
            .push(cx.subscribe(&form.domains, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }));
        form.initial = form.revision(cx);
        self.focus_plugins_page(window, cx);
        self.extensions.setup = Some(form);
        self.extension_request(
            "extension_component_status",
            json!({"id":package.id,"component":component}),
            cx,
        );
    }
    pub(super) fn extension_setup_result(
        &mut self,
        value: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let Some(form) = self.extensions.setup.as_mut() else {
            return Ok(());
        };
        if let Some(status) = value.get("component_status").filter(|s| !s.is_null()) {
            let status: ComponentRuntimeStatus = serde_json::from_value(status.clone())?;
            anyhow::ensure!(
                status.package == form.package
                    && status.component == form.component
                    && status.digest == form.digest,
                "Plugin changed. Reopen its configuration to review the new version."
            );
            form.workspace = Some(status.setup.workspace.clone());
            form.environment.update(cx, |input, cx| {
                input.set_value(
                    status
                        .setup
                        .environment
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    window,
                    cx,
                )
            });
            form.domains.update(cx, |input, cx| {
                input.set_value(status.setup.allowed_domains.join(", "), window, cx)
            });
            form.credentials = status.setup.credential_refs.clone();
            form.events = status.setup.enabled_events.clone();
            form.timeout = status.setup.timeout_seconds;
            form.saved = Some(status.setup.clone());
            form.status = Some(status);
        }
        if let Some(credential) = value.get("credential") {
            let variable = credential["variable"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Credential name missing"))?;
            let reference = credential["reference"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Credential reference missing"))?;
            form.credentials.insert(variable.into(), reference.into());
            form.secret
                .update(cx, |input, cx| input.set_value("", window, cx));
            form.variable
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.extensions.notice = "Secret stored securely. Save setup to use it.".into();
        }
        Ok(())
    }
    fn extension_apply_workspace(&mut self, identity: Uuid, workspace: PathBuf) {
        if let Some(form) = self
            .extensions
            .setup
            .as_mut()
            .filter(|form| form.identity == identity)
        {
            form.workspace = Some(workspace);
        }
    }
    #[cfg(test)]
    #[allow(dead_code)] // Exercised by the separate native validation test target.
    pub(in super::super) fn validation_setup_secret(
        &mut self,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Uuid {
        let form = self.extensions.setup.as_mut().unwrap();
        form.secret
            .update(cx, |input, cx| input.set_value(value, window, cx));
        form.identity
    }
    #[cfg(test)]
    #[allow(dead_code)] // Exercised by the separate native validation test target.
    pub(in super::super) fn validation_setup_stale_picker(
        &mut self,
        identity: Uuid,
        path: PathBuf,
    ) {
        self.extension_apply_workspace(identity, path);
    }
    #[cfg(test)]
    #[allow(dead_code)] // Exercised by the separate native validation test target.
    pub(in super::super) fn validation_setup_workspace(&self) -> Option<PathBuf> {
        self.extensions
            .setup
            .as_ref()
            .and_then(|form| form.workspace.clone())
    }
    fn extension_setup_pick_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(identity) = self.extensions.setup.as_ref().map(|form| form.identity) else {
            return;
        };
        cx.spawn_in(window, async move |view, cx| {
            if let Some(folder) = rfd::AsyncFileDialog::new()
                .set_title("Plugin project folder")
                .pick_folder()
                .await
            {
                let _ = view.update_in(cx, |view, _, cx| {
                    view.extension_apply_workspace(identity, folder.path().to_owned());
                    cx.notify();
                });
            }
        })
        .detach();
    }
    /// The setup sub-page of one executable component: "Plugins / <package>".
    pub(super) fn extension_setup_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(form) = &self.extensions.setup else {
            return div().into_any_element();
        };
        let busy = self.extensions.pending.is_some();
        let unchanged = form
            .value(cx)
            .ok()
            .as_ref()
            .is_some_and(|value| Some(value) == form.saved.as_ref());
        let title = match form.kind {
            ComponentKind::Mcp => "MCP servers".to_owned(),
            ComponentKind::Hooks => "Hooks".to_owned(),
            ComponentKind::LanguageServer => "Language servers".to_owned(),
            _ => form.component.clone(),
        };
        let mut page = settings_ui::settings_page("extension-setup", title)
            .description(
                "Saving never runs the component. Verification runs it once with this setup.",
            )
            .breadcrumb(settings_ui::breadcrumb(
                [settings_ui::crumb("extension-setup-back", "Plugins")
                    .disabled(busy)
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.extension_leave_setup(None, window, cx)
                    }))],
                display_name(&form.package_name),
            ));
        if let Some(status) = self.extension_status(cx) {
            page = page.child(status);
        }
        page = page.child(settings_ui::section(
            "Project folder",
            None,
            settings_ui::setting_row(
                "Works in",
                Some(
                    form.workspace
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "Choose where this component can work.".into())
                        .into(),
                ),
                Some(
                    settings_ui::secondary_button("extension-setup-folder", "Choose folder")
                        .disabled(busy)
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.extension_setup_pick_workspace(window, cx)
                        }))
                        .into_any_element(),
                ),
            ),
        ));
        if form.kind == ComponentKind::Hooks {
            let setup_complete = form.setup_complete();
            let events =
                ["PreToolUse", "PostToolUse"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, event)| {
                        let on = form.events.iter().any(|enabled| enabled == event);
                        let label = if event == "PreToolUse" {
                            "Before tools"
                        } else {
                            "After tools"
                        };
                        settings_ui::setting_row(
                            label,
                            None,
                            Some(
                                settings_ui::switch(("extension-hook-event", index), label, on)
                                    .disabled(busy || (!on && !setup_complete))
                                    .on_change(cx.listener(move |view, _: &bool, _, cx| {
                                        if let Some(form) = view.extensions.setup.as_mut() {
                                            if form.events.iter().any(|enabled| enabled == event) {
                                                form.events.retain(|enabled| enabled != event);
                                            } else {
                                                form.events.push(event.into());
                                            }
                                        }
                                        cx.notify();
                                    }))
                                    .into_any_element(),
                            ),
                        )
                        .into_any_element()
                    });
            page = page.child(settings_ui::section(
                "Automatic events",
                Some(
                    if setup_complete {
                        "Enable only the events this hook should handle."
                    } else {
                        "Complete and save the setup below before enabling automatic events."
                    }
                    .into(),
                ),
                settings_ui::rows(events),
            ));
        }
        page = page.child(settings_ui::section(
            "Network access",
            None,
            settings_ui::field(
                "Allowed hosts",
                Input::new(&form.domains)
                    .disabled(busy)
                    .aria_label("Allowed network hosts"),
                Some(
                    "Leave empty for no network access. Use exact hostnames, separated by commas."
                        .into(),
                ),
            ),
        ));
        let mut credentials: Vec<AnyElement> = form
            .credentials
            .keys()
            .enumerate()
            .map(|(index, name)| {
                let remove = name.clone();
                settings_ui::setting_row(
                    name.clone(),
                    Some("Stored securely".into()),
                    Some(
                        settings_ui::destructive_button(
                            ("extension-secret-remove", index),
                            "Remove access",
                        )
                        .disabled(busy)
                        .on_click(cx.listener(move |view, _, _, cx| {
                            if let Some(form) = view.extensions.setup.as_mut() {
                                form.credentials.remove(&remove);
                            }
                            cx.notify();
                        }))
                        .into_any_element(),
                    ),
                )
                .into_any_element()
            })
            .collect();
        credentials.push(
            div()
                .w_full()
                .pt(px(12.))
                .flex()
                .items_end()
                .gap(px(8.))
                .child(
                    div().w(px(170.)).child(settings_ui::field(
                        "Variable",
                        Input::new(&form.variable)
                            .disabled(busy)
                            .aria_label("Credential variable"),
                        None,
                    )),
                )
                .child(
                    div().flex_1().min_w_0().child(settings_ui::field(
                        "Secret",
                        Input::new(&form.secret)
                            .disabled(busy)
                            .aria_label("Credential secret"),
                        None,
                    )),
                )
                .child(
                    settings_ui::busy(
                        settings_ui::secondary_button("extension-secret-save", "Store secret").disabled(busy),
                        self.extension_busy("extension_component_credential", None),
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                            let Some(form) = &view.extensions.setup else {
                                return;
                            };
                            let variable = form.variable.read(cx).value().trim().to_owned();
                            let secret = form.secret.read(cx).value().to_string();
                            if variable.is_empty() || secret.is_empty() {
                                view.extensions.notice =
                                    "Enter a variable name and secret value.".into();
                                cx.notify();
                                return;
                            }
                            view.extension_request(
                                "extension_component_credential",
                                json!({"id": form.package, "component": form.component, "variable": variable, "secret": secret}),
                                cx,
                            );
                        })),
                )
                .into_any_element(),
        );
        page = page.child(settings_ui::section(
            "Credentials",
            Some("Secrets are kept in Xylo's secret storage and passed to the component as environment variables.".into()),
            settings_ui::rows(credentials),
        ));
        let mut advanced = div().flex().flex_col().gap(px(12.)).child(
            div().child(
                settings_ui::text_button(
                    "extension-setup-advanced",
                    if form.advanced {
                        "Hide advanced settings"
                    } else {
                        "Show advanced settings"
                    },
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    if let Some(form) = view.extensions.setup.as_mut() {
                        form.advanced = !form.advanced;
                    }
                    cx.notify();
                })),
            ),
        );
        if form.advanced {
            let weak = cx.entity().downgrade();
            let timeouts = [30, 60, 120, 300]
                .into_iter()
                .map(|seconds| {
                    let weak = weak.clone();
                    settings_ui::SelectOption::new(
                        format!("{seconds} seconds"),
                        form.timeout == seconds,
                        move |_, cx| {
                            let _ = weak.update(cx, |view, cx| {
                                if let Some(form) = view.extensions.setup.as_mut() {
                                    form.timeout = seconds;
                                }
                                cx.notify();
                            });
                        },
                    )
                })
                .collect();
            advanced = advanced
                .child(settings_ui::field(
                    "Environment variables",
                    Textarea::new(&form.environment)
                        .disabled(busy)
                        .aria_label("Non-secret environment variables"),
                    Some("One NAME=value per line. Store secrets above.".into()),
                ))
                .child(settings_ui::setting_row(
                    "Execution timeout",
                    Some("How long one run may take before it is stopped.".into()),
                    Some(settings_ui::select(
                        "extension-timeout",
                        "Execution timeout",
                        timeouts,
                    )),
                ));
        }
        page = page.child(advanced);
        if let Some(status) = &form.status {
            let entries = status.entries.iter().map(|entry| {
                let detail: Vec<&str> = entry
                    .requirements
                    .iter()
                    .map(String::as_str)
                    .chain(entry.evidence.as_deref())
                    .collect();
                let label = entry.readiness.label();
                settings_ui::setting_row(
                    entry.name.clone(),
                    (!detail.is_empty()).then(|| detail.join(" · ").into()),
                    Some(
                        settings_ui::pill(
                            label,
                            match label {
                                "Needs setup" => settings_ui::Tone::Warning,
                                "Unsupported requirement" => settings_ui::Tone::Danger,
                                _ => settings_ui::Tone::Success,
                            },
                        )
                        .into_any_element(),
                    ),
                )
                .inset()
                .into_any_element()
            });
            page = page.child(settings_ui::section(
                "Status",
                Some(
                    format!(
                        "{}Package revision {}",
                        if unchanged {
                            ""
                        } else {
                            "Unsaved changes: save before verifying. "
                        },
                        &form.digest[..form.digest.len().min(12)]
                    )
                    .into(),
                ),
                settings_ui::list_container(entries),
            ));
        }
        page.child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(settings_ui::form_actions([
                    settings_ui::busy(
                        settings_ui::secondary_button("extension-setup-verify", "Run verification").disabled(busy || !unchanged),
                        self.extension_busy("extension_component_verify", None),
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                            let Some(form) = &view.extensions.setup else {
                                return;
                            };
                            if !form
                                .value(cx)
                                .ok()
                                .as_ref()
                                .is_some_and(|setup| Some(setup) == form.saved.as_ref())
                            {
                                view.extensions.notice =
                                    "Save your setup changes before verification.".into();
                                cx.notify();
                                return;
                            }
                            view.extension_request(
                                "extension_component_verify",
                                json!({"id": form.package, "component": form.component}),
                                cx,
                            );
                        }))
                        .into_any_element(),
                    settings_ui::busy(
                        settings_ui::primary_button("extension-setup-save", "Save setup").disabled(busy || unchanged),
                        self.extension_busy("extension_component_setup", None),
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                            let Some(form) = &view.extensions.setup else {
                                return;
                            };
                            if form.kind == ComponentKind::Hooks
                                && form.newly_enabled_events()
                                && !form.setup_complete()
                            {
                                view.extensions.notice = "Complete and save the required setup before enabling automatic events.".into();
                                cx.notify();
                                return;
                            }
                            match form.value(cx) {
                                Ok(setup) => view.extension_request(
                                    "extension_component_setup",
                                    json!({"id": form.package, "component": form.component, "setup": setup}),
                                    cx,
                                ),
                                Err(error) => {
                                    view.extensions.notice = error.to_string();
                                    cx.notify();
                                }
                            }
                        }))
                        .into_any_element(),
                ]))
                .child(div().flex().justify_end().child(settings_ui::caption(
                    "Verification runs installed code with this setup. It may change files in the selected project.",
                ))),
        )
        .into_any_element()
    }
}
