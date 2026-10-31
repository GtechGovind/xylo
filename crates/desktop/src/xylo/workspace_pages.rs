//! The Files pane of the work panel (project tree and editor) and the
//! Artifacts page.
use super::*;
use crate::icons::{self, AppIcon};

impl Xylo {
    pub(super) fn files_page(&self, cx: &mut Context<Self>) -> AnyElement {
        use super::file_browser::{RowContent, TreeRow};
        let weak = cx.entity().downgrade();
        let rows = uniform_list(
            "file-list-rows",
            self.file_browser.tree.rows.len(),
            move |range, _, cx| {
                weak.update(cx, |view, cx| {
                    range
                        .filter_map(|i| {
                            let row: TreeRow = view.file_browser.tree.rows.get(i)?.clone();
                            let name = row
                                .path
                                .file_name()
                                .unwrap_or(row.path.as_os_str())
                                .to_string_lossy()
                                .into_owned();
                            let is_selected =
                                row.key.is_some() && row.key == view.file_browser.tree.selected;
                            let padding = 6. + row.depth as f32 * 14.;
                            let loading = matches!(row.content, RowContent::Loading);
                            let (label, detail, glyph, directory) = match &row.content {
                                RowContent::Entry(entry) => {
                                    let label = if entry.is_dir {
                                        format!(
                                            "{} folder {name}",
                                            if row.expanded { "Collapse" } else { "Expand" }
                                        )
                                    } else {
                                        format!("Open file {name}")
                                    };
                                    (
                                        label,
                                        name.clone(),
                                        Some(if entry.is_dir {
                                            if row.expanded {
                                                AppIcon::FolderOpen
                                            } else {
                                                AppIcon::Folder
                                            }
                                        } else {
                                            AppIcon::File
                                        }),
                                        entry.is_dir,
                                    )
                                }
                                RowContent::More => (
                                    format!("Load more in {name}"),
                                    "Load more…".into(),
                                    None,
                                    false,
                                ),
                                RowContent::Earlier => (
                                    format!("Show earlier items in {name}"),
                                    "Show earlier items…".into(),
                                    None,
                                    false,
                                ),
                                RowContent::Load => (
                                    format!("Load folder {name}"),
                                    "Load folder…".into(),
                                    None,
                                    false,
                                ),
                                RowContent::Loading => {
                                    (String::new(), "Loading…".into(), None, false)
                                }
                                RowContent::Notice(message) => {
                                    (String::new(), message.clone(), None, false)
                                }
                            };
                            let Some(key) = row.key else {
                                return Some(
                                    div()
                                        .id(("file-notice", i))
                                        .w_full()
                                        .h(px(30.))
                                        .pl(px(padding + 16.))
                                        .pr_2()
                                        .flex()
                                        .items_center()
                                        .gap_1p5()
                                        .min_w_0()
                                        .text_xs()
                                        .text_color(rgb(muted()))
                                        .when(loading, |row| {
                                            row.child(crate::motion::spinner_inline())
                                        })
                                        .tooltip({
                                            let detail = SharedString::from(detail.clone());
                                            move |_, cx| {
                                                cx.new(|_| crate::ui::XyloTooltip(detail.clone()))
                                                    .into()
                                            }
                                        })
                                        .child(div().flex_1().min_w_0().single_line().child(detail))
                                        .into_any_element(),
                                );
                            };
                            let mut button = gpui_kit::base::Button::new(("file", i))
                                .accessibility_label(label)
                                .h(px(30.))
                                .w_full()
                                .pl(px(padding))
                                .pr_2()
                                .flex()
                                .items_center()
                                .justify_start()
                                .gap_1()
                                .rounded(px(7.))
                                .min_w_0()
                                .text_size(px(12.5))
                                .text_color(rgb(if is_selected {
                                    text()
                                } else {
                                    text_secondary()
                                }))
                                .bg(rgb(if is_selected { selected() } else { sidebar() }))
                                .hover(|style| style.bg(rgb(hover())).text_color(rgb(text())))
                                .child(
                                    div()
                                        .w(px(14.))
                                        .flex_shrink_0()
                                        .flex()
                                        .items_center()
                                        .text_color(rgb(faint()))
                                        .when(directory, |caret| {
                                            caret.child(icons::small(if row.expanded {
                                                AppIcon::ChevronDown
                                            } else {
                                                AppIcon::ChevronRight
                                            }))
                                        }),
                                );
                            if let Some(glyph) = glyph {
                                button = button.child(icons::icon(glyph).text_color(rgb(
                                    if directory { accent_text() } else { muted() },
                                )));
                            }
                            Some(
                                button
                                    .child(div().flex_1().min_w_0().single_line().child(detail))
                                    .when_some(
                                        view.git_panel.statuses.get(&row.path).copied(),
                                        |button, status| {
                                            button.child(
                                                div()
                                                    .text_color(rgb(if status == 'D' {
                                                        danger()
                                                    } else {
                                                        muted()
                                                    }))
                                                    .child(status.to_string()),
                                            )
                                        },
                                    )
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        view.activate_tree_row(key.clone(), window, cx)
                                    }))
                                    .into_any_element(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
            },
        )
        .track_scroll(&self.file_scroll)
        .flex_1()
        .min_h_0();
        let list = div()
            .id("file-tree")
            .flex_shrink_0()
            .w(px(self.settings.ui.files_width))
            .max_w(relative(0.45))
            .min_w(px(130.))
            .h_full()
            .bg(rgb(sidebar()))
            .border_r_1()
            .border_color(rgb(border()))
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .when_some(self.file_browser.focus.as_ref(), |panel, focus| {
                panel.track_focus(focus)
            })
            .on_key_down(
                cx.listener(|view, event, window, cx| view.tree_key_down(event, window, cx)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_1()
                    .child(sidebar_label("Files").flex_1().min_w_0())
                    .child(
                        icons::icon_button("refresh-files", AppIcon::Refresh, "Refresh files")
                            .on_click(cx.listener(|view, _, _, cx| view.refresh_files(cx))),
                    ),
            )
            .when(self.file_browser.indexing, |panel| {
                panel.child(subtitle("Discovering project files…"))
            })
            .when(self.file_browser.watch_limited, |panel| {
                panel.child(subtitle("Refresh files to check recent changes."))
            })
            .when(!self.file_browser.notice.is_empty(), |panel| {
                panel.child(subtitle(self.file_browser.notice.clone()))
            })
            .child(rows)
            .relative()
            .child(self.resize_edge(Panel::Files));
        let content = if let Some(path) = &self.editor_path {
            div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .min_h(px(42.))
                        .flex_shrink_0()
                        .py_1()
                        .px_3()
                        .flex()
                        .flex_wrap()
                        .gap_1()
                        .items_center()
                        .justify_between()
                        .border_b_1()
                        .border_color(rgb(border()))
                        .child(
                            div()
                                .id("editor-file-identity")
                                .tooltip({
                                    let full_path: SharedString = path.display().to_string().into();
                                    move |_, cx| {
                                        cx.new(|_| crate::ui::XyloTooltip(full_path.clone())).into()
                                    }
                                })
                                .child(
                                    subtitle(format!(
                                        "{}{}",
                                        self.workspace
                                            .as_deref()
                                            .and_then(|root| path.strip_prefix(root).ok())
                                            .unwrap_or(path.as_path())
                                            .to_string_lossy(),
                                        if self.editor_dirty { "  ●" } else { "" }
                                    ))
                                    .flex_1()
                                    .min_w_0()
                                    .single_line(),
                                )
                                .flex_1()
                                .min_w(px(80.))
                                .overflow_hidden(),
                        )
                        .child(
                            primary(
                                "save-file",
                                if self.pending_saves.contains_key(path) {
                                    "Saving…"
                                } else {
                                    "Save"
                                },
                            )
                            .flex_shrink_0()
                            .disabled(self.pending_saves.contains_key(path))
                            .on_click(cx.listener(
                                |this, _, window, cx| this.request_editor_save(false, window, cx),
                            )),
                        ),
                )
                .when(self.project_search.external_conflict, |panel| {
                    panel.child(self.external_file_notice(cx))
                })
                .child(
                    Editor::new(&self.editor)
                        .aria_label("File editor")
                        .h(relative(1.))
                        .appearance(false)
                        .bordered(false),
                )
                .into_any_element()
        } else if self.workspace.is_none() {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .items_start()
                .gap(px(8.))
                .child(super::work_panel::panel_hint(
                    "Open a project to browse its files.",
                ))
                .child(
                    secondary("editor-open-project", "Open project…")
                        .ml(px(14.))
                        .on_click(
                            cx.listener(|view, _, window, cx| view.pick_workspace(window, cx)),
                        ),
                )
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .child(super::work_panel::panel_hint(
                    "Select a file to view or edit it.",
                ))
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .min_h_0()
            .child(list)
            .child(content)
            .into_any_element()
    }
    pub(super) fn artifacts_page(&self, cx: &mut Context<Self>) -> AnyElement {
        // The toolbar names the page; the body starts with its content.
        let mut view = div()
            .id("artifact-scroll")
            .size_full()
            .overflow_y_scroll()
            .px(px(24.))
            .pt(px(8.))
            .pb(px(24.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(self.documents_panel(cx))
            .child(self.native_result_card(cx))
            .child(sidebar_label("Recent files"));
        if self.artifacts.is_empty() {
            view = view.child(
                subtitle("Files Xylo creates for you appear here.")
                    .px(px(10.))
                    .single_line(),
            );
        }
        for (i, path) in self.artifacts.iter().enumerate() {
            let path = path.clone();
            let label = path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned();
            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("")
                .to_ascii_uppercase();
            let in_xylo = matches!(extension.as_str(), "DOCX" | "XLSX");
            let location = path.parent().unwrap_or(Path::new(""));
            let location = self
                .workspace
                .as_deref()
                .and_then(|root| location.strip_prefix(root).ok())
                .unwrap_or(location);
            let location = if location.as_os_str().is_empty() {
                "Project root".to_string()
            } else {
                location.display().to_string()
            };
            let detail = format!(
                "{} · {}",
                if extension.is_empty() {
                    "File"
                } else {
                    &extension
                },
                location
            );
            view = view.child(
                button(("artifact", i), label.clone())
                    .w_full()
                    .min_w_0()
                    .h(px(52.))
                    .flex()
                    .flex_col()
                    .items_start()
                    .justify_center()
                    .gap(px(2.))
                    .accessibility_label(format!(
                        "Open {label}, {}",
                        if in_xylo {
                            "in Xylo"
                        } else {
                            "with default app"
                        }
                    ))
                    .tooltip({
                        let full_path: SharedString = path.display().to_string().into();
                        move |_, cx| cx.new(|_| crate::ui::XyloTooltip(full_path.clone())).into()
                    })
                    .child(subtitle(detail).w_full().min_w_0().single_line())
                    .on_click(cx.listener(move |view, _, window, cx| {
                        if path
                            .extension()
                            .and_then(|extension| extension.to_str())
                            .is_some_and(|extension| {
                                matches!(extension.to_ascii_lowercase().as_str(), "docx" | "xlsx")
                            })
                        {
                            view.document_open(path.clone(), window, cx);
                        } else {
                            cx.open_with_system(&path);
                        }
                    })),
            );
        }
        view.into_any_element()
    }
}
