//! Product settings for reviewed packages, grants and explicit plugin runs.
use super::*;
use gpui_kit::component::{Disableable, switch::Switch};
impl Xylo {
    pub(in super::super) fn plugins_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.plugins.busy();
        let settings_blocked = busy || self.plugins.retry.is_some();
        let mut page = div()
            .id("plugins-page")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(heading(
                "Plugins",
                "Install reviewed tools and choose what they can access.",
            ))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        primary("plugin-choose", "Choose plugin package")
                            .disabled(settings_blocked)
                            .on_click(
                                cx.listener(|view, _, window, cx| view.plugin_pick(window, cx)),
                            ),
                    )
                    .child(
                        button("plugin-refresh", "Refresh installed plugins")
                            .disabled(busy)
                            .on_click(cx.listener(|view, _, _, cx| view.refresh_plugins(cx))),
                    ),
            );
        if !self.plugins.notice.is_empty() {
            page = page.child(subtitle(self.plugins.notice.clone()));
        }
        if !self.plugins.pending.is_empty() {
            page = page.child(
                button("plugin-cancel", "Cancel request")
                    .on_click(cx.listener(|view, _, _, cx| view.plugin_cancel(cx))),
            );
        }
        if self.plugins.retry.is_some() {
            page = page.child(
                card().max_w(px(860.))
                    .child(subtitle("A previous settings change needs checking. Cancelling its request did not undo a change already saved."))
                    .child(div().flex().gap_2()
                        .child(button("plugin-check-change", "Check previous change")
                            .disabled(busy)
                            .on_click(cx.listener(|view, _, _, cx| view.plugin_retry_change(cx))))
                        .child(button("plugin-dismiss-change", "Dismiss local request")
                            .disabled(busy)
                            .on_click(cx.listener(|view, _, window, cx| view.plugin_dismiss_change(window, cx)))))
            );
        }
        if let Some(review) = &self.plugins.inspection {
            let replacing = self.plugins.catalog.as_ref().is_some_and(|catalog| {
                catalog
                    .plugins
                    .iter()
                    .any(|plugin| plugin.manifest.id == review.manifest.id)
            });
            let mut card = card().max_w(px(860.)).gap_3()
                .child(div().text_lg().child(format!("{} · {}", review.manifest.name, review.manifest.version)))
                .child(subtitle(format!("Plugin file: {}", std::path::Path::new(&review.manifest.entrypoint)
                    .file_name().unwrap_or_default().to_string_lossy())))
                .child(subtitle("Allow only the permissions you want this plugin to use. These choices can be revoked later."));
            if review.manifest.capabilities.is_empty() {
                card = card.child(subtitle(
                    "This plugin requests no file or website permissions.",
                ));
            }
            for (index, permission) in review.manifest.capabilities.iter().enumerate() {
                let permission = permission.clone();
                card = card.child(
                    Switch::new(("plugin-review-grant", index))
                        .label(permission_label(&permission))
                        .checked(self.plugins.review_grants.contains(&permission))
                        .disabled(busy)
                        .on_change(cx.listener(move |view, checked, _, cx| {
                            if *checked {
                                view.plugins.review_grants.insert(permission.clone());
                            } else {
                                view.plugins.review_grants.remove(&permission);
                            }
                            view.plugins.generation = view.plugins.generation.saturating_add(1);
                            view.cancel_setup_close_intents(cx);
                            cx.notify();
                        })),
                );
            }
            card = card
                .child(
                    Switch::new("plugin-enable-install")
                        .label("Enable after installing")
                        .checked(self.plugins.enable_install)
                        .disabled(busy)
                        .on_change(cx.listener(|view, checked, _, cx| {
                            view.plugins.enable_install = *checked;
                            view.plugins.generation = view.plugins.generation.saturating_add(1);
                            view.cancel_setup_close_intents(cx);
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            primary(
                                "plugin-install",
                                if replacing {
                                    "Update plugin"
                                } else {
                                    "Install plugin"
                                },
                            )
                            .disabled(
                                busy || self.plugins.retry.is_some()
                                    || self.plugins.catalog.is_none()
                                    || self.plugins.grants_dirty(),
                            )
                            .on_click(
                                cx.listener(|view, _, window, cx| view.plugin_install(window, cx)),
                            ),
                        )
                        .child(
                            button("plugin-dismiss-review", "Discard package review")
                                .disabled(busy)
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.plugins.inspection = None;
                                    view.plugins.manifest = None;
                                    view.plugins.review_grants.clear();
                                    view.plugins.generation =
                                        view.plugins.generation.saturating_add(1);
                                    cx.notify();
                                })),
                        ),
                );
            page = page.child(card);
        }
        let rows = self
            .plugins
            .catalog
            .as_ref()
            .map(|catalog| catalog.plugins.clone())
            .unwrap_or_default();
        let mut installed = card()
            .max_w(px(860.))
            .gap_2()
            .child(div().text_lg().child("Installed plugins"));
        if self.plugins.catalog.is_none() {
            installed = installed.child(subtitle("Loading installed plugins…"));
        } else if rows.is_empty() {
            installed = installed.child(subtitle(
                "No plugins installed. Choose a package to review it.",
            ));
        } else {
            installed = installed.child(
                uniform_list(
                    "plugin-installed",
                    rows.len(),
                    cx.processor(move |view, range: std::ops::Range<usize>, _window, cx| {
                        range
                            .map(|index| {
                                let plugin = rows[index].clone();
                                let id = plugin.manifest.id.clone();
                                button(
                                    ("plugin-select", index),
                                    format!(
                                        "{} · {} · {}",
                                        plugin.manifest.name,
                                        plugin.manifest.version,
                                        if plugin.enabled {
                                            "Enabled"
                                        } else {
                                            "Disabled"
                                        }
                                    ),
                                )
                                .w_full()
                                .justify_start()
                                .disabled(view.plugins.busy())
                                .when(
                                    view.plugins
                                        .selected
                                        .as_ref()
                                        .is_some_and(|selected| selected.manifest.id == id),
                                    |button| button.bg(rgb(lavender())),
                                )
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.plugin_select(id.clone(), window, cx)
                                }))
                                .into_any_element()
                            })
                            .collect()
                    }),
                )
                .h(px(180.)),
            );
        }
        page = page.child(installed);
        if let Some(plugin) = &self.plugins.selected {
            let changed = self.plugins.selected_changed();
            let mut settings = card()
                .max_w(px(860.))
                .gap_3()
                .child(div().text_lg().child(plugin.manifest.name.clone()))
                .child(
                    Switch::new("plugin-enabled")
                        .label("Enabled")
                        .checked(plugin.enabled)
                        .disabled(settings_blocked || changed || self.plugins.grants_dirty())
                        .on_change(cx.listener(|view, checked, _, cx| {
                            view.plugin_set_enabled(*checked, cx)
                        })),
                );
            if changed {
                let id = plugin.manifest.id.clone();
                settings = settings
                    .child(subtitle("This plugin changed in another window. Reload it before changing permissions or running it."))
                    .child(button("plugin-reload-selection", "Reload plugin").disabled(busy)
                        .on_click(cx.listener(move |view, _, window, cx| view.plugin_select(id.clone(), window, cx))));
            }
            for (index, permission) in plugin.manifest.capabilities.iter().enumerate() {
                let permission = permission.clone();
                settings = settings.child(
                    Switch::new(("plugin-grant", index))
                        .label(permission_label(&permission))
                        .checked(self.plugins.grants.contains(&permission))
                        .disabled(settings_blocked || changed)
                        .on_change(cx.listener(move |view, checked, _, cx| {
                            if *checked {
                                view.plugins.grants.insert(permission.clone());
                            } else {
                                view.plugins.grants.remove(&permission);
                            }
                            view.plugins.generation = view.plugins.generation.saturating_add(1);
                            view.cancel_setup_close_intents(cx);
                            cx.notify();
                        })),
                );
            }
            settings = settings.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        primary("plugin-save-grants", "Save permissions")
                            .disabled(settings_blocked || changed || !self.plugins.grants_dirty())
                            .on_click(cx.listener(|view, _, _, cx| view.plugin_save_grants(cx))),
                    )
                    .child(
                        button("plugin-revoke", "Revoke all permissions")
                            .disabled(settings_blocked || plugin.grants.is_empty())
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.plugin_confirm(false, window, cx)
                            })),
                    )
                    .child(
                        button("plugin-remove", "Remove plugin")
                            .disabled(busy)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.plugin_confirm(true, window, cx)
                            })),
                    ),
            );
            let project = self
                .workspace
                .as_ref()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Open a project to run this plugin".into());
            settings = settings
                .child(subtitle(format!("Project: {project}")))
                .child(Textarea::new(&self.plugins.input).aria_label("Plugin input"))
                .child(
                    primary("plugin-run", "Run plugin")
                        .disabled(
                            busy || !plugin.enabled
                                || changed
                                || self.plugins.grants_dirty()
                                || !self.workspace_ready,
                        )
                        .on_click(cx.listener(|view, _, _, cx| view.plugin_run(cx))),
                );
            page = page.child(settings);
        }
        if !self.plugins.output.read(cx).value().is_empty() {
            page = page.child(
                card()
                    .max_w(px(860.))
                    .child(div().text_lg().child("Output"))
                    .child(
                        Textarea::new(&self.plugins.output)
                            .readonly(true)
                            .aria_label("Plugin output"),
                    ),
            );
        }
        for (index, path) in self.plugins.artifacts.iter().enumerate() {
            let path = path.clone();
            let label = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            page = page.child(
                button(("plugin-artifact", index), label)
                    .on_click(move |_, _, cx| cx.open_with_system(&path)),
            );
        }
        page.into_any_element()
    }
}
