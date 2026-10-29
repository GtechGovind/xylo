//! Conversation/work-panel composition and virtualized Git folder tree.
use super::*;
use gpui_kit::component::IconName;
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
    pub(super) fn conversation_actions(&self, cx: &Context<Self>) -> AnyElement {
        use gpui_kit::component::{
            button::{Button as MenuButton, ButtonVariants},
            menu::{DropdownMenu, PopupMenuItem},
        };
        let weak = cx.entity().downgrade();
        MenuButton::new("conversation-actions")
            .ghost()
            .icon(IconName::Ellipsis)
            .accessibility_label("Conversation actions")
            .dropdown_menu(move |menu, _, _| {
                let view = weak.clone();
                let menu = menu.item(PopupMenuItem::new("New chat").on_click(move |_, _, cx| {
                    let _ = view.update(cx, |view, cx| view.new_chat(cx));
                }));
                let view = weak.clone();
                menu.item(
                    PopupMenuItem::new("Copy conversation").on_click(move |_, _, cx| {
                        if let Ok(text) = view.update(cx, |view, _| {
                            view.messages
                                .iter()
                                .map(|message| format!("{}\n{}", message.role, message.content))
                                .collect::<Vec<_>>()
                                .join("\n\n")
                        }) {
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                        }
                    }),
                )
            })
            .into_any_element()
    }
    pub(super) fn refresh_git(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.workspace.clone() else {
            return;
        };
        let request = Uuid::new_v4();
        self.git_panel.request = Some(request);
        self.git_panel.root = Some(root.clone());
        self.git_panel.error = None;
        self.git_panel.diff_request = None;
        self.git_panel.selected = None;
        self.diff.clear();
        self.diff_lines.clear();
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
                            view.git_panel = panel;
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
                self.diff_lines = diff.lines().map(str::to_owned).collect();
                self.git_panel.diff_width = self
                    .diff_lines
                    .iter()
                    .map(|line| line.chars().count())
                    .max()
                    .unwrap_or(0) as f32
                    * 7.5
                    + 20.;
                self.diff = diff;
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
        self.diff.clear();
        self.diff_lines.clear();
        self.host_call(cx, |host| {
            host.git_file_diff(request, root, change.path, change.original, group)
        });
    }
    pub(super) fn activate_work_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.settings.ui.work_panel.as_str() {
            "changes" => self.refresh_git(cx),
            "terminal" => {
                if !self.terminal_visible {
                    self.toggle_terminal(window, cx);
                }
            }
            _ => {
                self.refresh_files(cx);
                if self.git_panel.status.is_none() {
                    self.refresh_git(cx);
                }
            }
        }
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
        let selected = if self.page == Page::Files {
            "files"
        } else if self.page == Page::Changes {
            "changes"
        } else {
            &self.settings.ui.work_panel
        };
        let mut tabs = div()
            .h(px(40.))
            .flex_shrink_0()
            .px_2()
            .flex()
            .items_center()
            .gap_1()
            .border_b_1()
            .border_color(rgb(border()));
        for (key, label) in [
            ("changes", "Changes"),
            ("files", "Files"),
            ("terminal", "Terminal"),
        ] {
            tabs = tabs.child(
                button(SharedString::from(format!("work-tab-{key}")), label)
                    .text_size(px(10.))
                    .h_full()
                    .rounded_none()
                    .border_0()
                    .border_b_2()
                    .border_color(rgb(if key == selected { accent() } else { panel() }))
                    .text_color(rgb(if key == selected { text() } else { muted() }))
                    .bg(rgb(panel()))
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
        let content = match selected {
            "changes" => self.git_changes_panel(cx),
            "terminal" => self
                .terminal
                .as_ref()
                .map(|terminal| terminal.clone().into_any_element())
                .unwrap_or_else(|| {
                    div()
                        .p_4()
                        .child(subtitle(if self.terminal_starting {
                            "Starting shell…"
                        } else {
                            "Open a project to start its shell."
                        }))
                        .into_any_element()
                }),
            _ => self.files_page(cx),
        };
        let _ = window;
        div()
            .size_full()
            .min_h_0()
            .flex()
            .child(div().flex_1().min_w_0().h_full().child(chat))
            .child(
                div()
                    .w(px(self.settings.ui.work_panel_width))
                    .max_w(relative(0.62))
                    .min_w(px(320.))
                    .relative()
                    .child(self.resize_edge(Panel::Work))
                    .h_full()
                    .flex()
                    .flex_col()
                    .border_l_1()
                    .border_color(rgb(border()))
                    .child(tabs)
                    .child(div().flex_1().min_h_0().child(content)),
            )
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
                                .child(icon(if folder {
                                    if collapsed {
                                        IconName::ChevronRight
                                    } else {
                                        IconName::ChevronDown
                                    }
                                } else {
                                    IconName::FileText
                                }))
                                .child(div().flex_1().min_w_0().truncate().child(label))
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
            .w(px(self.settings.ui.files_width.min(230.)))
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
        } else if self.git_panel.request.is_some() {
            explorer = explorer.child(div().p_3().child(subtitle("Reading Git status…")));
        } else if self.git_panel.rows.is_empty() {
            explorer = explorer.child(div().p_3().child(subtitle("No changes")));
        }
        let weak = cx.entity().downgrade();
        let diff = uniform_list(
            "git-diff-lines",
            self.diff_lines.len(),
            move |range, _, cx| {
                weak.update(cx, |view, _| {
                    range
                        .filter_map(|index| {
                            let line = view.diff_lines.get(index)?;
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
        let source = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(36.))
                    .px_3()
                    .flex()
                    .items_center()
                    .text_xs()
                    .text_color(rgb(muted()))
                    .child(
                        self.git_panel
                            .selected
                            .as_ref()
                            .map(|(path, _)| path.to_string_lossy().into_owned())
                            .unwrap_or_else(|| {
                                if self.git_panel.diff_request.is_some() {
                                    "Loading diff…".into()
                                } else {
                                    "Select a changed file".into()
                                }
                            }),
                    ),
            )
            .child(
                div()
                    .id("git-code-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_x_scroll()
                    .child(diff),
            );
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
                        icon_button("refresh-git", "Refresh changes", IconName::RefreshCw)
                            .on_click(cx.listener(|view, _, _, cx| view.refresh_git(cx))),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(explorer)
                    .child(source),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{ChangeGroup, GitChange, GitPanel, GitRow, GitStatus, group_id};
    use std::{collections::BTreeSet, path::PathBuf};
    #[test]
    fn changes_share_folder_structure_and_staged_unstaged_overlap() {
        let root = PathBuf::from("/project");
        let status = GitStatus {
            branch: "main".into(),
            truncated: false,
            changes: vec![
                GitChange {
                    path: "src/nested/app.rs".into(),
                    original: None,
                    index: 'M',
                    worktree: 'M',
                },
                GitChange {
                    path: "new.txt".into(),
                    original: None,
                    index: '?',
                    worktree: '?',
                },
            ],
        };
        let mut panel = GitPanel::prepare(root.clone(), status, BTreeSet::new());
        assert_eq!(panel.trees.len(), 3);
        assert_eq!(panel.statuses[&root.join("src/nested/app.rs")], 'M');
        assert_eq!(panel.rows.iter().filter(|row| matches!(row, GitRow::File(_,change,2) if change.path == PathBuf::from("src/nested/app.rs"))).count(),2);
        panel
            .collapsed
            .insert((group_id(ChangeGroup::Staged), "src".into()));
        panel.rebuild();
        assert!(
            !panel
                .rows
                .iter()
                .any(|row| matches!(row, GitRow::File(ChangeGroup::Staged, _, _)))
        );
        assert!(
            panel
                .rows
                .iter()
                .any(|row| matches!(row, GitRow::File(ChangeGroup::Unstaged, _, _)))
        );
        assert_eq!(panel.trees.len(), 3, "collapse preserves prepared data");
    }
}
