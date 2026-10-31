//! The Native modules sub-page of Plugins: reviewed packages, grants and
//! explicit runs.
use super::*;
use crate::motion;
impl Xylo {
    pub(in super::super) fn native_plugins_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.plugins.busy();
        let settings_blocked = busy || self.plugins.retry.is_some();
        let mut page = settings_ui::settings_page("native-modules-settings", "Native modules")
            .description("Xylo's own sandboxed plugins. Install reviewed tools and choose what they can access.")
            .breadcrumb(self.native_modules_breadcrumb(cx))
            .action(
                settings_ui::busy(
                    settings_ui::primary_button("plugin-choose", "Choose plugin package")
                        .disabled(settings_blocked),
                    self.plugins.picking || self.plugins.pending_tool("helper_plugin_inspect"),
                )
                    .on_click(cx.listener(|view, _, window, cx| view.plugin_pick(window, cx))),
            );
        if !self.plugins.notice.is_empty() || !self.plugins.pending.is_empty() {
            let mut status = div().flex().flex_col().gap(px(8.));
            if !self.plugins.pending.is_empty() {
                status = status.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(motion::spinner_inline())
                        .child(settings_ui::caption("Working…"))
                        .child(
                            settings_ui::text_button("plugin-cancel", "Cancel request")
                                .on_click(cx.listener(|view, _, _, cx| view.plugin_cancel(cx))),
                        )
                        .children(
                            self.plugins
                                .pending_since
                                .and_then(settings_ui::slow_caption),
                        ),
                );
            }
            if !self.plugins.notice.is_empty() {
                status = status.child(notice(BadgeTone::Neutral, self.plugins.notice.clone()));
            }
            page = page.child(status);
        }
        if self.plugins.retry.is_some() {
            page = page.child(settings_ui::section(
                "Previous change",
                Some("A previous settings change needs checking. Cancelling its request did not undo a change already saved.".into()),
                settings_ui::form_actions([
                    settings_ui::text_button("plugin-dismiss-change", "Dismiss local request")
                        .disabled(busy)
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.plugin_dismiss_change(window, cx)
                        }))
                        .into_any_element(),
                    settings_ui::secondary_button("plugin-check-change", "Check previous change")
                        .disabled(busy)
                        .on_click(cx.listener(|view, _, _, cx| view.plugin_retry_change(cx)))
                        .into_any_element(),
                ]),
            ));
        }
        if let Some(review) = self.native_review_section(busy, cx) {
            page = page.child(review);
        }
        page = page.child(settings_ui::section(
            "Installed modules",
            None,
            self.native_installed_list(busy, cx),
        ));
        if let Some(selected) = self.native_selected_sections(busy, settings_blocked, cx) {
            page = page.children(selected);
        }
        page.into_any_element()
    }
    /// The package under review: its permissions and the install action.
    fn native_review_section(&self, busy: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        let review = self.plugins.inspection.as_ref()?;
        let replacing = self.plugins.catalog.as_ref().is_some_and(|catalog| {
            catalog
                .plugins
                .iter()
                .any(|plugin| plugin.manifest.id == review.manifest.id)
        });
        let file = std::path::Path::new(&review.manifest.entrypoint)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mut rows: Vec<AnyElement> = review
            .manifest
            .capabilities
            .iter()
            .enumerate()
            .map(|(index, permission)| {
                let label = permission_label(permission);
                let permission = permission.clone();
                settings_ui::setting_row(
                    label.clone(),
                    None,
                    Some(
                        settings_ui::switch(
                            ("plugin-review-grant", index),
                            label,
                            self.plugins.review_grants.contains(&permission),
                        )
                        .disabled(busy)
                        .on_change(cx.listener(move |view, checked: &bool, _, cx| {
                            if *checked {
                                view.plugins.review_grants.insert(permission.clone());
                            } else {
                                view.plugins.review_grants.remove(&permission);
                            }
                            view.plugins.generation = view.plugins.generation.saturating_add(1);
                            view.cancel_setup_close_intents(cx);
                            cx.notify();
                        }))
                        .into_any_element(),
                    ),
                )
                .into_any_element()
            })
            .collect();
        if rows.is_empty() {
            rows.push(
                settings_ui::setting_row(
                    "Permissions",
                    Some("This plugin requests no file or website permissions.".into()),
                    None,
                )
                .into_any_element(),
            );
        }
        rows.push(
            settings_ui::setting_row(
                "Enable after installing",
                None,
                Some(
                    settings_ui::switch(
                        "plugin-enable-install",
                        "Enable after installing",
                        self.plugins.enable_install,
                    )
                    .disabled(busy)
                    .on_change(cx.listener(|view, checked: &bool, _, cx| {
                        view.plugins.enable_install = *checked;
                        view.plugins.generation = view.plugins.generation.saturating_add(1);
                        view.cancel_setup_close_intents(cx);
                        cx.notify();
                    }))
                    .into_any_element(),
                ),
            )
            .into_any_element(),
        );
        Some(
            settings_ui::section(
                format!("Review {} {}", review.manifest.name, review.manifest.version),
                Some(format!(
                    "Plugin file: {file}. Allow only the permissions you want this plugin to use; you can revoke them later."
                ).into()),
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(settings_ui::rows(rows))
                    .child(settings_ui::form_actions([
                        settings_ui::text_button("plugin-dismiss-review", "Discard package review")
                            .disabled(busy)
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.plugins.inspection = None;
                                view.plugins.manifest = None;
                                view.plugins.review_grants.clear();
                                view.plugins.generation = view.plugins.generation.saturating_add(1);
                                cx.notify();
                            }))
                            .into_any_element(),
                        settings_ui::busy(
                            settings_ui::primary_button(
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
                        ),
                            self.plugins.pending_tool("helper_plugin_install"),
                        )
                        .on_click(cx.listener(|view, _, window, cx| view.plugin_install(window, cx)))
                        .into_any_element(),
                    ])),
            )
            .into_any_element(),
        )
    }
    fn native_installed_list(&self, busy: bool, cx: &mut Context<Self>) -> AnyElement {
        let Some(catalog) = &self.plugins.catalog else {
            return if busy {
                settings_ui::loading_list(2, self.plugins.pending_since).into_any_element()
            } else {
                settings_ui::inline_error(
                    "Could not load native modules. Reopen Plugins to try again.",
                )
                .into_any_element()
            };
        };
        if catalog.plugins.is_empty() {
            return settings_ui::empty_state(
                AppIcon::Empty,
                "No native modules installed",
                "Choose a plugin package to review what it can access, then install it.",
                None,
            )
            .into_any_element();
        }
        let rows = catalog.plugins.clone();
        settings_ui::list_frame()
            .child(
                uniform_list(
                    "plugin-installed",
                    rows.len(),
                    cx.processor(move |view, range: std::ops::Range<usize>, _window, cx| {
                        range
                            .filter_map(|index| {
                                let plugin = rows.get(index)?.clone();
                                let id = plugin.manifest.id.clone();
                                let selected = view
                                    .plugins
                                    .selected
                                    .as_ref()
                                    .is_some_and(|selected| selected.manifest.id == id);
                                Some(
                                    settings_ui::list_item(
                                        ("plugin-select", index),
                                        Some(
                                            settings_ui::monogram(&plugin.manifest.name, &id)
                                                .into_any_element(),
                                        ),
                                        plugin.manifest.name.clone(),
                                        Some(format!("Version {}", plugin.manifest.version).into()),
                                    )
                                    .hover_detail(id.clone())
                                    .divider(index > 0)
                                    .selected(selected)
                                    .trailing(settings_ui::status_pill(if plugin.enabled {
                                        settings_ui::Status::Ready
                                    } else {
                                        settings_ui::Status::Disabled
                                    }))
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        if !view.plugins.busy() {
                                            view.plugin_select(id.clone(), window, cx)
                                        }
                                    }))
                                    .into_any_element(),
                                )
                            })
                            .collect()
                    }),
                )
                .h(px(
                    catalog.plugins.len().min(6) as f32 * settings_ui::LIST_ITEM_HEIGHT
                )),
            )
            .into_any_element()
    }
    /// Settings, run and output of the selected module, then its removal.
    fn native_selected_sections(
        &self,
        busy: bool,
        settings_blocked: bool,
        cx: &mut Context<Self>,
    ) -> Option<Vec<AnyElement>> {
        let plugin = self.plugins.selected.as_ref()?;
        let changed = self.plugins.selected_changed();
        let mut rows = vec![
            settings_ui::setting_row(
                "Enabled",
                Some("A disabled module keeps its permissions but cannot run.".into()),
                Some(
                    settings_ui::switch("plugin-enabled", "Enabled", plugin.enabled)
                        .disabled(settings_blocked || changed || self.plugins.grants_dirty())
                        .on_change(cx.listener(|view, checked: &bool, _, cx| {
                            view.plugin_set_enabled(*checked, cx)
                        }))
                        .into_any_element(),
                ),
            )
            .into_any_element(),
        ];
        if changed {
            let id = plugin.manifest.id.clone();
            rows.push(
                settings_ui::setting_row(
                    "Changed elsewhere",
                    Some("This plugin changed in another window. Reload it before changing permissions or running it.".into()),
                    Some(
                        settings_ui::secondary_button("plugin-reload-selection", "Reload plugin")
                            .disabled(busy)
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.plugin_select(id.clone(), window, cx)
                            }))
                            .into_any_element(),
                    ),
                )
                .into_any_element(),
            );
        }
        for (index, permission) in plugin.manifest.capabilities.iter().enumerate() {
            let label = permission_label(permission);
            let permission = permission.clone();
            rows.push(
                settings_ui::setting_row(
                    label.clone(),
                    None,
                    Some(
                        settings_ui::switch(
                            ("plugin-grant", index),
                            label,
                            self.plugins.grants.contains(&permission),
                        )
                        .disabled(settings_blocked || changed)
                        .on_change(cx.listener(move |view, checked: &bool, _, cx| {
                            if *checked {
                                view.plugins.grants.insert(permission.clone());
                            } else {
                                view.plugins.grants.remove(&permission);
                            }
                            view.plugins.generation = view.plugins.generation.saturating_add(1);
                            view.cancel_setup_close_intents(cx);
                            cx.notify();
                        }))
                        .into_any_element(),
                    ),
                )
                .into_any_element(),
            );
        }
        let settings = settings_ui::section(
            plugin.manifest.name.clone(),
            Some(format!("Version {}", plugin.manifest.version).into()),
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(settings_ui::rows(rows))
                .child(settings_ui::form_actions([
                    settings_ui::text_button("plugin-revoke", "Revoke all permissions")
                        .disabled(settings_blocked || plugin.grants.is_empty())
                        .on_click(
                            cx.listener(|view, _, window, cx| {
                                view.plugin_confirm(false, window, cx)
                            }),
                        )
                        .into_any_element(),
                    settings_ui::busy(
                        settings_ui::primary_button("plugin-save-grants", "Save permissions")
                            .disabled(settings_blocked || changed || !self.plugins.grants_dirty()),
                        self.plugins.pending_tool("helper_plugin_set_grants"),
                    )
                    .on_click(cx.listener(|view, _, _, cx| view.plugin_save_grants(cx)))
                    .into_any_element(),
                ])),
        );
        let project = self
            .workspace
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned());
        let run_blocked = busy
            || !plugin.enabled
            || changed
            || self.plugins.grants_dirty()
            || !self.workspace_ready;
        let mut run = settings_ui::form_column([
            settings_ui::setting_row(
                "Project",
                Some(
                    project
                        .clone()
                        .unwrap_or_else(|| "Open a project to run this plugin.".into())
                        .into(),
                ),
                None,
            )
            .into_any_element(),
            settings_ui::field(
                "Input",
                Textarea::new(&self.plugins.input).aria_label("Plugin input"),
                None,
            )
            .into_any_element(),
            settings_ui::form_actions([settings_ui::busy(
                settings_ui::secondary_button("plugin-run", "Run plugin").disabled(run_blocked),
                self.plugins.pending_tool("plugin_run_installed"),
            )
            .on_click(cx.listener(|view, _, _, cx| view.plugin_run(cx)))
            .into_any_element()])
            .into_any_element(),
        ]);
        if !self.plugins.output.read(cx).value().is_empty() {
            run = run.child(settings_ui::field(
                "Output",
                Textarea::new(&self.plugins.output)
                    .readonly(true)
                    .aria_label("Plugin output"),
                None,
            ));
        }
        if !self.plugins.artifacts.is_empty() {
            run = run.child(settings_ui::list_container(
                self.plugins
                    .artifacts
                    .iter()
                    .enumerate()
                    .map(|(index, path)| {
                        let label = path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        let open = path.clone();
                        settings_ui::list_item(
                            ("plugin-artifact", index),
                            Some(settings_ui::monogram(&label, &label).into_any_element()),
                            label,
                            None,
                        )
                        .on_click(move |_, _, cx| cx.open_with_system(&open))
                        .into_any_element()
                    }),
            ));
        }
        let remove = settings_ui::danger_zone([settings_ui::setting_row(
            "Remove module",
            Some(
                "It will no longer be available. Files it already created stay in Artifacts."
                    .into(),
            ),
            Some(
                settings_ui::busy(
                    settings_ui::destructive_button("plugin-remove", "Remove").disabled(busy),
                    self.plugins.pending_tool("helper_plugin_remove"),
                )
                .on_click(cx.listener(|view, _, window, cx| view.plugin_confirm(true, window, cx)))
                .into_any_element(),
            ),
        )
        .inset()
        .into_any_element()]);
        Some(vec![
            settings.into_any_element(),
            settings_ui::section("Run", None, run).into_any_element(),
            remove.into_any_element(),
        ])
    }
}
