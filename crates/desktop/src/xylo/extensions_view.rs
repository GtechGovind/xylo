//! The Plugins settings page: Discover, Installed and Sources tabs, a detail
//! sub-page per plugin, and the review shown before a package is enabled.
use super::*;
use crate::motion;
use std::rc::Rc;

/// Rows a list shows before it scrolls inside its own frame.
const VISIBLE_ROWS: usize = 9;

/// Tone of an installed component's readiness label.
fn readiness_tone(label: &str) -> settings_ui::Tone {
    match label {
        "Needs setup" => settings_ui::Tone::Warning,
        "Unsupported requirement" => settings_ui::Tone::Danger,
        _ => settings_ui::Tone::Success,
    }
}
/// The pill for an installed package: disabled, an update, unmet setup, or
/// ready.
fn package_pill(package: &ExtensionPackage, update: bool) -> Div {
    let labels: Vec<&str> = package
        .components
        .iter()
        .map(|component| component.readiness.label())
        .collect();
    if !package.enabled {
        settings_ui::status_pill(settings_ui::Status::Disabled)
    } else if update {
        settings_ui::status_pill(settings_ui::Status::UpdateAvailable)
    } else if labels.contains(&"Unsupported requirement") {
        settings_ui::pill("Unsupported", settings_ui::Tone::Danger)
    } else if labels.contains(&"Needs setup") {
        settings_ui::status_pill(settings_ui::Status::NeedsSetup)
    } else {
        settings_ui::status_pill(settings_ui::Status::Ready)
    }
}
/// Short tag for a directory component kind.
fn directory_tag(kind: &ComponentKind) -> &'static str {
    match kind {
        ComponentKind::Skill => "Skills",
        ComponentKind::Command => "Commands",
        ComponentKind::Agent => "Agents",
        ComponentKind::Hooks => "Hooks",
        ComponentKind::Mcp => "MCP",
        ComponentKind::LanguageServer => "Language server",
        ComponentKind::HostedIntegration => "Integration",
    }
}
fn short_revision(revision: &str) -> &str {
    &revision[..revision.len().min(7)]
}
/// The repository of a source status, for matching the source filter.
fn status_repository(status: &DirectorySourceStatus) -> &str {
    &status.source.repository
}
fn list_height(rows: usize) -> Pixels {
    px(rows.min(VISIBLE_ROWS) as f32 * settings_ui::LIST_ITEM_HEIGHT)
}

