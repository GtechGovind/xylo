//! The chat page's chrome: the toolbar title and the conversation "…" menu,
//! the work panel beside the chat (Changes, Files, Terminal and, while the
//! chat has helpers, Subagents) and its virtualized Git folder tree. The work
//! panel opens only when the user asks for it or opens a file or diff; it is
//! never shown on other pages.
use super::*;
use crate::icons::{self, AppIcon};
use xylo_runtime::workspace::git::{ChangeGroup, GitChange, GitStatus};

#[derive(Clone)]
enum GitRow {
    Group(ChangeGroup, &'static str, usize),
    Folder(ChangeGroup, PathBuf, usize),
    File(ChangeGroup, GitChange, usize),
}
#[derive(Default)]
struct Node {
    children: BTreeMap<(bool, String), Node>,
    file: Option<GitChange>,
    path: PathBuf,
}
#[derive(Default)]
pub(super) struct GitPanel {
    pub request: Option<Uuid>,
    pub diff_request: Option<Uuid>,
    diff_width: f32,
    pub root: Option<PathBuf>,
    pub status: Option<GitStatus>,
    trees: Vec<(ChangeGroup, &'static str, usize, Node)>,
    pub statuses: BTreeMap<PathBuf, char>,
    rows: std::sync::Arc<Vec<GitRow>>,
    collapsed: BTreeSet<(u8, PathBuf)>,
    pub selected: Option<(PathBuf, ChangeGroup)>,
    pub error: Option<String>,
    /// Lines of the selected change's diff.
    pub diff_lines: Vec<String>,
}
fn group_id(group: ChangeGroup) -> u8 {
    match group {
        ChangeGroup::Staged => 0,
        ChangeGroup::Unstaged => 1,
        ChangeGroup::Untracked => 2,
    }
}
impl GitPanel {
    pub(super) fn prepare(
        root: PathBuf,
        status: GitStatus,
        collapsed: BTreeSet<(u8, PathBuf)>,
    ) -> Self {
        let mut panel = Self {
            root: Some(root.clone()),
            collapsed,
            ..Self::default()
        };
        panel.statuses = status
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
        for (group, label) in [
            (ChangeGroup::Staged, "Staged"),
            (ChangeGroup::Unstaged, "Unstaged"),
            (ChangeGroup::Untracked, "Untracked"),
        ] {
            let mut count = 0;
            let mut tree = Node::default();
            for change in status
                .changes
                .iter()
                .filter(|change| change.in_group(group))
            {
                count += 1;
                let mut node = &mut tree;
                let components: Vec<_> = change.path.components().collect();
                let mut path = PathBuf::new();
                for (index, component) in components.iter().enumerate() {
                    path.push(component.as_os_str());
                    node = node
                        .children
                        .entry((
                            index + 1 == components.len(),
                            component.as_os_str().to_string_lossy().into_owned(),
                        ))
                        .or_default();
                    node.path = path.clone();
                }
                node.file = Some(change.clone());
            }
            if count > 0 {
                panel.trees.push((group, label, count, tree));
            }
        }
        panel.status = Some(status);
        panel.rebuild();
        panel
    }
    /// Takes the status tree from a freshly prepared panel. The selected
    /// file, its diff and a diff still loading stay, so a refresh never drops
    /// a click.
    pub(super) fn take_tree(&mut self, prepared: GitPanel) {
        let GitPanel {
            root,
            status,
            trees,
            statuses,
            rows,
            collapsed,
            ..
        } = prepared;
        self.request = None;
        self.root = root;
        self.status = status;
        self.trees = trees;
        self.statuses = statuses;
        self.rows = rows;
        self.collapsed = collapsed;
    }
    fn rebuild(&mut self) {
        fn walk(
            node: &Node,
            depth: usize,
            group: ChangeGroup,
            collapsed: &BTreeSet<(u8, PathBuf)>,
            rows: &mut Vec<GitRow>,
        ) {
            for child in node.children.values() {
                if let Some(change) = &child.file {
                    rows.push(GitRow::File(group, change.clone(), depth));
                } else {
                    rows.push(GitRow::Folder(group, child.path.clone(), depth));
                    if !collapsed.contains(&(group_id(group), child.path.clone())) {
                        walk(child, depth + 1, group, collapsed, rows);
                    }
                }
            }
        }
        let mut rows = Vec::new();
        for (group, label, count, tree) in &self.trees {
            rows.push(GitRow::Group(*group, label, *count));
            if !self.collapsed.contains(&(group_id(*group), PathBuf::new())) {
                walk(tree, 0, *group, &self.collapsed, &mut rows);
            }
        }
        self.rows = std::sync::Arc::new(rows);
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
            .text_size(px(13.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(text_secondary()))
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
            let written = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move { std::fs::write(path, markdown) }
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                match written {
                    Ok(()) => {
                        let name = path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        view.status = format!("Exported to {name}").into();
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Conversation export failed");
                        view.error = Some("The conversation could not be exported.".into());
                    }
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
        if !saved && self.messages.is_empty() && !self.has_live_transcript() {
            return None;
        }
        let weak = cx.entity().downgrade();
        let session = self.session;
        let busy = self.chat_busy(session);
        Some(
            icons::menu_trigger(
                "conversation-actions",
                AppIcon::More,
                "Conversation actions",
            )
            .dropdown_menu(move |menu, _, _| {
                let rename = weak.clone();
                let copy = weak.clone();
                let export = weak.clone();
                let archive = weak.clone();
                let menu = if saved {
                    menu.item(PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
                        let _ = rename
                            .update(cx, |view, cx| view.start_header_rename(session, window, cx));
                    }))
                } else {
                    menu
                };
                let menu = menu
                    .item(
                        PopupMenuItem::new("Copy conversation").on_click(move |_, _, cx| {
                            if let Ok(text) = copy.update(cx, |view, _| view.conversation_text()) {
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                            }
                        }),
                    )
                    .item(
                        PopupMenuItem::new("Export…").on_click(move |_, window, cx| {
                            let _ =
                                export.update(cx, |view, cx| view.export_conversation(window, cx));
                        }),
                    );
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
                let collapsed = self.git_panel.collapsed.clone();
                let expected_root = root.clone();
                let prepared = cx
                    .background_executor()
                    .spawn(async move { GitPanel::prepare(root, status, collapsed) });
                cx.spawn(async move |view, cx| {
                    let panel = prepared.await;
                    let _ = view.update(cx, |view, cx| {
                        if view.workspace.as_ref() == Some(&expected_root)
                            && view.git_panel.request == Some(request)
                        {
                            view.git_panel.take_tree(panel);
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
                self.git_panel.diff_lines = diff.lines().map(str::to_owned).collect();
                self.git_panel.diff_width = self
                    .git_panel
                    .diff_lines
                    .iter()
                    .map(|line| line.chars().count())
                    .max()
                    .unwrap_or(0) as f32
                    * 7.5
                    + 20.;
            }
            Err(error) => self.git_panel.error = Some(error),
        }
        cx.notify();
    }
    fn open_git_change(&mut self, group: ChangeGroup, change: GitChange, cx: &mut Context<Self>) {
        let Some(root) = self.workspace.clone() else {
            return;
        };
        let request = Uuid::new_v4();
        self.git_panel.diff_request = Some(request);
        self.git_panel.selected = Some((change.path.clone(), group));
        self.git_panel.error = None;
        self.git_panel.diff_lines.clear();
        self.host_call(cx, |host| {
            host.git_file_diff(request, root, change.path, change.original, group)
        });
    }
    pub(super) fn activate_work_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal_visible = false;
        match self.settings.ui.work_panel.as_str() {
            "changes" => self.refresh_git(cx),
            "terminal" => self.show_terminal(window, cx),
            "subagents" => {}
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
        let chat = self.chat(cx);
        let open =
            self.settings.ui.work_panel_open || matches!(self.page, Page::Files | Page::Changes);
        if !open {
            return chat;
        }
        let subagents = self.subagent_count();
        let selected = if self.page == Page::Files {
            "files"
        } else if self.page == Page::Changes {
            "changes"
        } else if self.settings.ui.work_panel == "subagents" && subagents == 0 {
            "files"
        } else {
            &self.settings.ui.work_panel
        };
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
        let tab_list = [
            ("changes", "Changes", AppIcon::Changes),
            ("files", "Files", AppIcon::Files),
            ("terminal", "Terminal", AppIcon::Terminal),
            ("subagents", "Subagents", AppIcon::Agent),
        ];
        for (key, label, glyph) in tab_list {
            if key == "subagents" && subagents == 0 {
                continue;
            }
            let active = key == selected;
            tabs = tabs.child(
                focus_ring(gpui_kit::base::Button::new(SharedString::from(format!(
                    "work-tab-{key}"
                ))))
                .accessibility_label(label)
                .h(px(30.))
                .px(px(10.))
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
                .child(label)
                .when(key == "subagents", |tab| {
                    tab.child(
                        div()
                            .min_w(px(18.))
                            .h(px(18.))
                            .px(px(5.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(rgb(well()))
                            .text_size(px(11.))
                            .text_color(rgb(muted()))
                            .child(subagents.to_string()),
                    )
                })
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
            "subagents" => self.subagents_panel(cx),
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
                .min_h(px(460.))
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
            .child(crate::motion::slide_in_x(
                "native-work-pane-enter",
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
                // The pane opens from the edge it lives on: the right, or the
                // bottom when the window is narrow and the panes stack.
                if stacked { 0. } else { 24. },
            ))
            .into_any_element()
    }
    fn git_changes_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.git_panel.rows.clone();
        let weak = cx.entity().downgrade();
        let tree = uniform_list("git-tree-rows", rows.len(), move |range, _, cx| {
            weak.update(cx, |view, cx| {
                range
                    .filter_map(|index| {
                        let row = rows.get(index)?.clone();
                        let (group, path, depth, label, status, folder) = match &row {
                            GitRow::Group(group, label, count) => (
                                *group,
                                PathBuf::new(),
                                0,
                                format!("{label}  {count}"),
                                None,
                                true,
                            ),
                            GitRow::Folder(group, path, depth) => (
                                *group,
                                path.clone(),
                                *depth,
                                path.file_name()?.to_string_lossy().into_owned(),
                                None,
                                true,
                            ),
                            GitRow::File(group, change, depth) => (
                                *group,
                                change.path.clone(),
                                *depth,
                                change.path.file_name()?.to_string_lossy().into_owned(),
                                Some(if *group == ChangeGroup::Staged {
                                    change.index
                                } else if *group == ChangeGroup::Untracked {
                                    'U'
                                } else {
                                    change.worktree
                                }),
                                false,
                            ),
                        };
                        let collapsed = view
                            .git_panel
                            .collapsed
                            .contains(&(group_id(group), path.clone()));
                        let selected = view
                            .git_panel
                            .selected
                            .as_ref()
                            .is_some_and(|(p, g)| p == &path && *g == group);
                        Some(
                            button(("git-row", index), "")
                                .accessibility_label(format!(
                                    "{}{}",
                                    if folder {
                                        if collapsed { "Expand " } else { "Collapse " }
                                    } else {
                                        "Open diff "
                                    },
                                    label
                                ))
                                .w_full()
                                .h(px(30.))
                                .pl(px(8. + depth as f32 * 14.))
                                .pr_2()
                                .flex()
                                .items_center()
                                .justify_start()
                                .gap_1()
                                .text_xs()
                                .bg(rgb(if selected {
                                    crate::ui::selected()
                                } else {
                                    bg()
                                }))
                                .child(icons::small(if folder {
                                    if collapsed {
                                        AppIcon::ChevronRight
                                    } else {
                                        AppIcon::ChevronDown
                                    }
                                } else {
                                    AppIcon::File
                                }))
                                .child(div().flex_1().min_w_0().single_line().child(label))
                                .when_some(status, |row, status| {
                                    row.child(
                                        div()
                                            .text_color(rgb(if status == 'D' {
                                                danger()
                                            } else {
                                                muted()
                                            }))
                                            .child(status.to_string()),
                                    )
                                })
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    match &row {
                                        GitRow::File(group, change, _) => {
                                            view.open_git_change(*group, change.clone(), cx)
                                        }
                                        _ => {
                                            let key = (group_id(group), path.clone());
                                            if !view.git_panel.collapsed.remove(&key) {
                                                view.git_panel.collapsed.insert(key);
                                            }
                                            view.git_panel.rebuild();
                                        }
                                    }
                                    cx.notify();
                                }))
                                .into_any_element(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
        })
        .flex_1()
        .min_h_0();
        let mut explorer = div()
            .id("git-changes-explorer")
            .test_support()
            .w(px(self.settings.ui.files_width.min(230.)))
            .max_w(relative(0.45))
            .min_w(px(130.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(rgb(border()))
            .child(tree);
        if let Some(error) = &self.git_panel.error {
            explorer = explorer.child(
                div()
                    .p_3()
                    .text_xs()
                    .text_color(rgb(muted()))
                    .child(error.clone()),
            );
        } else if self.git_panel.request.is_some() && self.git_panel.rows.is_empty() {
            explorer = explorer.child(
                div()
                    .id("git-status-loading")
                    .test_support()
                    .aria_label("Reading Git status")
                    .py_2()
                    .child(crate::motion::skeleton_list(5)),
            );
        } else if self.git_panel.request.is_some() {
            explorer = explorer.child(loading_caption("Refreshing Git status…"));
        }
        let weak = cx.entity().downgrade();
        let diff = uniform_list(
            "git-diff-lines",
            self.git_panel.diff_lines.len(),
            move |range, _, cx| {
                weak.update(cx, |view, _| {
                    range
                        .filter_map(|index| {
                            let line = view.git_panel.diff_lines.get(index)?;
                            let addition = line.starts_with('+') && !line.starts_with("+++");
                            let deletion = line.starts_with('-') && !line.starts_with("---");
                            Some(
                                div()
                                    .h(px(22.))
                                    .px_2()
                                    .font_family("monospace")
                                    .text_xs()
                                    .whitespace_nowrap()
                                    .bg(rgb(if addition {
                                        diff_added_bg()
                                    } else if deletion {
                                        diff_removed_bg()
                                    } else {
                                        bg()
                                    }))
                                    .text_color(rgb(if addition {
                                        success()
                                    } else if deletion {
                                        danger()
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
            },
        )
        .track_scroll(&self.diff_scroll)
        .w_full()
        .min_w(px(self.git_panel.diff_width))
        .h_full()
        .min_h_0();
        let has_target = self.git_panel.selected.is_some() || self.git_panel.diff_request.is_some();
        let source = div()
            .id("git-diff-pane")
            .test_support()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .when(!has_target, |pane| {
                pane.child(panel_hint("Select a file to see its diff."))
            })
            .when(has_target, |pane| {
                pane.child(
                    div()
                        .h(px(36.))
                        .flex_shrink_0()
                        .min_w_0()
                        .overflow_hidden()
                        .single_line()
                        .px_3()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .text_color(rgb(muted()))
                        .when(self.git_panel.diff_request.is_some(), |header| {
                            header.child(
                                crate::motion::spinner(icons::SMALL).color(rgb(muted()).into()),
                            )
                        })
                        .child(
                            self.git_panel
                                .selected
                                .as_ref()
                                .map(|(path, _)| path.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "Loading diff…".into()),
                        ),
                )
            })
            .when(
                self.git_panel.diff_request.is_some() && self.git_panel.diff_lines.is_empty(),
                |pane| {
                    pane.child(
                        div()
                            .id("git-diff-loading")
                            .test_support()
                            .aria_label("Loading diff")
                            .p_3()
                            .child(crate::motion::skeleton_paragraph(6)),
                    )
                },
            )
            .child(
                div()
                    .id("git-code-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_x_scroll()
                    .child(diff),
            );
        // A clean tree has nothing to list or select: one line says so.
        let clean = self.git_panel.status.is_some()
            && self.git_panel.rows.is_empty()
            && self.git_panel.request.is_none()
            && self.git_panel.error.is_none();
        let body = if clean {
            panel_hint("No uncommitted changes.").into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .child(explorer)
                .child(source)
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .child(
                div()
                    .h(px(40.))
                    .px_3()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(border()))
                    .child(
                        div().flex_1().text_xs().child(
                            self.git_panel
                                .status
                                .as_ref()
                                .map(|status| status.branch.clone())
                                .unwrap_or_else(|| "Git changes".into()),
                        ),
                    )
                    .child(
                        icons::icon_button("refresh-git", AppIcon::Refresh, "Refresh changes")
                            .on_click(cx.listener(|view, _, _, cx| view.refresh_git(cx))),
                    ),
            )
            .child(body)
            .into_any_element()
    }
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

#[cfg(test)]
#[path = "work_panel/tests.rs"]
mod tests;
