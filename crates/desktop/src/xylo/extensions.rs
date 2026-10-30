//! Portable extension management. Filesystem and networking stay on the host.
use super::*;
use serde_json::{Value, json};
use xylo_runtime::extensions::{
    ComponentKind, DiscoveredExtension, ExtensionPackage, ExtensionSource, default_sources,
};
#[path = "extension_setup.rs"]
mod setup;

pub(super) struct ExtensionUi {
    tab: usize,
    native: bool,
    packages: Vec<ExtensionPackage>,
    sources: Vec<ExtensionSource>,
    discovered: Vec<DiscoveredExtension>,
    pending: Option<Uuid>,
    pending_name: String,
    cancelling: bool,
    loaded: bool,
    notice: String,
    source: Entity<InputState>,
    query: Entity<InputState>,
    selected_source: usize,
    pub(super) setup: Option<setup::SetupUi>,
}
impl ExtensionUi {
    pub fn new(window: &mut Window, cx: &mut Context<Xylo>) -> Self {
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Xylo>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        Self {
            tab: 1,
            native: false,
            packages: vec![],
            sources: default_sources(),
            discovered: vec![],
            pending: None,
            pending_name: String::new(),
            cancelling: false,
            loaded: false,
            notice: String::new(),
            source: input("GitHub repository or directory URL", window, cx),
            query: input("Search plugins and skills", window, cx),
            selected_source: 0,
            setup: None,
        }
    }
    pub(super) fn active(&self) -> bool {
        self.pending.is_some()
    }
}
impl Xylo {
    pub(super) fn extension_request(&mut self, name: &str, args: Value, cx: &mut Context<Self>) {
        if self.extensions.pending.is_some() {
            return;
        }
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.capability(id, name.into(), args));
        match result {
            Ok(()) => {
                self.extensions.pending = Some(id);
                self.extensions.pending_name = name.into();
                self.extensions.cancelling = false;
                self.extensions.notice.clear();
            }
            Err(e) => self.extensions.notice = e.to_string(),
        }
        cx.notify();
    }
    pub(super) fn extension_result(
        &mut self,
        id: Uuid,
        result: Result<Value, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.extensions.pending != Some(id) {
            return;
        }
        self.extensions.pending = None;
        let operation = std::mem::take(&mut self.extensions.pending_name);
        let cancelling = std::mem::take(&mut self.extensions.cancelling);
        let result = result.map_err(anyhow::Error::msg).and_then(|v| {
            self.extension_setup_result(&v, window, cx)?;
            if let Some(p) = v.get("packages") {
                self.extensions.packages = serde_json::from_value(p.clone())?;
                self.extensions.loaded = true;
            }
            if let Some(s) = v.get("sources") {
                self.extensions.sources = serde_json::from_value(s.clone())?;
                self.extensions.selected_source = self
                    .extensions
                    .selected_source
                    .min(self.extensions.sources.len().saturating_sub(1));
            }
            if let Some(d) = v.get("discovered") {
                self.extensions.discovered = serde_json::from_value(d.clone())?;
            }
            if v.get("package").is_some() {
                self.extensions.tab = 1;
                self.extensions.notice =
                    "Installed. Enable it when you are ready to use it.".into();
            }
            Ok(())
        });
        if let Err(e) = result {
            self.extensions.notice = if cancelling {
                format!("Operation interrupted. Changes made before cancellation may remain.\n{e}")
            } else if operation == "extension_component_verify" {
                format!(
                    "Verification failed. Check the component's command and setup, then retry.\n{e}"
                )
            } else {
                e.to_string()
            };
        }
        cx.notify();
    }
    fn extension_pick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.extensions.pending.is_some() {
            return;
        }
        cx.spawn_in(window, async move |view, cx| {
            if let Some(folder) = rfd::AsyncFileDialog::new()
                .set_title("Choose a skill or plugin folder")
                .pick_folder()
                .await
            {
                let _ = view.update_in(cx, |view, _, cx| {
                    view.extension_request(
                        "extension_install_local",
                        json!({"path": folder.path()}),
                        cx,
                    )
                });
            }
        })
        .detach();
    }
    pub(super) fn plugins_page(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.extensions.native {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .child(
                    button("extensions-back", "Back to plugins").on_click(cx.listener(
                        |v, _, _, cx| {
                            v.extensions.native = false;
                            cx.notify();
                        },
                    )),
                )
                .child(div().flex_1().min_h_0().child(self.native_plugins_page(cx)))
                .into_any_element();
        }
        let busy = self.extensions.pending.is_some();
        let mut tabs = div().flex().gap_2();
        for (index, label) in ["Discover", "Installed", "Sources"].into_iter().enumerate() {
            tabs = tabs.child(
                button(("extensions-tab", index), label)
                    .disabled(busy)
                    .bg(rgb(if self.extensions.tab == index {
                        selected()
                    } else {
                        panel()
                    }))
                    .on_click(cx.listener(move |v, _, window, cx| {
                        v.extension_leave_setup(Some(index), window, cx);
                    })),
            );
        }
        let mut content = div()
            .w_full()
            .max_w(px(760.))
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(22.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Plugins"),
                    )
                    .child(
                        button("extensions-local", "Install from folder")
                            .disabled(busy)
                            .on_click(cx.listener(|v, _, w, cx| v.extension_pick(w, cx))),
                    ),
            )
            .child(tabs);
        if busy {
            content = content.child(div().flex().items_center().gap_3()
                .child(subtitle(if self.extensions.pending_name == "extension_component_verify" { "Verifying…" } else { "Working…" }))
                .child(button("extension-cancel", "Cancel").on_click(cx.listener(|view, _, _, cx| {
                    if let Some(id) = view.extensions.pending && let Some(host) = &view.host {
                        view.extensions.notice = match host.cancel_capability(id) {
                            Ok(()) => { view.extensions.cancelling = true; "Cancellation requested. Waiting for the operation's result.".into() },
                            Err(error) => error.to_string(),
                        };
                        cx.notify();
                    }
                }))));
        }
        if !self.extensions.notice.is_empty() {
            content = content.child(
                div()
                    .id("extensions-notice")
                    .text_sm()
                    .child(self.extensions.notice.clone()),
            );
        }
        content = content.child(if self.extensions.setup.is_some() {
            self.extension_setup_view(cx)
        } else {
            match self.extensions.tab {
                0 => self.extension_discover_view(cx),
                1 => self.extension_installed_view(cx),
                _ => self.extension_sources_view(cx),
            }
        });
        div()
            .id("extensions-page")
            .size_full()
            .overflow_y_scroll()
            .px(px(40.))
            .py(px(32.))
            .flex()
            .flex_col()
            .items_center()
            .child(content)
            .into_any_element()
    }
    fn extension_discover_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.extensions.pending.is_some();
        let mut sources = div().flex().flex_wrap().gap_2();
        for (index, source) in self.extensions.sources.iter().enumerate() {
            sources = sources.child(
                button(("extension-source", index), source.name.clone())
                    .disabled(busy)
                    .bg(rgb(if index == self.extensions.selected_source {
                        selected()
                    } else {
                        panel()
                    }))
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.extensions.selected_source = index;
                        v.extensions.discovered.clear();
                        if let Some(source) = v.extensions.sources.get(index).cloned() {
                            v.extension_request("extension_discover", json!({"source":source}), cx);
                        }
                    })),
            );
        }
        let mut body = div().flex().flex_col().gap_3().child(sources).child(
            div()
                .flex()
                .gap_2()
                .child(div().flex_1().child(Input::new(&self.extensions.query)))
                .child(
                    button("extension-search", "Search")
                        .disabled(busy)
                        .on_click(cx.listener(|v, _, _, cx| {
                            if let Some(source) = v
                                .extensions
                                .sources
                                .get(v.extensions.selected_source)
                                .cloned()
                            {
                                v.extension_request(
                                    "extension_discover",
                                    json!({"source":source}),
                                    cx,
                                );
                            }
                        })),
                ),
        );
        let query = self.extensions.query.read(cx).value().to_lowercase();
        for (index, item) in self
            .extensions
            .discovered
            .iter()
            .filter(|p| {
                p.name.to_lowercase().contains(&query) || p.path.to_lowercase().contains(&query)
            })
            .enumerate()
        {
            let item = item.clone();
            let installed = self.extensions.packages.iter().any(|p| {
                p.source.ends_with(&format!("/{}", item.path))
                    && p.source.contains(&item.source.repository)
            });
            body = body.child(
                div()
                    .py_3()
                    .border_b_1()
                    .border_color(rgb(border()))
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
                            .child(div().text_sm().child(item.name.clone()))
                            .child(subtitle(item.source.name.clone())),
                    )
                    .child(
                        button(
                            ("extension-install", index),
                            if installed { "Update" } else { "Install" },
                        )
                        .disabled(busy)
                        .on_click(cx.listener(move |v, _, _, cx| {
                            v.extension_request(
                                "extension_install_remote",
                                json!({"item":item}),
                                cx,
                            )
                        })),
                    ),
            );
        }
        if self.extensions.discovered.is_empty() && !busy {
            body = body.child(subtitle("Choose a source to browse plugins and skills."));
        }
        body.into_any_element()
    }
    fn extension_installed_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.extensions.pending.is_some();
        let mut body = div().flex().flex_col().gap_3().child(
            div()
                .flex()
                .gap_2()
                .child(
                    button("extensions-refresh", "Refresh")
                        .disabled(busy)
                        .on_click(cx.listener(|v, _, _, cx| {
                            v.extension_request("extension_snapshot", json!({}), cx)
                        })),
                )
                .child(
                    button("extensions-native", "Native modules").on_click(cx.listener(
                        |v, _, _, cx| {
                            v.extensions.native = true;
                            cx.notify();
                        },
                    )),
                ),
        );
        if self.extensions.packages.is_empty() && !busy {
            body = body.child(subtitle(if self.extensions.loaded {
                "No plugins installed. Browse Discover or install a folder."
            } else {
                "Could not load plugins. Refresh to try again."
            }));
        }
        for (index, p) in self.extensions.packages.iter().enumerate() {
            let id = p.id.clone();
            let remove = id.clone();
            let enabled = p.enabled;
            let mut row = div()
                .py_4()
                .border_b_1()
                .border_color(rgb(border()))
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child(p.name.clone()),
                        )
                        .child(
                            button(
                                ("extension-enable", index),
                                if enabled { "Disable" } else { "Enable" },
                            )
                            .disabled(busy)
                            .on_click(cx.listener(
                                move |v, _, _, cx| {
                                    v.extension_request(
                                        "extension_enable",
                                        json!({"id":id,"enabled":!enabled}),
                                        cx,
                                    )
                                },
                            )),
                        )
                        .child(
                            button(("extension-remove", index), "Remove")
                                .disabled(busy)
                                .on_click(cx.listener(move |v, _, _, cx| {
                                    v.extension_request(
                                        "extension_remove",
                                        json!({"id":remove}),
                                        cx,
                                    )
                                })),
                        ),
                );
            for (component_index, c) in p.components.iter().enumerate() {
                let mut component = div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().flex_1().text_size(px(12.)).child(match c.kind {
                        ComponentKind::Mcp => "MCP servers".to_owned(),
                        ComponentKind::LanguageServer => "Language servers".to_owned(),
                        ComponentKind::Hooks => "Hooks".to_owned(),
                        ComponentKind::HostedIntegration => "Hosted integrations".to_owned(),
                        _ => c.name.clone(),
                    }))
                    .child(subtitle(c.readiness.label()));
                if matches!(
                    c.kind,
                    ComponentKind::Hooks | ComponentKind::Mcp | ComponentKind::LanguageServer
                ) {
                    let package = p.clone();
                    let selected = c.clone();
                    component = component.child(
                        button(
                            ("extension-configure", index * 1000 + component_index),
                            "Configure",
                        )
                        .disabled(busy || !enabled)
                        .on_click(cx.listener(move |v, _, w, cx| {
                            v.extension_open_setup(
                                &package,
                                &selected.name,
                                selected.kind.clone(),
                                w,
                                cx,
                            )
                        })),
                    );
                }
                row = row.child(component);
                for requirement in &c.requirements {
                    if requirement == &format!("Configure and verify {}", c.name) {
                        continue;
                    }
                    row = row.child(subtitle(requirement.clone()));
                }
            }
            body = body.child(row);
        }
        body.into_any_element()
    }
    fn extension_sources_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.extensions.pending.is_some();
        let mut body = div().flex().flex_col().gap_3();
        for (index, source) in self.extensions.sources.iter().enumerate() {
            body = body.child(
                div()
                    .py_3()
                    .border_b_1()
                    .border_color(rgb(border()))
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_sm().child(source.name.clone()))
                            .child(subtitle(format!(
                                "{} · {}",
                                source.repository, source.revision
                            ))),
                    )
                    .child(
                        button(("extension-source-remove", index), "Remove")
                            .disabled(busy)
                            .on_click(cx.listener(move |v, _, _, cx| {
                                let mut sources = v.extensions.sources.clone();
                                sources.remove(index);
                                v.extension_request(
                                    "extension_sources",
                                    json!({"sources":sources}),
                                    cx,
                                );
                            })),
                    ),
            );
        }
        body.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().flex_1().child(Input::new(&self.extensions.source)))
                .child(
                    button("extension-source-add", "Add source")
                        .disabled(busy)
                        .on_click(cx.listener(|v, _, _, cx| {
                            match ExtensionSource::from_input(&v.extensions.source.read(cx).value())
                            {
                                Ok(source) => {
                                    let mut sources = v.extensions.sources.clone();
                                    if sources.iter().any(|s| {
                                        s.repository == source.repository
                                            && s.revision == source.revision
                                            && s.subdirectory == source.subdirectory
                                    }) {
                                        v.extensions.notice =
                                            "This source is already added.".into();
                                        cx.notify();
                                        return;
                                    }
                                    sources.push(source);
                                    v.extension_request(
                                        "extension_sources",
                                        json!({"sources":sources}),
                                        cx,
                                    );
                                }
                                Err(error) => {
                                    v.extensions.notice = error.to_string();
                                    cx.notify();
                                }
                            }
                        })),
                ),
        )
        .into_any_element()
    }
}