impl Xylo {
    pub(in super::super) fn plugins_page(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .size_full()
            .track_focus(&self.extensions.focus)
            .child(self.plugins_sub_page(cx))
            .into_any_element()
    }
    fn plugins_sub_page(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.extensions.native {
            return self.native_plugins_page(cx);
        }
        if self.extensions.setup.is_some() {
            return self.extension_setup_view(cx);
        }
        if let Some(package) = &self.extensions.review {
            return self.extension_review_page(package, cx);
        }
        match &self.extensions.detail {
            Some(Detail::Available(item)) => return self.extension_available_page(item, cx),
            Some(Detail::Installed(id)) => {
                if let Some((index, package)) = self
                    .extensions
                    .packages
                    .iter()
                    .enumerate()
                    .find(|(_, package)| &package.id == id)
                {
                    return self.extension_installed_page(index, package, cx);
                }
            }
            None => {}
        }
        let busy = self.extensions.pending.is_some();
        let weak = cx.entity().downgrade();
        let tabs = [
            "Discover".to_owned(),
            format!("Installed ({})", self.extensions.packages.len()),
            "Sources".to_owned(),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, label)| {
            let weak = weak.clone();
            settings_ui::SelectOption::new(
                label,
                self.extensions.tab == index,
                move |window, cx| {
                    let _ = weak.update(cx, |view, cx| {
                        view.extension_leave_setup(Some(index), window, cx)
                    });
                },
            )
        })
        .collect();
        let mut page = settings_ui::settings_page("extensions-page", "Plugins")
            .description("Add skills, commands, agents and tool connections to Xylo.")
            .action(
                settings_ui::busy(
                    settings_ui::secondary_button("extensions-local", "Install from folder")
                        .disabled(busy),
                    self.extension_busy("extension_install_local", None),
                )
                .on_click(cx.listener(|view, _, window, cx| view.extension_pick(window, cx))),
            )
            .child(settings_ui::segmented("extensions-tab", tabs));
        if let Some(status) = self.extension_status(cx) {
            page = page.child(status);
        }
        page.child(match self.extensions.tab {
            DISCOVER => self.extension_discover_view(cx),
            INSTALLED => self.extension_installed_view(cx),
            _ => self.extension_sources_view(cx),
        })
        .into_any_element()
    }
    /// The in-flight package operation with its Cancel, and the last notice.
    pub(super) fn extension_status(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        // Loading installed plugins shows in the lists themselves.
        let busy = self.extensions.pending.is_some()
            && self.extensions.pending_name != "extension_snapshot";
        if !busy && self.extensions.notice.is_empty() {
            return None;
        }
        let mut status = div().flex().flex_col().gap(px(8.));
        if busy {
            status = status.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(motion::spinner_inline())
                    .child(settings_ui::caption(
                        if self.extensions.pending_name == "extension_component_verify" {
                            "Verifying…"
                        } else {
                            "Working…"
                        },
                    ))
                    .child(
                        settings_ui::text_button("extension-cancel", "Cancel").on_click(
                            cx.listener(|view, _, _, cx| {
                                if let Some(id) = view.extensions.pending
                                    && let Some(host) = &view.host
                                {
                                    view.extensions.notice = match host.cancel_capability(id) {
                                        Ok(()) => {
                                            view.extensions.cancelling = true;
                                            "Cancellation requested. Waiting for the operation's result.".into()
                                        }
                                        Err(error) => error.to_string(),
                                    };
                                    cx.notify();
                                }
                            }),
                        ),
                    )
                    .children(
                        self.extensions
                            .pending_since
                            .and_then(settings_ui::slow_caption),
                    ),
            );
        }
        if !self.extensions.notice.is_empty() {
            status = status.child(
                div()
                    .id("extensions-notice")
                    .child(notice(BadgeTone::Neutral, self.extensions.notice.clone())),
            );
        }
        Some(status.into_any_element())
    }
    /// "Plugins / <current>" for sub-pages; the crumb returns to the tabs.
    fn plugins_breadcrumb(
        &self,
        id: &'static str,
        current: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Div {
        settings_ui::breadcrumb(
            [
                settings_ui::crumb(id, "Plugins").on_click(cx.listener(|view, _, window, cx| {
                    view.focus_plugins_page(window, cx);
                    view.extensions.detail = None;
                    view.extensions.review = None;
                    view.extensions.native = false;
                    cx.notify();
                })),
            ],
            current,
        )
    }

    // -----------------------------------------------------------------------
    // Discover
    // -----------------------------------------------------------------------

    fn extension_discover_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let directory = &self.extensions.directory;
        let weak = cx.entity().downgrade();
        let choose = |weak: &WeakEntity<Self>, apply: Box<dyn Fn(&mut Xylo)>| {
            let weak = weak.clone();
            move |_: &mut Window, cx: &mut App| {
                let _ = weak.update(cx, |view, cx| {
                    apply(view);
                    view.extensions.directory.limit = DIRECTORY_PAGE;
                    view.extension_directory_search(cx);
                });
            }
        };
        let mut sources = vec![settings_ui::SelectOption::new(
            "All sources",
            self.extensions.source_filter.is_none(),
            choose(&weak, Box::new(|view| view.extensions.source_filter = None)),
        )];
        for source in &self.extensions.sources {
            let repository = source.repository.clone();
            sources.push(settings_ui::SelectOption::new(
                source.name.clone(),
                self.extensions.source_filter.as_ref() == Some(&repository),
                choose(
                    &weak,
                    Box::new(move |view| view.extensions.source_filter = Some(repository.clone())),
                ),
            ));
        }
        let mut toolbar = div()
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(div().flex_1().min_w_0().child(settings_ui::search_field(
                &self.extensions.query,
                "Search plugins and skills",
            )))
            .child(settings_ui::select(
                "extension-source-filter",
                "Source",
                sources,
            ));
        let categories = directory
            .page
            .as_ref()
            .map(|page| page.categories.clone())
            .unwrap_or_default();
        if !categories.is_empty() || directory.category.is_some() {
            let mut options = vec![settings_ui::SelectOption::new(
                "All categories",
                directory.category.is_none(),
                choose(
                    &weak,
                    Box::new(|view| view.extensions.directory.category = None),
                ),
            )];
            for category in categories {
                let chosen = category.clone();
                options.push(settings_ui::SelectOption::new(
                    category.clone(),
                    directory.category.as_ref() == Some(&category),
                    choose(
                        &weak,
                        Box::new(move |view| {
                            view.extensions.directory.category = Some(chosen.clone())
                        }),
                    ),
                ));
            }
            toolbar = toolbar.child(settings_ui::select(
                "extension-category-filter",
                "Category",
                options,
            ));
        }
        let shown: Vec<&DirectorySourceStatus> = directory
            .sources
            .iter()
            .filter(|status| {
                self.extensions
                    .source_filter
                    .as_deref()
                    .is_none_or(|filter| filter == status_repository(status))
            })
            .collect();
        let refreshing = directory.refresh.is_some();
        let freshness = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .child(settings_ui::caption(freshness_caption(&shown, refreshing)))
            .child(
                settings_ui::busy(
                    settings_ui::text_button("extensions-refresh", "Refresh"),
                    refreshing,
                )
                .on_click(cx.listener(|view, _, _, cx| view.extension_directory_refresh(true, cx))),
            )
            .children(
                directory
                    .refresh_since
                    .filter(|_| refreshing)
                    .and_then(settings_ui::slow_caption),
            );
        let mut body = div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(toolbar)
            .child(freshness);
        for message in shown.iter().filter_map(|status| status.message.clone()) {
            body = body.child(notice(BadgeTone::Warning, message));
        }
        if let Some(error) = &directory.error {
            body = body.child(settings_ui::inline_error(error));
        }
        let entries = directory
            .page
            .as_ref()
            .map(|page| page.entries.clone())
            .unwrap_or_default();
        let total = directory.page.as_ref().map_or(0, |page| page.total);
        let loading = entries.is_empty()
            && (directory.page.is_none()
                || (refreshing && shown.iter().all(|status| status.fetched_at.is_none())));
        body.child(if loading {
            settings_ui::loading_list(4, directory.since.or(directory.refresh_since))
                .into_any_element()
        } else if entries.is_empty() {
            let searching = !self.extensions.query.read(cx).value().trim().is_empty()
                || self.extensions.source_filter.is_some()
                || directory.category.is_some();
            let (title, description) = if searching {
                (
                    "No matching plugins",
                    "Try other words, or choose All sources and All categories.",
                )
            } else {
                (
                    "No plugins listed",
                    "Refresh to load the plugins your sources publish.",
                )
            };
            settings_ui::empty_state(AppIcon::Search, title, description, None).into_any_element()
        } else {
            let more = total > entries.len();
            let shown_count = entries.len();
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(self.extension_results(entries, cx))
                .when(more, |list| {
                    list.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .child(settings_ui::caption(format!(
                                "Showing {shown_count} of {total}"
                            )))
                            .child(
                                settings_ui::text_button("extensions-more", "Show more").on_click(
                                    cx.listener(|view, _, _, cx| {
                                        view.extensions.directory.limit += DIRECTORY_PAGE;
                                        view.extension_directory_search(cx);
                                    }),
                                ),
                            ),
                    )
                })
                .into_any_element()
        })
        .into_any_element()
    }
    fn extension_results(&self, rows: Vec<DirectoryEntry>, cx: &mut Context<Self>) -> AnyElement {
        let height = list_height(rows.len());
        let rows = Rc::new(rows);
        settings_ui::list_frame()
            .child(
                uniform_list(
                    "extension-results",
                    rows.len(),
                    cx.processor(move |view, range: std::ops::Range<usize>, _, cx| {
                        let busy = view.extensions.pending.is_some();
                        range
                            .filter_map(|index| {
                                let entry = rows.get(index)?.clone();
                                Some(view.extension_result_row(index, entry, busy, cx))
                            })
                            .collect()
                    }),
                )
                .h(height),
            )
            .into_any_element()
    }
    /// The trailing action of a directory entry: why it cannot be installed,
    /// Installed, Update or Install.
    fn extension_entry_action(
        &self,
        id: impl Into<ElementId>,
        entry: &DirectoryEntry,
        busy: bool,
        primary: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if entry.installed && !entry.update_available {
            return settings_ui::caption("Installed ✓").into_any_element();
        }
        let (Some(install), None) = (entry.install.clone(), &entry.unavailable) else {
            return settings_ui::caption(
                entry
                    .unavailable
                    .clone()
                    .unwrap_or_else(|| "Not installable".into()),
            )
            .max_w(px(200.))
            .single_line()
            .into_any_element();
        };
        let label = if entry.update_available {
            "Update"
        } else {
            "Install"
        };
        let button = if primary {
            settings_ui::primary_button(id, label)
        } else {
            settings_ui::secondary_button(id, label)
        };
        let target = entry.id.clone();
        settings_ui::busy(
            button.disabled(busy),
            self.extension_busy("extension_install_remote", Some(&entry.id)),
        )
        .on_click(cx.listener(move |view, _, _, cx| {
            view.extension_request_for(
                "extension_install_remote",
                target.clone(),
                json!({"item": install}),
                cx,
            )
        }))
        .into_any_element()
    }
    fn extension_result_row(
        &self,
        index: usize,
        entry: DirectoryEntry,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let action =
            self.extension_entry_action(("extension-install", index), &entry, busy, false, cx);
        let mut item = settings_ui::list_item(
            ("extension-result", index),
            Some(settings_ui::monogram(&entry.display_name, &entry.id).into_any_element()),
            entry.display_name.clone(),
            Some(
                if entry.description.is_empty() {
                    entry.source.clone()
                } else {
                    entry.description.clone()
                }
                .into(),
            ),
        )
        .hover_detail(entry.id.clone())
        .tags(
            entry
                .components
                .iter()
                .map(|kind| directory_tag(kind).into()),
        )
        .divider(index > 0);
        if let Some(version) = &entry.version {
            item = item.trailing(settings_ui::caption(version.clone()));
        }
        let open = entry.clone();
        item.trailing(action)
            .on_click(cx.listener(move |view, _, window, cx| {
                view.focus_plugins_page(window, cx);
                view.extensions.detail = Some(Detail::Available(Box::new(open.clone())));
                cx.notify();
            }))
            .into_any_element()
    }
    fn extension_available_page(
        &self,
        entry: &DirectoryEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.extensions.pending.is_some();
        let name = entry.display_name.clone();
        let mut page = settings_ui::settings_page("extension-detail", name.clone())
            .description(if entry.description.is_empty() {
                format!("From {}", entry.source)
            } else {
                entry.description.clone()
            })
            .breadcrumb(self.plugins_breadcrumb("extension-detail-back", name, cx))
            .action(self.extension_entry_action("extension-detail-install", entry, busy, true, cx));
        if let Some(status) = self.extension_status(cx) {
            page = page.child(status);
        }
        let mut about = vec![
            settings_ui::setting_row(
                "Source",
                None,
                Some(settings_ui::value_text(entry.source.clone()).into_any_element()),
            )
            .into_any_element(),
        ];
        if let Some(author) = &entry.author {
            about.push(
                settings_ui::setting_row(
                    "Publisher",
                    None,
                    Some(settings_ui::value_text(author.clone()).into_any_element()),
                )
                .into_any_element(),
            );
        }
        if let Some(version) = &entry.version {
            about.push(
                settings_ui::setting_row(
                    "Version",
                    None,
                    Some(settings_ui::value_text(version.clone()).into_any_element()),
                )
                .into_any_element(),
            );
        }
        if !entry.categories.is_empty() {
            about.push(
                settings_ui::setting_row(
                    "Categories",
                    None,
                    Some(settings_ui::value_text(entry.categories.join(", ")).into_any_element()),
                )
                .into_any_element(),
            );
        }
        if let Some(homepage) = entry.homepage.clone() {
            let open = homepage.clone();
            about.push(
                settings_ui::setting_row(
                    "Homepage",
                    Some(homepage.into()),
                    Some(
                        settings_ui::text_button("extension-detail-homepage", "Open")
                            .on_click(move |_, _, cx| cx.open_url(&open))
                            .into_any_element(),
                    ),
                )
                .into_any_element(),
            );
        }
        about.push(
            settings_ui::setting_row(
                "Identifier",
                None,
                Some(settings_ui::value_text(entry.id.clone()).into_any_element()),
            )
            .into_any_element(),
        );
        let components: Vec<AnyElement> = entry
            .components
            .iter()
            .map(|kind| {
                settings_ui::setting_row(
                    component_group(kind),
                    Some(component_risk(kind).into()),
                    None,
                )
                .into_any_element()
            })
            .collect();
        page = page.child(settings_ui::section(
            "About",
            None,
            settings_ui::rows(about),
        ));
        if let Some(reason) = &entry.unavailable {
            page = page.child(notice(BadgeTone::Warning, reason.clone()));
        }
        page.child(settings_ui::section(
            "Components and permissions",
            Some("You review every component before it is enabled. Servers and hooks get no network access until you allow exact hosts.".into()),
            if components.is_empty() {
                settings_ui::caption("Listed in the review after you install.").into_any_element()
            } else {
                settings_ui::rows(components).into_any_element()
            },
        ))
        .into_any_element()
    }

    // -----------------------------------------------------------------------
    // Installed
    // -----------------------------------------------------------------------

    /// Whether a source offers different content for an installed package.
    fn extension_update_available(&self, package: &ExtensionPackage) -> bool {
        self.extension_directory_entry(package)
            .is_some_and(|entry| entry.update_available)
    }
    fn extension_installed_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.extensions.pending.is_some();
        let packages: AnyElement = if self.extensions.packages.is_empty() {
            if self.extensions.loaded {
                settings_ui::empty_state(
                    AppIcon::Empty,
                    "No plugins installed",
                    "Browse Discover or install a plugin from a folder.",
                    Some(
                        settings_ui::primary_button("extensions-browse", "Browse plugins")
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.extension_leave_setup(Some(DISCOVER), window, cx)
                            }))
                            .into_any_element(),
                    ),
                )
                .into_any_element()
            } else {
                settings_ui::loading_list(3, self.extensions.pending_since).into_any_element()
            }
        } else {
            settings_ui::list_container(
                self.extensions
                    .packages
                    .iter()
                    .enumerate()
                    .map(|(index, package)| self.extension_package_row(index, package, busy, cx)),
            )
            .into_any_element()
        };
        let native = settings_ui::list_item(
            "extensions-native",
            Some(settings_ui::monogram("Native modules", "native-modules").into_any_element()),
            "Native modules",
            Some("Xylo's own sandboxed WebAssembly plugins".into()),
        )
        .on_click(cx.listener(|view, _, window, cx| {
            view.focus_plugins_page(window, cx);
            view.extensions.native = true;
            cx.notify();
        }));
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(settings_ui::SECTION_GAP))
            .child(packages)
            .child(settings_ui::section(
                "Native modules",
                None,
                settings_ui::list_container([native.into_any_element()]),
            ))
            .into_any_element()
    }
    fn extension_package_row(
        &self,
        index: usize,
        package: &ExtensionPackage,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = display_name(&package.name);
        let id = package.id.clone();
        let review = package.clone();
        let enabled = package.enabled;
        let open = package.id.clone();
        let components = match package.components.len() {
            1 => "1 component".to_owned(),
            count => format!("{count} components"),
        };
        settings_ui::list_item(
            ("extension-details", index),
            Some(settings_ui::monogram(&name, &package.id).into_any_element()),
            name.clone(),
            Some(format!("Version {} · {components}", package.version).into()),
        )
        .hover_detail(package.name.clone())
        .tags(
            package_kinds(package)
                .iter()
                .map(|kind| component_tag(kind).into()),
        )
        .trailing(package_pill(
            package,
            self.extension_update_available(package),
        ))
        .trailing(
            settings_ui::switch(
                ("extension-enable", index),
                format!("{name} enabled"),
                enabled,
            )
            .disabled(busy)
            .on_change(cx.listener(move |view, checked: &bool, window, cx| {
                if *checked {
                    // Enabling always goes through the review.
                    view.focus_plugins_page(window, cx);
                    view.extensions.review = Some(review.clone());
                    cx.notify();
                } else {
                    view.extension_request(
                        "extension_enable",
                        json!({"id": id, "enabled": false}),
                        cx,
                    )
                }
            })),
        )
        .on_click(cx.listener(move |view, _, window, cx| {
            view.focus_plugins_page(window, cx);
            view.extensions.detail = Some(Detail::Installed(open.clone()));
            cx.notify();
        }))
        .into_any_element()
    }
    fn extension_installed_page(
        &self,
        index: usize,
        package: &ExtensionPackage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.extensions.pending.is_some();
        let name = display_name(&package.name);
        let enabled = package.enabled;
        let review = package.clone();
        let id = package.id.clone();
        let toggling = self.extension_busy("extension_enable", Some(&package.id));
        let action = if enabled {
            settings_ui::busy(
                settings_ui::secondary_button("extension-detail-disable", "Disable").disabled(busy),
                toggling,
            )
            .on_click(cx.listener(move |view, _, _, cx| {
                view.extension_request_for(
                    "extension_enable",
                    id.clone(),
                    json!({"id": id, "enabled": false}),
                    cx,
                )
            }))
        } else {
            settings_ui::busy(
                settings_ui::primary_button("extension-detail-enable", "Enable").disabled(busy),
                toggling,
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                view.focus_plugins_page(window, cx);
                view.extensions.review = Some(review.clone());
                cx.notify();
            }))
        };
        let mut page = settings_ui::settings_page("extension-detail", name.clone())
            .description(format!("Version {} · {}", package.version, package.source))
            .breadcrumb(self.plugins_breadcrumb("extension-detail-back", name.clone(), cx))
            .action(package_pill(
                package,
                self.extension_update_available(package),
            ))
            .action(action);
        if let Some(status) = self.extension_status(cx) {
            page = page.child(status);
        }
        let components =
            package
                .components
                .iter()
                .enumerate()
                .map(|(component_index, component)| {
                    let mut detail = component.description.clone();
                    for requirement in &component.requirements {
                        if requirement == &format!("Configure and verify {}", component.name) {
                            continue;
                        }
                        detail.push_str(if detail.is_empty() { "" } else { " · " });
                        detail.push_str(requirement);
                    }
                    let label = match component.kind {
                        ComponentKind::Skill | ComponentKind::Command | ComponentKind::Agent => {
                            component.name.clone()
                        }
                        _ => component_group(&component.kind).to_owned(),
                    };
                    let mut control =
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(settings_ui::pill(
                                component.readiness.label(),
                                readiness_tone(component.readiness.label()),
                            ));
                    if configurable(&component.kind) {
                        let package = package.clone();
                        let selected = component.clone();
                        control = control.child(
                            settings_ui::secondary_button(
                                ("extension-configure", index * 1000 + component_index),
                                "Configure",
                            )
                            .disabled(busy || !enabled)
                            .on_click(cx.listener(
                                move |view, _, window, cx| {
                                    view.extension_open_setup(
                                        &package,
                                        &selected.name,
                                        selected.kind.clone(),
                                        window,
                                        cx,
                                    )
                                },
                            )),
                        );
                    }
                    settings_ui::setting_row(
                        label,
                        (!detail.is_empty()).then(|| detail.into()),
                        Some(control.into_any_element()),
                    )
                    .inset()
                    .into_any_element()
                });
        let permissions = settings_ui::rows(package_kinds(package).into_iter().map(|kind| {
            settings_ui::setting_row(
                component_group(&kind),
                Some(component_risk(&kind).into()),
                None,
            )
            .into_any_element()
        }));
        let remove = package.clone();
        page.child(settings_ui::section(
            "Components",
            (!enabled).then(|| "Enable the plugin to configure its servers and hooks.".into()),
            settings_ui::list_container(components),
        ))
        .child(settings_ui::section(
            "Permissions",
            Some("No network access until you allow exact hosts in a component's setup.".into()),
            permissions,
        ))
        .child(settings_ui::danger_zone([settings_ui::setting_row(
            "Remove plugin",
            Some("Its skills, commands, hooks and servers are removed with their setup.".into()),
            Some(
                settings_ui::busy(
                    settings_ui::destructive_button(("extension-remove", index), "Remove")
                        .disabled(busy),
                    self.extension_busy("extension_remove", Some(&package.id)),
                )
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.confirm_remove_extension(&remove, window, cx)
                }))
                .into_any_element(),
            ),
        )
        .inset()
        .into_any_element()]))
        .into_any_element()
    }

    /// What a package contains and may do, confirmed before it is enabled.
    fn extension_review_page(
        &self,
        package: &ExtensionPackage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.extensions.pending.is_some();
        let name = display_name(&package.name);
        let mut page = settings_ui::settings_page("extension-review", format!("Review {name}"))
            .description(format!(
                "Version {} · {}{}",
                package.version,
                package.source,
                package
                    .revision
                    .as_ref()
                    .map(|revision| format!(" @ {}", short_revision(revision)))
                    .unwrap_or_default()
            ))
            .breadcrumb(self.plugins_breadcrumb("extension-review-back", name, cx));
        if let Some(status) = self.extension_status(cx) {
            page = page.child(status);
        }
        for (kind, rows) in review_groups(package) {
            page = page.child(settings_ui::section(
                component_group(&kind),
                Some(component_risk(&kind).into()),
                settings_ui::list_container(rows.into_iter().map(|row| {
                    let component = &package.components[row];
                    let mut detail = component.description.clone();
                    for requirement in &component.requirements {
                        detail.push_str(if detail.is_empty() { "" } else { " · " });
                        detail.push_str(requirement);
                    }
                    settings_ui::setting_row(
                        component.name.clone(),
                        (!detail.is_empty()).then(|| detail.into()),
                        None,
                    )
                    .inset()
                    .into_any_element()
                })),
            ));
        }
        page = page.child(settings_ui::section(
            "Network access",
            Some("None until you allow exact hosts in a component's setup. Servers and hooks get no network access by default.".into()),
            div(),
        ));
        let actions = if package.enabled {
            page = page.child(notice(
                BadgeTone::Warning,
                "This plugin is enabled. Disable it if anything above is unexpected.",
            ));
            settings_ui::form_actions([
                settings_ui::secondary_button("extension-review-disable", "Disable plugin")
                    .disabled(busy)
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.focus_plugins_page(window, cx);
                        if let Some(package) = view.extensions.review.take() {
                            view.extension_request_for(
                                "extension_enable",
                                package.id.clone(),
                                json!({"id": package.id, "enabled": false}),
                                cx,
                            );
                        }
                    }))
                    .into_any_element(),
                settings_ui::primary_button("extension-review-close", "Done")
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.focus_plugins_page(window, cx);
                        view.extensions.review = None;
                        cx.notify();
                    }))
                    .into_any_element(),
            ])
        } else {
            settings_ui::form_actions([
                settings_ui::text_button("extension-review-cancel", "Cancel")
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.focus_plugins_page(window, cx);
                        view.extensions.review = None;
                        cx.notify();
                    }))
                    .into_any_element(),
                settings_ui::primary_button("extension-review-enable", "Enable plugin")
                    .disabled(busy)
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.focus_plugins_page(window, cx);
                        if let Some(package) = view.extensions.review.take() {
                            // Land on the plugin so its setup is one step away.
                            view.extensions.tab = INSTALLED;
                            view.extensions.detail = Some(Detail::Installed(package.id.clone()));
                            view.extension_request_for(
                                "extension_enable",
                                package.id.clone(),
                                json!({"id": package.id, "enabled": true}),
                                cx,
                            );
                        }
                    }))
                    .into_any_element(),
            ])
        };
        page.child(actions).into_any_element()
    }

    // -----------------------------------------------------------------------
    // Sources
    // -----------------------------------------------------------------------

    fn extension_sources_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.extensions.pending.is_some();
        let rows = self
            .extensions
            .sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                let status = self
                    .extensions
                    .directory
                    .sources
                    .iter()
                    .find(|status| status.source.repository == source.repository);
                let detail = match status {
                    Some(status) if status.message.is_some() => {
                        status.message.clone().unwrap_or_default()
                    }
                    Some(status) => match status.fetched_at {
                        Some(at) => {
                            format!("Updated {} · {} plugins", time_ago(at), status.entries)
                        }
                        None => "Not loaded yet".into(),
                    },
                    None => "Not loaded yet".into(),
                };
                let mut item = settings_ui::list_item(
                    ("extension-source", index),
                    Some(
                        settings_ui::monogram(&source.name, &source.repository).into_any_element(),
                    ),
                    source.name.clone(),
                    Some(detail.into()),
                )
                .hover_detail(source.repository.clone());
                if !built_in_source(source) {
                    let target = source.repository.clone();
                    item = item.trailing(
                        settings_ui::busy(
                            settings_ui::destructive_button(
                                ("extension-source-remove", index),
                                "Remove",
                            )
                            .disabled(busy),
                            self.extension_busy("extension_sources", Some(&source.repository)),
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            let mut sources = view.extensions.sources.clone();
                            sources.remove(index);
                            view.extension_request_for(
                                "extension_sources",
                                target.clone(),
                                json!({"sources": sources}),
                                cx,
                            );
                        })),
                    );
                }
                item.into_any_element()
            });
        let add: AnyElement = if self.extensions.adding_source {
            settings_ui::form_column([
                settings_ui::field(
                    "Repository",
                    Input::new(&self.extensions.source)
                        .aria_label("GitHub repository or directory URL"),
                    Some("A public GitHub repository or folder, for example owner/plugins.".into()),
                )
                .into_any_element(),
                settings_ui::form_actions([
                    settings_ui::text_button("extension-source-cancel", "Cancel")
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.extensions.adding_source = false;
                            cx.notify();
                        }))
                        .into_any_element(),
                    settings_ui::busy(
                        settings_ui::primary_button("extension-source-add", "Add source")
                            .disabled(busy),
                        self.extension_busy("extension_sources", Some("")),
                    )
                    .on_click(cx.listener(|view, _, _, cx| view.extension_add_source(cx)))
                    .into_any_element(),
                ])
                .into_any_element(),
            ])
            .into_any_element()
        } else {
            div()
                .child(
                    settings_ui::secondary_button("extension-source-new", "Add source")
                        .disabled(busy)
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.extensions.adding_source = true;
                            cx.notify();
                        })),
                )
                .into_any_element()
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(settings_ui::list_container(rows))
            .child(add)
            .into_any_element()
    }
    fn extension_add_source(&mut self, cx: &mut Context<Self>) {
        match ExtensionSource::from_input(&self.extensions.source.read(cx).value()) {
            Ok(source) => {
                if self
                    .extensions
                    .sources
                    .iter()
                    .any(|existing| source_key(existing) == source_key(&source))
                {
                    self.extensions.notice = "This source is already added.".into();
                    cx.notify();
                    return;
                }
                let mut sources = self.extensions.sources.clone();
                sources.push(source);
                // The empty target marks the add form's button as the busy one.
                self.extension_request_for(
                    "extension_sources",
                    "",
                    json!({"sources": sources}),
                    cx,
                );
            }
            Err(error) => {
                self.extensions.notice = error.to_string();
                cx.notify();
            }
        }
    }
}
