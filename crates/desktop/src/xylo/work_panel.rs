//! The chat page's chrome: the toolbar title and the conversation "…" menu,
//! the task results beside the chat and the file changes viewer. The panel
//! opens when requested; empty utility sections do not occupy the chat UI.
use super::*;
use crate::icons::{self, AppIcon};
use xylo_runtime::workspace::git::{ChangeGroup, GitChange, GitStatus};

#[derive(Default)]
pub(super) struct GitPanel {
    pub request: Option<Uuid>,
    pub diff_request: Option<Uuid>,
    diff_width: f32,
    pub root: Option<PathBuf>,
    pub status: Option<GitStatus>,
    pub statuses: BTreeMap<PathBuf, char>,
    pub selected: Option<(PathBuf, ChangeGroup)>,
    pub error: Option<String>,
    /// Lines of the selected change's diff.
    pub diff_lines: Vec<String>,
    pub diff_source: String,
    pub diff_hunks: Vec<xylo_runtime::workspace::git::hunks::Hunk>,
    pub original: Option<PathBuf>,
    pub hunk_request: Option<Uuid>,
}
impl GitPanel {
    pub(super) fn prepare(root: PathBuf, status: GitStatus) -> Self {
        let statuses = status
            .changes
            .iter()
            .map(|change| {
                (
                    root.join(&change.path),
                    if change.index == '?' {
                        'U'
                    } else if change.worktree != ' ' {
                        change.worktree
                    } else {
                        change.index
                    },
                )
            })
            .collect();
        Self {
            root: Some(root),
            status: Some(status),
            statuses,
            ..Self::default()
        }
    }
    /// Refresh the file list without dropping an open diff or its in-flight request.
    pub(super) fn take_status(&mut self, prepared: GitPanel) {
        self.request = None;
        self.root = prepared.root;
        self.status = prepared.status;
        self.statuses = prepared.statuses;
    }
}
impl Xylo {
    /// The toolbar's title: the chat's name on the chat page, the page's
    /// name elsewhere, nothing on settings pages (they carry their own
    /// heading). Renaming from the "…" menu edits it in place.
    pub(super) fn toolbar_title(&self) -> AnyElement {
        if let Some(input) = self.sidebar_state.header_rename_input() {
            return div()
                .flex_1()
                .min_w_0()
                .max_w(px(420.))
                .child(Input::new(&input).aria_label("Chat title").h(px(30.)))
                .into_any_element();
        }
        let title = match self.page {
            Page::Chat | Page::Files | Page::Changes => self
                .conversations
                .iter()
                .find(|chat| chat.id == self.session)
                .map(sidebar::display_title)
                .unwrap_or_else(|| "New chat".into()),
            Page::Drafts => "Saved drafts".into(),
            Page::Schedules => "Scheduled".into(),
            Page::Artifacts => "Artifacts".into(),
            Page::Search => "Search in project".into(),
            _ => String::new(),
        };
        div()
            .id("toolbar-title")
            .test_support()
            .aria_label(title.clone())
            .flex_1()
            .min_w_0()
            .single_line()
            .text_size(px(14.))
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(text()))
            .child(title)
            .into_any_element()
    }
    /// The open chat as plain text, oldest first, including the live turn.
    fn conversation_text(&self) -> String {
        // The live turn's text is not a history message yet.
        let live = self
            .turns
            .get(&self.session)
            .map(|turn| turn.transcript_text())
            .filter(|text| !text.is_empty())
            .map(|text| format!("assistant\n{text}"));
        self.messages
            .iter()
            .map(|message| format!("{}\n{}", message.role, message.content))
            .chain(live)
            .collect::<Vec<_>>()
            .join("\n\n")
    }
    /// The open chat as a Markdown document, for Export.
    fn conversation_markdown(&self) -> String {
        let title = self
            .conversations
            .iter()
            .find(|chat| chat.id == self.session)
            .map(sidebar::display_title)
            .unwrap_or_else(|| "New chat".into());
        let live = self
            .turns
            .get(&self.session)
            .map(|turn| turn.transcript_text())
            .filter(|text| !text.is_empty())
            .map(|text| ("assistant".to_owned(), text));
        let turns = self
            .messages
            .iter()
            .map(|message| (message.role.clone(), message.content.clone()))
            .chain(live)
            .map(|(role, content)| {
                let speaker = if role == "user" { "You" } else { "Xylo" };
                format!("## {speaker}\n\n{}", content.trim())
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        format!("# {title}\n\n{turns}\n")
    }
    /// Asks where to save the chat as Markdown and writes it off the UI
    /// thread; the outcome shows as a toast or an error.
    fn export_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let markdown = self.conversation_markdown();
        let file_name = format!("{}.md", export_file_stem(&markdown));
        let session = self.session;
        cx.spawn_in(window, async move |view, cx| {
            let Some(file) = rfd::AsyncFileDialog::new()
                .set_title("Export conversation")
                .set_file_name(file_name)
                .add_filter("Markdown", &["md"])
                .save_file()
                .await
            else {
                return;
            };
            let path = file.path().to_path_buf();
            let _ = view.update(cx, |view, cx| {
                if let Some(host) = &view.host
                    && let Err(error) = host.export_conversation(session, path)
                {
                    view.error = Some(error.to_string());
                }
                cx.notify();
            });
        })
        .detach();
    }
    /// The chat's "…" menu: Rename, Copy conversation, Export and Archive. A
    /// chat with nothing in it yet has no menu.
    pub(super) fn conversation_actions(&self, cx: &Context<Self>) -> Option<AnyElement> {
        use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
        let saved = self
            .conversations
            .iter()
            .any(|chat| chat.id == self.session);
        if !saved
            && self.messages.is_empty()
            && !self.has_live_transcript()
            && self.workspace.is_none()
        {
            return None;
        }
        let weak = cx.entity().downgrade();
        let session = self.session;
        let busy = self.chat_busy(session);
        let separate_checkout = self.workspace.is_some();
        Some(
            icons::menu_trigger("conversation-actions", AppIcon::More, "Chat options")
                .size(px(34.))
                .dropdown_menu(move |menu, _, _| {
                    let checkout = weak.clone();
                    let menu = if separate_checkout {
                        menu.item(
                            PopupMenuItem::new("New chat in separate checkout…").on_click(
                                move |_, _, cx| {
                                    let _ = checkout.update(cx, |view, cx| {
                                        let previous = view.session;
                                        view.new_chat(cx);
                                        if view.session != previous {
                                            view.start_worktree_chat(cx);
                                        }
                                    });
                                },
                            ),
                        )
                    } else {
                        menu
                    };
                    let rename = weak.clone();
                    let copy = weak.clone();
                    let export = weak.clone();
                    let find = weak.clone();
                    let archive = weak.clone();
                    let menu = if saved {
                        menu.item(PopupMenuItem::new("Rename chat").on_click(
                            move |_, window, cx| {
                                let _ = rename.update(cx, |view, cx| {
                                    view.start_header_rename(session, window, cx)
                                });
                            },
                        ))
                    } else {
                        menu
                    };
                    let menu = menu
                        .item(
                            PopupMenuItem::new("Find in chat").on_click(move |_, window, cx| {
                                let _ =
                                    find.update(cx, |view, cx| view.open_chat_search(window, cx));
                            }),
                        )
                        .item(
                            PopupMenuItem::new("Copy conversation").on_click(move |_, _, cx| {
                                if let Ok(text) =
                                    copy.update(cx, |view, _| view.conversation_text())
                                {
                                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                                }
                            }),
                        )
                        .item(PopupMenuItem::new("Export conversation…").on_click(
                            move |_, window, cx| {
                                let _ = export
                                    .update(cx, |view, cx| view.export_conversation(window, cx));
                            },
                        ));
                    if !saved {
                        return menu;
                    }
                    menu.separator()
                        .item(sidebar::archive_item(busy, move |cx| {
                            let _ = archive.update(cx, |view, cx| view.archive_chat(session, cx));
                        }))
                })
                .into_any_element(),
        )
    }
    fn chat_details_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let agents = self.subagent_count();
        let changes = self
            .git_panel
            .status
            .as_ref()
            .map_or(0, |status| status.changes.len());
        let jobs = self
            .turns
            .get(&self.session)
            .map_or(0, |turn| turn.shell_jobs.len());
        let mut content = div()
            .id("chat-details")
            .test_support()
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(12.));
        if agents > 0 {
            content = content.child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .px_2()
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .child("Subagents"),
                    )
                    .child(self.subagents_panel(cx)),
            );
        }
        if changes > 0 {
            content = content.child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(self.chat_details_link(
                        "chat-detail-changes",
                        AppIcon::Changes,
                        format!(
                            "{changes} {} changed",
                            if changes == 1 { "file" } else { "files" }
                        ),
                        "changes",
                        cx,
                    ))
                    .children(self.changed_file_rows(Some(5), cx))
                    .when(changes > 5, |section| {
                        section.child(self.chat_details_link(
                            "chat-detail-all-changes",
                            AppIcon::ChevronRight,
                            "View all files".into(),
                            "changes",
                            cx,
                        ))
                    }),
            );
        }
        if jobs > 0 {
            content = content.child(self.chat_details_link(
                "chat-detail-commands",
                AppIcon::Terminal,
                format!("{jobs} {}", if jobs == 1 { "command" } else { "commands" }),
                "jobs",
                cx,
            ));
        }
        if self.subagent_activity_loading() {
            content = content.child(loading_caption("Loading saved results…"));
        } else if self.subagent_activity_error().is_some() {
            content =
                content.child(
                    div()
                        .px_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(muted()))
                                .child("Saved results couldn’t be loaded."),
                        )
                        .child(pill("retry-subagent-results", "Retry").on_click(
                            cx.listener(|view, _, _, cx| view.load_subagent_activity(cx)),
                        )),
                );
        }
        if agents == 0
            && changes == 0
            && jobs == 0
            && !self.subagent_activity_loading()
            && self.subagent_activity_error().is_none()
        {
            content = content.child(
                div()
                    .id("task-details-empty")
                    .test_support()
                    .px_2()
                    .py_2()
                    .text_size(px(13.))
                    .line_height(relative(1.5))
                    .text_color(rgb(muted()))
                    .child("Subagent results and changed files appear here as Xylo works."),
            );
        }
        content.into_any_element()
    }
    fn chat_details_link(
        &self,
        id: &'static str,
        glyph: AppIcon,
        label: String,
        target: &'static str,
        cx: &Context<Self>,
    ) -> AnyElement {
        focus_ring(gpui_kit::base::Button::new(id))
            .accessibility_label(label.clone())
            .w_full()
            .min_h(px(35.))
            .py(px(4.))
            .flex()
            .items_center()
            .justify_start()
            .gap(px(9.))
            .text_size(px(12.))
            .text_color(rgb(text_secondary()))
            .hover(|style| style.text_color(rgb(text())))
            .child(icons::small(glyph).size(px(15.)))
            .child(label)
            .on_click(cx.listener(move |view, _, window, cx| {
                view.settings.ui.work_panel = target.into();
                view.activate_work_panel(window, cx);
                cx.notify();
            }))
            .into_any_element()
    }
    /// Hides the work panel and returns any panel route to the chat.
    pub(super) fn close_work_panel(&mut self, cx: &mut Context<Self>) {
        if matches!(self.page, Page::Files | Page::Changes) {
            self.page = Page::Chat;
        }
        self.settings.ui.work_panel_open = false;
        self.terminal_visible = false;
        self.persist_ui(cx);
        cx.notify();
    }
    /// Helpers the open chat has started; the Subagents tab shows only while
    /// there are some.
    fn subagent_count(&self) -> usize {
        self.turns.get(&self.session).map_or(0, |turn| {
            turn.subagents.len() + turn.earlier_subagents.len()
        })
    }
    pub(super) fn refresh_git(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.workspace.clone() else {
            return;
        };
        let request = Uuid::new_v4();
        self.git_panel.request = Some(request);
        self.git_panel.root = Some(root.clone());
        self.git_panel.error = None;
        // The open diff, and one still loading, survive a status refresh.
        self.host_call(cx, |host| host.git_status(request, root));
    }
    pub(super) fn git_status_received(
        &mut self,
        request: Uuid,
        root: PathBuf,
        result: Result<GitStatus, String>,
        cx: &mut Context<Self>,
    ) {
        if self.workspace.as_ref() != Some(&root) || self.git_panel.request != Some(request) {
            return;
        }
        match result {
            Ok(status) => {
                let expected_root = root.clone();
                let prepared = cx
                    .background_executor()
                    .spawn(async move { GitPanel::prepare(root, status) });
                cx.spawn(async move |view, cx| {
                    let panel = prepared.await;
                    let _ = view.update(cx, |view, cx| {
                        if view.workspace.as_ref() == Some(&expected_root)
                            && view.git_panel.request == Some(request)
                        {
                            view.git_panel.take_status(panel);
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
            Err(error) => {
                self.git_panel.request = None;
                self.git_panel.error = Some(error);
                cx.notify();
            }
        }
    }
    pub(super) fn git_diff_received(
        &mut self,
        request: Uuid,
        root: PathBuf,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        if self.workspace.as_ref() != Some(&root) || self.git_panel.diff_request != Some(request) {
            return;
        }
        self.git_panel.diff_request = None;
        match result {
            Ok(diff) => {
                self.git_panel.diff_hunks =
                    xylo_runtime::workspace::git::hunks::parse(&diff).unwrap_or_default();
                self.git_panel.diff_lines = diff.lines().map(str::to_owned).collect();
                self.git_panel.diff_source = diff;
                self.git_panel.diff_width = self
                    .git_panel
                    .diff_lines
                    .iter()
                    .map(|line| line.chars().count())
                    .max()
                    .unwrap_or(0) as f32
                    * 7.5
                    + 190.;
            }
            Err(error) => self.git_panel.error = Some(error),
        }
        cx.notify();
    }
    pub(super) fn open_git_change(
        &mut self,
        group: ChangeGroup,
        change: GitChange,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.workspace.clone() else {
            return;
        };
        let request = Uuid::new_v4();
        self.git_panel.diff_request = Some(request);
        self.git_panel.selected = Some((change.path.clone(), group));
        self.git_panel.error = None;
        self.git_panel.diff_lines.clear();
        self.git_panel.diff_source.clear();
        self.git_panel.diff_hunks.clear();
        self.git_panel.original = change.original.clone();
        self.host_call(cx, |host| {
            host.git_file_diff(request, root, change.path, change.original, group)
        });
    }
    pub(super) fn activate_work_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal_visible = false;
        match self.settings.ui.work_panel.as_str() {
            "changes" => self.refresh_git(cx),
            "terminal" => self.show_terminal(window, cx),
            "subagents" | "details" => {
                self.refresh_git(cx);
                self.load_subagent_activity(cx);
            }
            _ => {
                self.refresh_files(cx);
                if self.git_panel.status.is_none() {
                    self.refresh_git(cx);
                }
            }
        }
    }
    /// Clamp only the rendered panel so a narrow window keeps readable chat.
    /// The saved width returns automatically when the viewport grows again.
    fn available_work_panel_width(&self, total: f32) -> f32 {
        let sidebar = if self.settings.ui.sidebar_visible || self.settings_open {
            self.settings.ui.sidebar_width
        } else {
            0.
        };
        (total - sidebar).max(0.)
    }
    pub(super) fn effective_work_panel_width(&self, total: f32) -> f32 {
        let available = self.available_work_panel_width(total);
        self.settings
            .ui
            .work_panel_width
            .max(320.)
            .min((available - 400.).max(320.))
    }
    pub(super) fn native_workspace(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open =
            self.settings.ui.work_panel_open || matches!(self.page, Page::Files | Page::Changes);
        let total = f32::from(window.viewport_size().width);
        let available = self.available_work_panel_width(total);
        let panel_width = if available < 720. {
            0.
        } else {
            self.effective_work_panel_width(total)
        };
        self.chat_width
            .set(available - if open { panel_width } else { 0. });
        let chat = self.chat(cx);
        if !open {
            return chat;
        }
        let selected = if self.page == Page::Files {
            "files"
        } else if self.page == Page::Changes {
            "changes"
        } else if self.settings.ui.work_panel == "subagents" {
            "details"
        } else {
            &self.settings.ui.work_panel
        };
        let compact_tabs = (if available < 720. {
            available
        } else {
            panel_width
        }) < 440.;
        let mut tabs = div()
            .h(px(52.))
            .flex_shrink_0()
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(2.))
            .bg(rgb(sidebar()))
            .border_b_1()
            .border_color(rgb(border()));
        let change_count = self
            .git_panel
            .status
            .as_ref()
            .map_or(0, |status| status.changes.len());
        let jobs = self
            .turns
            .get(&self.session)
            .map_or(0, |turn| turn.shell_jobs.len());
        let tab_list = [
            ("details", "Activity", AppIcon::Agent),
            ("changes", "Changes", AppIcon::Changes),
            ("files", "Files", AppIcon::Files),
            ("terminal", "Terminal", AppIcon::Terminal),
            ("jobs", "Commands", AppIcon::Terminal),
        ];
        for (key, label, glyph) in tab_list {
            if key != selected
                && match key {
                    "changes" => change_count == 0,
                    "jobs" => jobs == 0,
                    "files" | "terminal" => true,
                    _ => false,
                }
            {
                continue;
            }
            let active = key == selected;
            tabs = tabs.child(
                focus_ring(gpui_kit::base::Button::new(SharedString::from(format!(
                    "work-tab-{key}"
                ))))
                .accessibility_label(label)
                .h(px(30.))
                .px(px(if compact_tabs { 8. } else { 10. }))
                .rounded(px(8.))
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(12.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(if active { text() } else { muted() }))
                .bg(rgb(if active {
                    crate::ui::selected()
                } else {
                    sidebar()
                }))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(hover())).text_color(rgb(text())))
                .child(icons::small(glyph))
                .when(!compact_tabs || active, |button| button.child(label))
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.page = Page::Chat;
                    view.settings_open = false;
                    view.settings.ui.work_panel = key.into();
                    view.settings.ui.work_panel_open = true;
                    view.activate_work_panel(window, cx);
                    view.persist_ui(cx);
                    cx.notify();
                })),
            );
        }
        tabs = tabs.child(div().flex_1()).child(
            icons::icon_button("close-work-panel", AppIcon::Close, "Hide work panel")
                .on_click(cx.listener(|view, _, _, cx| view.close_work_panel(cx))),
        );
        let content = match selected {
            "changes" => self.git_changes_panel(cx),
            "terminal" => self
                .terminal
                .as_ref()
                .map(|terminal| terminal.clone().into_any_element())
                .unwrap_or_else(|| {
                    if self.terminal_starting {
                        div()
                            .p_4()
                            .child(loading_caption("Starting shell…"))
                            .into_any_element()
                    } else {
                        panel_hint("Open a project to start its shell.").into_any_element()
                    }
                }),
            "details" | "subagents" => self.chat_details_panel(cx),
            "jobs" => self.shell_jobs_panel(cx),
            _ => self.files_page(cx),
        };
        let total = f32::from(window.viewport_size().width);
        let stacked = self.available_work_panel_width(total) < 720.;
        let work_width = self.effective_work_panel_width(total);
        // Home includes generous vertical spacing. Scroll it rather than letting
        // its composer paint over the work pane in a short stacked viewport.
        let chat = if stacked
            && self.messages.is_empty()
            && !self.running()
            && !self.history_loading()
            && !self.has_live_transcript()
        {
            div()
                .w_full()
                .h_full()
                .min_h_0()
                .child(chat)
                .into_any_element()
        } else {
            chat
        };
        div()
            .id("native-workspace")
            .test_support()
            .size_full()
            .min_h_0()
            .flex()
            .when(stacked, |layout| layout.flex_col())
            .child(
                div()
                    .id("native-chat-pane")
                    .test_support()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .when(stacked, |pane| pane.w_full().overflow_y_scroll())
                    .when(!stacked, |pane| pane.h_full())
                    .child(chat),
            )
            .child(
                div()
                    .id("native-work-pane")
                    .test_support()
                    .flex_shrink_0()
                    .min_h_0()
                    .relative()
                    .when(stacked, |pane| pane.w_full().h(relative(0.5)).border_t_1())
                    .when(!stacked, |pane| {
                        pane.w(px(work_width))
                            .h_full()
                            .border_l_1()
                            .child(self.resize_edge(Panel::Work))
                    })
                    .flex()
                    .flex_col()
                    .bg(rgb(sidebar()))
                    .border_color(rgb(border()))
                    .child(tabs)
                    .child(div().flex_1().min_h_0().bg(rgb(panel())).child(content)),
            )
            .into_any_element()
    }
    fn changed_file_rows(&self, limit: Option<usize>, cx: &Context<Self>) -> Vec<AnyElement> {
        self.git_panel
            .status
            .as_ref()
            .into_iter()
            .flat_map(|status| &status.changes)
            .take(limit.unwrap_or(usize::MAX))
            .enumerate()
            .map(|(index, change)| self.changed_file_row(index, change, cx))
            .collect()
    }
    fn changed_file_row(&self, index: usize, change: &GitChange, cx: &Context<Self>) -> AnyElement {
        let change = change.clone();
        let name = change
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let parent = change
            .path
            .parent()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let status = change_label(&change);
        focus_ring(gpui_kit::base::Button::new(("changed-file", index)))
            .accessibility_label(format!("Review {}, {status}", change.path.display()))
            .w_full()
            .min_w_0()
            .h(px(48.))
            .px_2()
            .rounded(px(8.))
            .flex()
            .items_center()
            .justify_start()
            .gap_2()
            .text_color(rgb(text_secondary()))
            .hover(|style| style.bg(rgb(hover())))
            .child(icons::small(AppIcon::File).flex_shrink_0())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().single_line().text_size(px(13.)).child(name))
                    .when(!parent.is_empty(), |column| {
                        column.child(
                            div()
                                .single_line()
                                .text_size(px(11.))
                                .text_color(rgb(muted()))
                                .child(parent),
                        )
                    }),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(rgb(muted()))
                    .child(status),
            )
            .child(icons::small(AppIcon::ChevronRight).text_color(rgb(muted())))
            .on_click(cx.listener(move |view, _, _, cx| {
                view.page = Page::Chat;
                view.settings.ui.work_panel = "changes".into();
                view.open_git_change(preferred_change_group(&change), change.clone(), cx);
                cx.notify();
            }))
            .into_any_element()
    }
    fn git_changes_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.git_panel.selected.as_ref();
        let mut header = div()
            .h(px(44.))
            .flex_shrink_0()
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(rgb(border()));
        if let Some((path, group)) = selected {
            header = header
                .child(
                    icons::icon_button("back-to-changed-files", AppIcon::Back, "All changed files")
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.git_panel.selected = None;
                            view.git_panel.diff_request = None;
                            view.git_panel.diff_lines.clear();
                            view.git_panel.diff_source.clear();
                            view.git_panel.diff_hunks.clear();
                            view.git_panel.error = None;
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .single_line()
                        .text_size(px(13.))
                        .child(
                            path.file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned(),
                        ),
                );
            // A file can have both versions; keep each reachable without listing it twice.
            if let Some(change) = self
                .git_panel
                .status
                .as_ref()
                .and_then(|status| status.changes.iter().find(|change| &change.path == path))
                && change.in_group(ChangeGroup::Staged)
                && change.in_group(ChangeGroup::Unstaged)
            {
                let change = change.clone();
                let next = if *group == ChangeGroup::Staged {
                    ChangeGroup::Unstaged
                } else {
                    ChangeGroup::Staged
                };
                header = header.child(
                    pill(
                        "switch-change-version",
                        if next == ChangeGroup::Staged {
                            "View staged"
                        } else {
                            "View working"
                        },
                    )
                    .text_size(px(11.))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.open_git_change(next, change.clone(), cx)
                    })),
                );
            }
        } else {
            let count = self
                .git_panel
                .status
                .as_ref()
                .map_or(0, |status| status.changes.len());
            header = header
                .child(
                    div()
                        .flex_1()
                        .text_size(px(13.))
                        .font_weight(FontWeight::MEDIUM)
                        .child("File changes"),
                )
                .when(count > 0, |header| {
                    header.child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(muted()))
                            .child(count.to_string()),
                    )
                });
        }
        header = header.child(
            icons::icon_button("refresh-git", AppIcon::Refresh, "Refresh changes")
                .on_click(cx.listener(|view, _, _, cx| view.refresh_git(cx))),
        );
        let mut panel = div()
            .id("file-changes-panel")
            .test_support()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header);
        if let Some(error) = &self.git_panel.error {
            let copy =
                if error.contains("not a Git worktree") || error.contains("not a git repository") {
                    "File history is unavailable for this folder.".to_owned()
                } else {
                    error.clone()
                };
            panel = panel.child(
                div()
                    .id("file-changes-error")
                    .test_support()
                    .p_3()
                    .text_size(px(12.))
                    .text_color(rgb(muted()))
                    .child(copy),
            );
        }
        if let Some((path, group)) = selected {
            panel = panel.child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(11.))
                    .text_color(rgb(muted()))
                    .child(format!(
                        "{}{}",
                        path.display(),
                        if *group == ChangeGroup::Staged {
                            " · Staged"
                        } else {
                            ""
                        }
                    )),
            );
            let indices = visible_diff_indices(&self.git_panel.diff_lines);
            if self.git_panel.diff_request.is_some() {
                panel = panel.child(div().p_3().child(loading_caption("Loading changes…")));
            } else if indices.is_empty() && self.git_panel.error.is_none() {
                panel = panel.child(panel_hint("No text changes in this version."));
            }
            if indices.is_empty() {
                return panel.into_any_element();
            }
            let weak = cx.entity().downgrade();
            let diff = uniform_list("git-diff-lines", indices.len(), move |range, _, cx| {
                weak.update(cx, |view, cx| {
                    range
                        .filter_map(|index| view.git_diff_line(indices[index], cx))
                        .collect()
                })
                .unwrap_or_default()
            })
            .track_scroll(&self.diff_scroll)
            .w_full()
            .min_w(px(self.git_panel.diff_width))
            .h_full()
            .min_h_0();
            panel = panel.child(
                div()
                    .id("git-diff-pane")
                    .test_support()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(
                        div()
                            .id("git-code-scroll")
                            .size_full()
                            .overflow_x_scroll()
                            .child(diff),
                    ),
            );
        } else {
            let count = self
                .git_panel
                .status
                .as_ref()
                .map_or(0, |status| status.changes.len());
            let empty = count == 0;
            let weak = cx.entity().downgrade();
            let rows = uniform_list("changed-file-rows", count, move |range, _, cx| {
                weak.update(cx, |view, cx| {
                    range
                        .filter_map(|index| {
                            let change = view.git_panel.status.as_ref()?.changes.get(index)?;
                            Some(view.changed_file_row(index, change, cx))
                        })
                        .collect()
                })
                .unwrap_or_default()
            })
            .h_full()
            .w_full();
            panel = panel.child(div().id("changed-files-list").test_support().flex_1().min_h_0().p_2()
                .when(!empty, |list| list.child(rows))
                .when(empty && self.git_panel.request.is_some(), |list| list.child(loading_caption("Reading file changes…")))
                .when(empty && self.git_panel.request.is_none() && self.git_panel.error.is_none(), |list| list.child(
                    div().id("file-changes-empty").test_support().child(panel_hint("No changed files."))
                ))
                .when(self.git_panel.status.as_ref().is_some_and(|status| status.truncated), |list| list.child(panel_hint("Showing the first files. More changes are available in the project."))));
        }
        panel.into_any_element()
    }
}

