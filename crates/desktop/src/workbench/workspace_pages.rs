use super::*;

impl Workbench {
    pub(super) fn files_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.entity().downgrade();
        let rows = uniform_list("file-list-rows", self.file_browser.entries.len(), move |range, _, cx| {
            weak.update(cx, |view, cx| range.filter_map(|i| {
                let entry = view.file_browser.entries.get(i)?.clone();
                let path = entry.path;
                let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy();
                let label = if entry.is_dir { format!("▸ {name}") } else { name.into_owned() };
                Some(button(("file", i), label).h(px(32.)).w_full().justify_start().text_xs()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if entry.is_dir {
                            this.browse_directory(path.clone(), cx);
                            return;
                        }
                        if path.extension().and_then(|extension|extension.to_str()).is_some_and(|extension|matches!(extension.to_ascii_lowercase().as_str(),"docx"|"xlsx")) {
                            this.document_open(path.clone(),window,cx);
                            return;
                        }
                        if this.editor_dirty {
                            this.error = Some("Save the current editor changes before opening another file.".into());
                            cx.notify();
                            return;
                        }
                        this.request_editor_file(path.clone(), None, false, cx);
                    })).into_any_element())
            }).collect()).unwrap_or_default()
        }).track_scroll(&self.file_scroll).flex_1().min_h_0();
        let directory = self
            .file_browser
            .directory
            .as_ref()
            .or(self.workspace.as_ref());
        let location = directory
            .and_then(|path| {
                self.workspace
                    .as_ref()
                    .and_then(|root| path.strip_prefix(root).ok())
            })
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Project folder".into());
        let parent = directory
            .and_then(|path| path.parent())
            .filter(|path| {
                self.workspace
                    .as_ref()
                    .is_some_and(|root| path.starts_with(root))
            })
            .map(Path::to_path_buf);
        let next = self.file_browser.next.clone();
        let loading = self.file_browser.loading();
        let list = div()
            .id("file-tree")
            .w(px(self.settings.ui.files_width))
            .h_full()
            .bg(rgb(sidebar()))
            .border_r_1()
            .border_color(rgb(border()))
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .child(subtitle(format!(
                "PROJECT FILES · {}",
                self.file_browser.file_count
            )))
            .child(
                div()
                    .flex()
                    .gap_1()
                    .child(
                        button("files-up", "Up")
                            .text_xs()
                            .disabled(parent.is_none())
                            .on_click(cx.listener(move |view, _, _, cx| {
                                if let Some(parent) = &parent {
                                    view.browse_directory(parent.clone(), cx);
                                }
                            })),
                    )
                    .child(
                        button("refresh-files", "Refresh files")
                            .text_xs()
                            .on_click(cx.listener(|view, _, _, cx| view.refresh_files(cx))),
                    ),
            )
            .child(subtitle(location).min_w_0().truncate())
            .when(self.file_browser.indexing, |panel| {
                panel.child(subtitle("Discovering project files…"))
            })
            .when(self.file_browser.watch_limited, |panel| {
                panel.child(subtitle(
                    "Automatic file updates are limited. Refresh files to check recent changes.",
                ))
            })
            .when(loading, |panel| panel.child(subtitle("Loading folder…")))
            .when(!self.file_browser.notice.is_empty(), |panel| {
                panel.child(subtitle(self.file_browser.notice.clone()))
            })
            .when(!loading && self.file_browser.entries.is_empty(), |panel| {
                panel.child(subtitle("No files in this folder."))
            })
            .child(rows)
            .child(
                div()
                    .flex()
                    .gap_1()
                    .child(
                        button("files-first", "First page")
                            .text_xs()
                            .disabled(loading || self.file_browser.after.is_none())
                            .on_click(cx.listener(|view, _, _, cx| view.load_file_page(None, cx))),
                    )
                    .child(
                        button("files-next", "Next page")
                            .text_xs()
                            .disabled(loading || next.is_none())
                            .on_click(cx.listener(move |view, _, _, cx| {
                                if let Some(next) = &next {
                                    view.load_file_page(Some(next.clone()), cx);
                                }
                            })),
                    ),
            )
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
