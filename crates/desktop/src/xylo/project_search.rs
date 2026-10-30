//! Native project search and watched-file presentation. Each response is tied to
//! a request and project; late reads cannot replace a newer editor selection.
use super::*;
use std::sync::Arc;
use xylo_runtime::workspace::{FileIndex, SearchCursor, SearchHit, SearchOptions, SearchPage};

pub(super) struct ProjectSearchUi {
    query: Entity<InputState>,
    case_sensitive: bool,
    regular_expression: bool,
    hits: Vec<SearchHit>,
    shown_before: usize,
    submitted: Option<(String, bool, bool)>,
    next: Option<SearchCursor>,
    request: Option<(Uuid, bool)>,
    notice: String,
    scroll: UniformListScrollHandle,
    revision: u64,
    opening: Option<FileRequest>,
    pub external_conflict: bool,
    _subscription: Subscription,
}
struct FileRequest {
    id: Uuid,
    path: PathBuf,
    jump: Option<(u64, usize)>,
    refresh: bool,
}
impl ProjectSearchUi {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Xylo>) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search project contents"));
        let subscription = cx.subscribe(&query, |view, _, event, cx| match event {
            InputEvent::PressEnter { .. } => view.run_project_search(false, cx),
            InputEvent::Change => view.invalidate_project_search(cx),
            _ => {}
        });
        Self {
            query,
            _subscription: subscription,
            case_sensitive: false,
            regular_expression: false,
            hits: Vec::new(),
            shown_before: 0,
            submitted: None,
            next: None,
            request: None,
            notice: String::new(),
            scroll: UniformListScrollHandle::new(),
            revision: 0,
            opening: None,
            external_conflict: false,
        }
    }
    pub(super) fn reset(&mut self) {
        self.hits.clear();
        self.shown_before = 0;
        self.submitted = None;
        self.next = None;
        self.request = None;
        self.notice.clear();
        self.revision = 0;
        self.opening = None;
        self.external_conflict = false;
    }
}
impl Xylo {
    pub(super) fn request_editor_save(
        &mut self,
        quit_after: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.project_search.external_conflict {
            self.save_editor(quit_after, cx);
            return;
        }
        let path = self.editor_path.clone();
        let answer=window.prompt(PromptLevel::Warning,"Overwrite changes on disk?",
            Some("This file changed outside Xylo. Saving will replace those changes with your editor contents."),
            &[PromptButton::ok("Overwrite"),PromptButton::cancel("Cancel")],cx);
        cx.spawn_in(window, async move |view, cx| {
            let answer = answer.await;
            let _ = view.update_in(cx, |view, _, cx| {
                if matches!(answer, Ok(0)) && view.editor_path == path {
                    view.save_editor(quit_after, cx);
                } else {
                    view.updates.after_save = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }
    pub(super) fn open_project_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = false;
        self.navigate(Page::Search, cx);
        self.project_search
            .query
            .update(cx, |input, cx| input.focus(window, cx));
    }
    pub(super) fn cancel_project_search(&mut self, cx: &mut Context<Self>) {
        if let Some((id, _)) = self.project_search.request.take() {
            if let Some(host) = &self.host {
                let _ = host.cancel_project_search(id);
            }
            self.project_search.notice = "Search stopped.".into();
            cx.notify();
        }
    }
    fn invalidate_project_search(&mut self, cx: &mut Context<Self>) {
        self.cancel_project_search(cx);
        self.project_search.hits.clear();
        self.project_search.shown_before = 0;
        self.project_search.next = None;
        self.project_search.submitted = None;
        self.project_search.notice = "Search again for the updated query and options.".into();
        cx.notify();
    }
    fn run_project_search(&mut self, more: bool, cx: &mut Context<Self>) {
        self.cancel_project_search(cx);
        let Some(root) = self.workspace.clone() else {
            self.project_search.notice = "Open a project folder to search its files.".into();
            cx.notify();
            return;
        };
        let query = self
            .project_search
            .query
            .read(cx)
            .value()
            .trim()
            .to_string();
        if query.is_empty() {
            self.project_search.reset();
            cx.notify();
            return;
        }
        let identity = (
            query.clone(),
            self.project_search.case_sensitive,
            self.project_search.regular_expression,
        );
        let more = more && self.project_search.submitted.as_ref() == Some(&identity);
        if more && self.project_search.next.is_none() {
            return;
        }
        let id = Uuid::new_v4();
        let options = SearchOptions {
            query,
            case_sensitive: self.project_search.case_sensitive,
            regular_expression: self.project_search.regular_expression,
            after: if more {
                self.project_search.next.clone()
            } else {
                None
            },
            limit: 100,
        };
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.search_project(id, root, options));
        if result.is_ok() {
            self.project_search.submitted = Some(identity);
            if !more {
                self.project_search.hits.clear();
                self.project_search.next = None;
            }
            self.project_search.request = Some((id, more));
            self.project_search.notice = "Searching…".into();
        }
        self.result(result, cx);
    }
    pub(super) fn project_search_result(
        &mut self,
        id: Uuid,
        result: Result<SearchPage, String>,
        cx: &mut Context<Self>,
    ) {
        let Some((expected, more)) = self.project_search.request else {
            return;
        };
        if expected != id {
            return;
        }
        self.project_search.request = None;
        match result {
            Ok(page) if self.workspace.as_ref() == Some(&page.root) => {
                self.project_search.shown_before = if more {
                    self.project_search
                        .shown_before
                        .saturating_add(self.project_search.hits.len())
                } else {
                    0
                };
                self.project_search.hits = page.hits;
                self.project_search.scroll = UniformListScrollHandle::new();
                self.project_search.next = page.next;
                self.project_search.notice = format!(
                    "{} matching lines on this page{}{}",
                    self.project_search.hits.len(),
                    if page.timed_out {
                        " · Search paused; continue for more results"
                    } else {
                        ""
                    },
                    if page.skipped_files > 0 {
                        format!(
                            " · {} unreadable or oversized files skipped",
                            page.skipped_files
                        )
                    } else {
                        String::new()
                    }
                );
                if self.project_search.shown_before > 0 {
                    self.project_search.notice.push_str(&format!(
                        " · {} shown earlier",
                        self.project_search.shown_before
                    ));
                }
                if page.indexing {
                    self.project_search
                        .notice
                        .push_str(" · Discovering project files; search again when ready");
                }
            }
            Ok(_) => {}
            Err(message) => {
                self.project_search.notice = message;
                self.project_search.next = None;
            }
        }
        cx.notify();
    }
    pub(super) fn workspace_indexed(&mut self, index: Arc<FileIndex>, cx: &mut Context<Self>) {
        if self.workspace.as_ref() != Some(&index.root) {
            return;
        }
        if self.project_search.revision != 0
            && self.project_search.revision != index.revision
            && !self.project_search.hits.is_empty()
        {
            self.project_search.next = None;
            self.project_search.notice =
                "Project changed. Search again for current results.".into();
        }
        self.project_search.revision = index.revision;
        self.update_file_browser(&index, cx);
        if let Some(path) = self.editor_path.clone() {
            if index.removed.contains(&path) {
                self.project_search.external_conflict = true;
            } else if (index.changed.contains(&path) || !index.changes_complete)
                && !self.pending_saves.contains_key(&path)
            {
                if self.editor_dirty {
                    self.project_search.external_conflict = true;
                } else {
                    self.request_editor_file(path, None, true, cx);
                }
            }
        }
        cx.notify();
    }
    pub(super) fn request_editor_file(
        &mut self,
        path: PathBuf,
        jump: Option<(u64, usize)>,
        refresh: bool,
        cx: &mut Context<Self>,
    ) {
        if self.editor_dirty {
            self.error = Some("Save your changes before opening another file.".into());
            cx.notify();
            return;
        }
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.open_file(id, path.clone()));
        if result.is_ok() {
            self.project_search.opening = Some(FileRequest {
                id,
                path,
                jump,
                refresh,
            });
        }
        self.result(result, cx);
    }
    pub(super) fn opened_file(
        &mut self,
        id: Uuid,
        path: PathBuf,
        result: Result<String, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .project_search
            .opening
            .as_ref()
            .is_none_or(|request| request.id != id || request.path != path)
        {
            return;
        }
        let Some(request) = self.project_search.opening.take() else {
            return;
        };
        if self.editor_dirty {
            if request.refresh {
                self.project_search.external_conflict = true;
            } else {
                self.error =
                    Some("Your file has unsaved changes. Save before opening another file.".into());
            }
            cx.notify();
            return;
        }
        let content = match result {
            Ok(content) => content,
            Err(error) => {
                if request.refresh {
                    self.project_search.external_conflict = true;
                }
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        // Metadata notifications (including the first project snapshot) can
        // describe the same bytes we just opened. Keep the existing entity and
        // its language-server menu, diagnostics, focus and selection intact.
        if request.refresh
            && self.editor_path.as_ref() == Some(&path)
            && self.editor.read(cx).value().as_ref() == content
        {
            self.project_search.external_conflict = false;
            cx.notify();
            return;
        }
        let cursor = if let Some((line, byte_column)) = request.jump {
            let row = line.saturating_sub(1).min(u32::MAX as u64) as u32;
            let column = content
                .lines()
                .nth(row as usize)
                .map(|text| {
                    text.char_indices()
                        .take_while(|(offset, _)| *offset < byte_column)
                        .count()
                })
                .unwrap_or(0)
                .min(u32::MAX as usize) as u32;
            Some(gpui_kit::component::input::Position::new(row, column))
        } else if request.refresh {
            Some(self.editor.read(cx).cursor_position())
        } else {
            None
        };
        self.editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(language(&path))
                .default_value(content)
        });
        self.editor_subscription = Some(cx.subscribe(&self.editor, |view, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                view.editor_changed(cx);
            }
        }));
        self.attach_editor_assist(path.clone(), cx);
        self.editor_path = Some(path);
        self.editor_dirty = false;
        self.project_search.external_conflict = false;
        if let Some(position) = cursor {
            self.editor.update(cx, |editor, cx| {
                editor.set_cursor_position(position, window, cx)
            });
        }
        if !request.refresh {
            self.settings_open = false;
            self.page = Page::Files;
        }
        cx.notify();
    }
    pub(super) fn external_file_notice(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .px_3()
            .py_2()
            .flex()
            .flex_wrap()
            .gap_2()
            .items_center()
            .child(
                subtitle("This file changed outside Xylo. Your edits are preserved.")
                    .w_full()
                    .min_w_0(),
            )
            .child(
                button("reload-external-file", "Reload from disk")
                    .flex_shrink_0()
                    .disabled(self.editor_dirty)
                    .on_click(cx.listener(|view, _, _, cx| {
                        if let Some(path) = view.editor_path.clone() {
                            view.request_editor_file(path, None, true, cx);
                        }
                    })),
            )
            .into_any_element()
    }
    pub(super) fn project_search_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.entity().downgrade();
        let rows = uniform_list(
            "project-search-results",
            self.project_search.hits.len(),
            move |range, _, cx| {
                weak.update(cx, |view, cx| {
                    range
                        .filter_map(|i| {
                            let hit = view.project_search.hits.get(i)?.clone();
                            let label = hit
                                .path
                                .strip_prefix(view.workspace.as_deref().unwrap_or(Path::new("")))
                                .unwrap_or(&hit.path)
                                .display()
                                .to_string();
                            Some(
                                gpui_kit::base::Button::new(("project-match", i))
                                    .accessibility_label(format!("{label}:{}", hit.line))
                                    .px_2()
                                    .cursor_pointer()
                                    .hover(|row| row.bg(rgb(hover())))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .items_start()
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(muted()))
                                                    .child(format!("{label}:{}", hit.line)),
                                            )
                                            .child(
                                                div()
                                                    .font_family("monospace")
                                                    .text_sm()
                                                    .whitespace_nowrap()
                                                    .child(hit.preview.clone()),
                                            ),
                                    )
                                    .w_full()
                                    .h(px(54.))
                                    .justify_start()
                                    .overflow_hidden()
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        view.request_editor_file(
                                            hit.path.clone(),
                                            Some((hit.line, hit.byte_column)),
                                            false,
                                            cx,
                                        );
                                    }))
                                    .into_any_element(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
            },
        )
        .track_scroll(&self.project_search.scroll)
        .flex_1()
        .min_h_0();
        div()
            .size_full()
            .p_6()
            .flex()
            .flex_col()
            .gap_3()
            .min_h_0()
            .child(heading(
                "Search project",
                "Find text in your project and open the matching line.",
            ))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div().flex_1().child(
                            Input::new(&self.project_search.query)
                                .aria_label("Search project contents"),
                        ),
                    )
                    .child(
                        primary("project-search-submit", "Search").on_click(
                            cx.listener(|view, _, _, cx| view.run_project_search(false, cx)),
                        ),
                    )
                    .when(self.project_search.request.is_some(), |row| {
                        row.child(
                            button("project-search-stop", "Stop").on_click(
                                cx.listener(|view, _, _, cx| view.cancel_project_search(cx)),
                            ),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        button(
                            "project-search-case",
                            if self.project_search.case_sensitive {
                                "Match case ✓"
                            } else {
                                "Match case"
                            },
                        )
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.project_search.case_sensitive =
                                !view.project_search.case_sensitive;
                            view.invalidate_project_search(cx);
                        })),
                    )
                    .child(
                        button(
                            "project-search-regex",
                            if self.project_search.regular_expression {
                                "Regular expression ✓"
                            } else {
                                "Regular expression"
                            },
                        )
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.project_search.regular_expression =
                                !view.project_search.regular_expression;
                            view.invalidate_project_search(cx);
                        })),
                    ),
            )
            .child(subtitle(self.project_search.notice.clone()))
            .child(rows)
            .when(self.project_search.next.is_some(), |page| {
                page.child(
                    button("project-search-more", "Next results")
                        .disabled(self.project_search.request.is_some())
                        .on_click(cx.listener(|view, _, _, cx| view.run_project_search(true, cx))),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
#[allow(dead_code)] // Used by the standalone native validation test target.
impl ProjectSearchUi {
    pub(super) fn validation_hits(&self) -> &[SearchHit] {
        &self.hits
    }
    pub(super) fn validation_request(&self) -> Option<Uuid> {
        self.request.map(|(id, _)| id)
    }
    pub(super) fn validation_opening(&self) -> Option<Uuid> {
        self.opening.as_ref().map(|request| request.id)
    }
    pub(super) fn validation_revision(&self) -> u64 {
        self.revision
    }
}
#[cfg(test)]
#[allow(dead_code)] // Used by the standalone native validation test target.
impl Xylo {
    pub(super) fn validation_search_query(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.project_search.query.update(cx, |input, cx| {
            input.set_value(query.to_owned(), window, cx)
        });
    }
}