fn preferred_change_group(change: &GitChange) -> ChangeGroup {
    if change.index == '?' {
        ChangeGroup::Untracked
    } else if change.in_group(ChangeGroup::Unstaged) {
        ChangeGroup::Unstaged
    } else {
        ChangeGroup::Staged
    }
}
fn change_label(change: &GitChange) -> &'static str {
    match if change.worktree != ' ' {
        change.worktree
    } else {
        change.index
    } {
        '?' | 'A' => "New",
        'D' => "Deleted",
        'R' => "Renamed",
        'C' => "Copied",
        'U' => "Conflict",
        _ => "Modified",
    }
}
/// Filter patch transport metadata, keeping original indices for comments and hunk actions.
fn visible_diff_indices(lines: &[String]) -> Vec<usize> {
    let mut in_hunk = false;
    lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            if line.starts_with("diff --git ") {
                in_hunk = false;
            }
            if line.starts_with("@@ ") {
                in_hunk = true;
            }
            (in_hunk || line.starts_with("Binary files ") || line == "GIT binary patch")
                .then_some(index)
        })
        .collect()
}

/// A small muted line for an empty or idle pane in the narrow work panel.
pub(super) fn panel_hint(text: &'static str) -> Div {
    div()
        .px(px(14.))
        .py(px(12.))
        .text_size(px(12.5))
        .text_color(rgb(muted()))
        .single_line()
        .child(text)
}

/// A file name for an exported chat: its Markdown title with characters that
/// file systems reject replaced, at most 80 characters.
fn export_file_stem(markdown: &str) -> String {
    let title = markdown
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("# "))
        .unwrap_or("Conversation");
    let stem: String = title
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '-'
            } else {
                c
            }
        })
        .take(80)
        .collect();
    let stem = stem.trim().trim_matches('.').trim();
    if stem.is_empty() {
        "Conversation".into()
    } else {
        stem.to_owned()
    }
}

/// A spinner beside a short caption, for a wait inside a panel.
fn loading_caption(caption: &'static str) -> Div {
    div()
        .px_3()
        .py_2()
        .flex()
        .items_center()
        .gap_2()
        .text_size(px(12.5))
        .text_color(rgb(muted()))
        .child(crate::motion::spinner_inline())
        .child(caption)
}

#[cfg(all(test, not(feature = "validation")))]
#[path = "work_panel/tests.rs"]
mod tests;
