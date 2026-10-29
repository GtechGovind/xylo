use super::*;

impl Xylo {
    pub(super) fn files_page(&self, cx: &mut Context<Self>) -> AnyElement {
        use super::file_browser::{RowContent, TreeRow};
        use gpui_kit::component::{Icon, IconName};
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
                                            IconName::Folder
                                        } else {
                                            IconName::FileText
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
                                        .h(px(30.))
                                        .pl(px(padding + 16.))
                                        .pr_2()
                                        .flex()
                                        .items_center()
                                        .min_w_0()
                                        .text_xs()
                                        .text_color(rgb(muted()))
                                        .child(div().truncate().child(detail))
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
                                .rounded_md()
                                .min_w_0()
                                .text_xs()
                                .text_color(rgb(text()))
                                .bg(rgb(if is_selected { selected() } else { sidebar() }))
                                .hover(|style| style.bg(rgb(hover())))
                                .child(
                                    div()
                                        .w(px(12.))
                                        .flex_shrink_0()
                                        .text_color(rgb(muted()))
                                        .child(if directory {
                                            if row.expanded { "⌄" } else { "›" }
                                        } else {
                                            ""
                                        }),
                                );
                            if let Some(glyph) = glyph {
                                button = button
                                    .child(Icon::new(glyph).size(px(14.)).text_color(rgb(muted())));
                            }
                            Some(
                                button
                                    .child(div().flex_1().min_w_0().truncate().child(detail))
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
            .w(px(self.settings.ui.files_width))
            .max_w(relative(0.6))
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
                    .child(subtitle("Files").flex_1().min_w_0())
                    .child(
                        button("refresh-files", "Refresh")
                            .text_xs()
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
            .when(self.workspace.is_none(), |panel| {
                panel.child(subtitle("Open a project to browse its files."))
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
                        .h(px(42.))
                        .px_3()
                        .flex()
                        .items_center()
                        .justify_between()
                        .border_b_1()
                        .border_color(rgb(border()))
                        .child(
                            subtitle(format!(
                                "{}{}",
                                path.display(),
                                if self.editor_dirty { "  ●" } else { "" }
                            ))
                            .flex_1()
                            .min_w_0()
                            .truncate(),
                        )
                        .child(
                            button("editor-suggest", "Suggest")
                                .flex_shrink_0()
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.editor.update(cx, |editor, cx| {
                                        editor.show_completions(window, cx)
                                    })
                                })),
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
        } else {
            div()
                .flex_1()
                .p_8()
                .child(heading(
                    "Your project, in reach",
                    "Choose a file to edit. Changes save only when you click Save.",
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
    pub(super) fn changes(&self, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.entity().downgrade();
        let lines = uniform_list("diff-lines", self.diff_lines.len(), move |range, _, cx| {
            weak.update(cx, |view, _| {
                range
                    .filter_map(|i| {
                        let line = view.diff_lines.get(i)?;
                        Some(
                            div()
                                .id(("diff-line", i))
                                .h(px(22.))
                                .px_2()
                                .font_family("monospace")
                                .text_sm()
                                .whitespace_nowrap()
                                .bg(rgb(if line.starts_with('+') && !line.starts_with("+++") {
                                    0xecf9ee
                                } else if line.starts_with('-') && !line.starts_with("---") {
                                    0xffecec
                                } else {
                                    bg()
                                }))
                                .text_color(rgb(if line.starts_with('+') {
                                    0x25713a
                                } else if line.starts_with('-') {
                                    0xb93636
                                } else {
                                    text()
                                }))
                                .child(line.clone())
                                .into_any_element(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
        })
        .track_scroll(&self.diff_scroll)
        .flex_1()
        .min_h_0();
        div()
            .size_full()
            .p_6()
            .flex()
            .flex_col()
            .min_h_0()
            .child(heading("Changes", "Review changes in your project."))
            .child(button("refresh-diff", "Refresh diff").on_click(cx.listener(
                |this, _, _, cx| {
                    if let Some(path) = this.workspace.clone() {
                        this.host_call(cx, |host| host.git_diff(path));
                    }
                },
            )))
            .child(if self.diff.is_empty() {
                div()
                    .mt_5()
                    .child(subtitle("No diff loaded. Open a Git project and refresh."))
                    .into_any_element()
            } else {
                lines.into_any_element()
            })
            .into_any_element()
    }
    pub(super) fn artifacts_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut view = div()
            .id("artifact-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .child(heading(
                "Artifacts",
                "Recent files created for you. Open a file to inspect it.",
            ));
        view = view
            .child(self.documents_panel(cx))
            .child(self.native_result_card(cx));
        if self.artifacts.is_empty() {
            view = view.child(card().child(subtitle("No artifacts have been returned yet.")));
        }
        for (i, path) in self.artifacts.iter().enumerate() {
            let path = path.clone();
            let label = path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned();
            view = view.child(button(("artifact", i), label).on_click(cx.listener(
                move |view, _, window, cx| {
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
                },
            )));
        }
        view.into_any_element()
    }
}
