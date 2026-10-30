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
            let _ = view.update_in(cx, |view, _, cx| {
                if matches!(answer, Ok(1))
                    && view.extensions.setup.as_ref().map(|form| form.identity) == identity
                    && view.extensions.setup.as_ref().map(|form| form.revision(cx)) == revision
                {
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
    pub(in super::super) fn validation_setup_stale_picker(
        &mut self,
        identity: Uuid,
        path: PathBuf,
    ) {
        self.extension_apply_workspace(identity, path);
    }
    #[cfg(test)]
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
        let mut body = div()
            .id("extension-setup")
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        button("extension-setup-back", "Back")
                            .disabled(busy)
                            .on_click(cx.listener(|v, _, window, cx| {
                                v.extension_leave_setup(None, window, cx)
                            })),
                    )
                    .child(
                        div().text_lg().font_weight(FontWeight::SEMIBOLD).child(
                            match form.kind {
                                ComponentKind::Mcp => "MCP servers",
                                ComponentKind::Hooks => "Hooks",
                                ComponentKind::LanguageServer => "Language servers",
                                _ => &form.component,
                            }
                            .to_owned(),
                        ),
                    ),
            )
            .child(subtitle(form.package_name.clone()))
            .child(
                div()
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
                            .child("Project folder")
                            .child(subtitle(
                                form.workspace
                                    .as_ref()
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_else(|| {
                                        "Choose where this component can work".into()
                                    }),
                            )),
                    )
                    .child(
                        button("extension-setup-folder", "Choose folder")
                            .disabled(busy)
                            .on_click(
                                cx.listener(|v, _, w, cx| v.extension_setup_pick_workspace(w, cx)),
                            ),
                    ),
            );
        if form.kind == ComponentKind::Hooks {
            let mut events = div().flex().flex_wrap().gap_2();
            for (index, event) in ["PreToolUse", "PostToolUse"].into_iter().enumerate() {
                events = events.child(
                    button(
                        ("extension-hook-event", index),
                        if event == "PreToolUse" {
                            "Before tools"
                        } else {
                            "After tools"
                        },
                    )
                    .disabled(busy)
                    .bg(rgb(if form.events.iter().any(|e| e == event) {
                        selected()
                    } else {
                        panel()
                    }))
                    .on_click(cx.listener(move |v, _, _, cx| {
                        if let Some(form) = v.extensions.setup.as_mut() {
                            if form.events.iter().any(|e| e == event) {
                                form.events.retain(|e| e != event);
                            } else {
                                form.events.push(event.into());
                            }
                        }
                        cx.notify();
                    })),
                );
            }
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child("Automatic events")
                    .child(subtitle("Enable only the events this hook should handle."))
                    .child(events),
            );
        }
        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child("Network access")
                .child(
                    Input::new(&form.domains)
                        .disabled(busy)
                        .aria_label("Allowed network hosts"),
                )
                .child(subtitle(
                    "Leave empty for no network access. Use exact hostnames, separated by commas.",
                )),
        );
        let mut credentials = div().flex().flex_col().gap_2().child("Credentials");
        for (index, (name, _)) in form.credentials.iter().enumerate() {
            let name = name.clone();
            let remove = name.clone();
            credentials = credentials.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .child(format!("{name} · Stored securely")),
                    )
                    .child(
                        button(("extension-secret-remove", index), "Remove access")
                            .disabled(busy)
                            .on_click(cx.listener(move |v, _, _, cx| {
                                if let Some(form) = v.extensions.setup.as_mut() {
                                    form.credentials.remove(&remove);
                                }
                                cx.notify();
                            })),
                    ),
            );
        }
        credentials = credentials.child(div().flex().gap_2()
            .child(div().w(px(155.)).child(Input::new(&form.variable).disabled(busy).aria_label("Credential variable")))
            .child(div().flex_1().min_w_0().child(Input::new(&form.secret).disabled(busy).aria_label("Credential secret")))
            .child(button("extension-secret-save", "Store secret").disabled(busy).on_click(cx.listener(|v, _, _, cx| {
                let Some(form) = &v.extensions.setup else { return; };
                let variable = form.variable.read(cx).value().trim().to_owned();
                let secret = form.secret.read(cx).value().to_string();
                if variable.is_empty() || secret.is_empty() { v.extensions.notice = "Enter a variable name and secret value.".into(); cx.notify(); return; }
                v.extension_request("extension_component_credential", json!({"id":form.package,"component":form.component,"variable":variable,"secret":secret}), cx);
            }))));
        body = body.child(credentials).child(
            div().flex().child(
                button(
                    "extension-setup-advanced",
                    if form.advanced {
                        "Hide advanced"
                    } else {
                        "Advanced"
                    },
                )
                .on_click(cx.listener(|v, _, _, cx| {
                    if let Some(form) = v.extensions.setup.as_mut() {
                        form.advanced = !form.advanced;
                    }
                    cx.notify();
                })),
            ),
        );
        if form.advanced {
            let mut timeouts = div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().flex_1().child("Execution timeout"));
            for seconds in [30, 60, 120, 300] {
                timeouts = timeouts.child(
                    button(
                        ("extension-timeout", seconds as usize),
                        format!("{seconds}s"),
                    )
                    .disabled(busy)
                    .bg(rgb(if form.timeout == seconds {
                        selected()
                    } else {
                        panel()
                    }))
                    .on_click(cx.listener(move |v, _, _, cx| {
                        if let Some(form) = v.extensions.setup.as_mut() {
                            form.timeout = seconds;
                        }
                        cx.notify();
                    })),
                );
            }
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child("Environment variables")
                    .child(
                        Textarea::new(&form.environment)
                            .disabled(busy)
                            .aria_label("Non-secret environment variables"),
                    )
                    .child(subtitle("One NAME=value per line. Store secrets above."))
                    .child(timeouts),
            );
        }
        if let Some(status) = &form.status {
            let mut entries = div().flex().flex_col().gap_2();
            if !unchanged {
                entries = entries.child(subtitle(
                    "Unsaved changes. Save setup before verifying this configuration.",
                ));
            }
            for entry in &status.entries {
                entries = entries.child(
                    div()
                        .flex()
                        .gap_3()
                        .child(div().flex_1().text_sm().child(entry.name.clone()))
                        .child(subtitle(entry.readiness.label())),
                );
                for requirement in &entry.requirements {
                    entries = entries.child(subtitle(requirement.clone()));
                }
                if let Some(evidence) = &entry.evidence {
                    entries = entries.child(subtitle(evidence.clone()));
                }
            }
            body = body.child(entries).child(subtitle(format!(
                "Package revision {}",
                &form.digest[..form.digest.len().min(12)]
            )));
        }
        body.child(div().flex().items_center().gap_2()
            .child(primary("extension-setup-save", "Save setup").disabled(busy || unchanged).opacity(if busy || unchanged { 0.4 } else { 1. }).on_click(cx.listener(|v, _, _, cx| {
                let Some(form) = &v.extensions.setup else { return; };
                match form.value(cx) { Ok(setup) => v.extension_request("extension_component_setup", json!({"id":form.package,"component":form.component,"setup":setup}), cx), Err(error) => { v.extensions.notice = error.to_string(); cx.notify(); } }
            })))
            .child(button("extension-setup-verify", "Run verification").disabled(busy || !unchanged).opacity(if busy || !unchanged { 0.4 } else { 1. }).on_click(cx.listener(|v, _, _, cx| {
                let Some(form) = &v.extensions.setup else { return; };
                if !form.value(cx).ok().as_ref().is_some_and(|setup| Some(setup) == form.saved.as_ref()) {
                    v.extensions.notice = "Save your setup changes before verification.".into(); cx.notify(); return;
                }
                v.extension_request("extension_component_verify", json!({"id":form.package,"component":form.component}), cx);
            }))))
            .child(subtitle("Verification runs installed code with this setup. It may change files in the selected project."))
            .into_any_element()
    }
}
