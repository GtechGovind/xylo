use super::*;
use gpui_kit::{TestSupportExt, component::IconName};

#[derive(Clone, Debug, PartialEq, Eq)]
enum SidebarRow {
    Project(PathBuf),
    Conversation(usize),
    DraftChat,
    More,
}

/// Flatten only expanded project groups; the virtual list paints visible rows.
fn project_rows(
    conversations: &[Conversation],
    workspace: Option<&Path>,
    open: &[PathBuf],
    collapsed: &BTreeSet<PathBuf>,
    query: &str,
    active_draft: bool,
    more: bool,
) -> Vec<SidebarRow> {
    let query = query.to_lowercase();
    let mut projects: BTreeMap<&Path, Vec<usize>> = open
        .iter()
        .map(|root| (root.as_path(), Vec::new()))
        .collect();
    for (index, chat) in conversations.iter().enumerate() {
        let children = projects.entry(chat.workspace.as_path()).or_default();
        if !collapsed.contains(&chat.workspace)
            && (query.is_empty() || chat.title.to_lowercase().contains(&query))
        {
            children.push(index);
        }
    }
    if let Some(root) = workspace {
        projects.entry(root).or_default();
    }
    let mut rows = Vec::with_capacity(conversations.len() + projects.len());
    for (root, children) in projects {
        rows.push(SidebarRow::Project(root.to_path_buf()));
        if !collapsed.contains(root) {
            if active_draft && query.is_empty() && workspace == Some(root) {
                rows.push(SidebarRow::DraftChat);
            }
            rows.extend(children.into_iter().map(SidebarRow::Conversation));
        }
    }
    if more {
        rows.push(SidebarRow::More);
    }
    rows
}

pub(super) struct SidebarUi {
    rows: std::sync::Arc<Vec<SidebarRow>>,
    pub dirty: bool,
    pub collapsed: BTreeSet<PathBuf>,
}
impl Default for SidebarUi {
    fn default() -> Self {
        Self {
            rows: Default::default(),
            dirty: true,
            collapsed: Default::default(),
        }
    }
}
impl Xylo {
    pub(super) fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mut sidebar = div()
            .w(px(self.settings.ui.sidebar_width))
            .relative()
            .flex_shrink_0()
            .h_full()
            .bg(rgb(crate::ui::sidebar()))
            .border_r_1()
            .border_color(rgb(border()))
            .flex()
            .flex_col()
            .px_2()
            .pb_2()
            .gap_1()
            .child(self.resize_edge(Panel::Sidebar));
        if self.settings_open {
            sidebar = sidebar
                .child(
                    nav_button("back-to-chat", "Workspace", IconName::ArrowLeft)
                        .h(px(40.))
                        .mb_4()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.reset_provider_discovery(cx);
                            this.cancel_account_command(cx);
                            this.settings_open = false;
                            this.page = Page::Chat;
                            this.persist_ui(cx);
                            cx.notify();
                        })),
                )
                .child(sidebar_section("Settings"));
            for (id, page, label, name) in [
                (
                    "nav-general",
                    Page::Appearance,
                    "General",
                    IconName::Settings,
                ),
                ("nav-agent", Page::Agent, "Agent", IconName::Bot),
                (
                    "nav-permissions",
                    Page::Permissions,
                    "Permissions",
                    IconName::Info,
                ),
            ] {
                sidebar = sidebar.child(self.settings_nav(id, page, label, name, cx));
            }
            sidebar = sidebar.child(sidebar_section("Connections").mt_4());
            for (id, page, label, name) in [
                ("nav-providers", Page::Providers, "Providers", IconName::Bot),
                ("nav-skills", Page::Skills, "Skills", IconName::BookOpen),
                ("nav-memory", Page::Memory, "Memory", IconName::BookOpen),
                ("nav-context", Page::Context, "Context", IconName::Folder),
            ] {
                sidebar = sidebar.child(self.settings_nav(id, page, label, name, cx));
            }
            return sidebar.into_any_element();
        }
        sidebar = sidebar
            .child(
                div()
                    .h(px(49.))
                    .px_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .id("xylo-brand-mark")
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(15.))
                            .child("Xylo")
                            .test_support(),
                    )
                    .child(
                        icon_button("search-chats", "Search chats", IconName::Search).on_click(
                            cx.listener(|this, _, window, cx| {
                                this.search_open = true;
                                this.page = Page::HistorySearch;
                                this.conversation_query
                                    .update(cx, |s, cx| s.focus(window, cx));
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .child(
                div()
                    .h(px(36.))
                    .px_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(10.)).text_color(rgb(muted())).child("Projects"))
                    .child(
                        icon_button("open-project", "Add project", IconName::Plus).on_click(
                            cx.listener(|view, _, window, cx| view.pick_workspace(window, cx)),
                        ),
                    ),
            );
        if self.search_open {
            sidebar = sidebar
                .child(Input::new(&self.conversation_query).aria_label("Search conversations"));
        }
        if self.sidebar_state.dirty {
            self.sidebar_state.dirty = false;
            self.sidebar_state.rows = std::sync::Arc::new(project_rows(
                &self.conversations,
                self.workspace.as_deref(),
                &self.settings.ui.open_projects,
                &self.sidebar_state.collapsed,
                self.conversation_query.read(cx).value().as_ref(),
                self.messages.is_empty()
                    && !self
                        .conversations
                        .iter()
                        .any(|chat| chat.id == self.session),
                self.conversations_has_more || self.conversations_loading_more,
            ));
        }
        let rows = self.sidebar_state.rows.clone();
        let weak = cx.entity().downgrade();
        let content = uniform_list("conversation-list-rows", rows.len(), move |range, _, cx| {
            weak.update(cx, |view, cx| {
                range
                    .filter_map(|i| match rows.get(i)? {
                        SidebarRow::Project(path) => {
                            let path = path.clone();
                            let new_path = path.clone();
                            let expanded = !view.sidebar_state.collapsed.contains(&path);
                            let label = path
                                .file_name()
                                .unwrap_or(path.as_os_str())
                                .to_string_lossy()
                                .into_owned();
                            Some(
                                div()
                                    .h(px(34.))
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        nav_button(
                                            ("project", i),
                                            "",
                                            if expanded {
                                                IconName::ChevronDown
                                            } else {
                                                IconName::ChevronRight
                                            },
                                        )
                                        .flex_1()
                                        .min_w_0()
                                        .gap_2()
                                        .text_size(px(11.))
                                        .text_color(rgb(muted()))
                                        .child(icon(IconName::Folder))
                                        .child(div().flex_1().min_w_0().truncate().child(label))
                                        .accessibility_label(format!(
                                            "{} project {}",
                                            if expanded { "Collapse" } else { "Expand" },
                                            path.display()
                                        ))
                                        .on_click(
                                            cx.listener(move |view, _, _, cx| {
                                                if !view.sidebar_state.collapsed.remove(&path) {
                                                    view.sidebar_state
                                                        .collapsed
                                                        .insert(path.clone());
                                                }
                                                view.settings.ui.collapsed_projects = view
                                                    .sidebar_state
                                                    .collapsed
                                                    .iter()
                                                    .cloned()
                                                    .collect();
                                                view.sidebar_state.dirty = true;
                                                view.persist_ui(cx);
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                    .child(
                                        icon_button(
                                            ("project-new-chat", i),
                                            format!("New chat in {}", new_path.display()),
                                            IconName::Plus,
                                        )
                                        .on_click(
                                            cx.listener(move |view, _, window, cx| {
                                                view.sidebar_state.collapsed.remove(&new_path);
                                                view.settings.ui.collapsed_projects = view
                                                    .sidebar_state
                                                    .collapsed
                                                    .iter()
                                                    .cloned()
                                                    .collect();
                                                view.sidebar_state.dirty = true;
                                                if view.workspace.as_ref() == Some(&new_path) {
                                                    view.new_chat(cx);
                                                } else {
                                                    view.set_workspace(
                                                        new_path.clone(),
                                                        window,
                                                        cx,
                                                    );
                                                }
                                                view.persist_ui(cx);
                                            }),
                                        ),
                                    )
                                    .into_any_element(),
                            )
                        }
                        SidebarRow::DraftChat => Some(
                            div()
                                .h(px(34.))
                                .pl(px(15.))
                                .border_l_1()
                                .border_color(rgb(border()))
                                .child(
                                    button("unsaved-chat", "New chat")
                                        .w_full()
                                        .h(px(34.))
                                        .text_size(px(11.))
                                        .bg(rgb(selected()))
                                        .text_color(rgb(text()))
                                        .justify_start()
                                        .on_click(cx.listener(|view, _, _, cx| {
                                            view.page = Page::Chat;
                                            view.settings_open = false;
                                            cx.notify();
                                        })),
                                )
                                .into_any_element(),
                        ),
                        SidebarRow::Conversation(index) => {
                            let conversation = view.conversations.get(*index)?;
                            let id = conversation.id;
                            let path = conversation.workspace.clone();
                            let running = view.turns.get(&id).is_some_and(|turn| turn.running);
                            let title = conversation.title.clone();
                            Some(
                                div()
                                    .h(px(34.))
                                    .pl(px(15.))
                                    .border_l_1()
                                    .border_color(rgb(border()))
                                    .child(
                                        focus_ring(gpui_kit::base::Button::new((
                                            "conversation",
                                            i,
                                        )))
                                        .accessibility_label(if running { format!("{}, running", title) } else { title.clone() })
                                        .h(px(34.))
                                        .w_full()
                                        .px(px(9.))
                                        .rounded_md()
                                        .text_size(px(11.))
                                        .text_color(rgb(if view.session == id {
                                            text()
                                        } else {
                                            muted()
                                        }))
                                        .bg(rgb(if view.session == id {
                                            selected()
                                        } else {
                                            crate::ui::sidebar()
                                        }))
                                        .hover(|style| style.bg(rgb(hover())))
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .justify_start()
                                        .min_w_0()
                                        .when(running, |button| button.child(div().size(px(5.)).rounded_full().bg(rgb(accent()))))
                                        .child(div().flex_1().min_w_0().truncate().child(title))
                                        .on_click(
                                            cx.listener(move |view, _, window, cx| {
                                                view.open_conversation(id, path.clone(), window, cx)
                                            }),
                                        ),
                                    )
                                    .into_any_element(),
                            )
                        }
                        SidebarRow::More => Some(
                            nav_button(
                                "more-conversations",
                                if view.conversations_loading_more {
                                    "Loading…"
                                } else {
                                    "Load more chats"
                                },
                                IconName::Plus,
                            )
                            .disabled(view.conversations_loading_more)
                            .on_click(cx.listener(|view, _, _, cx| view.more_conversations(cx)))
                            .into_any_element(),
                        ),
                    })
                    .collect()
            })
            .unwrap_or_default()
        })
        .track_scroll(&self.conversation_scroll)
        .flex_1()
        .min_h_0();
        sidebar
            .child(content)
            .child(
                nav_button("settings", "Settings", IconName::Settings).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.settings_open = true;
                        this.navigate(Page::Appearance, cx);
                    },
                )),
            )
            .into_any_element()
    }
    fn settings_nav(
        &self,
        id: &'static str,
        page: Page,
        label: &'static str,
        name: IconName,
        cx: &Context<Self>,
    ) -> gpui_kit::base::Button {
        nav_button(id, label, name)
            .bg(rgb(
                if self.page == page
                    || (page == Page::Context && self.page == Page::Connectors)
                    || (page == Page::Providers && self.page == Page::Images)
                {
                    selected()
                } else {
                    crate::ui::sidebar()
                },
            ))
            .on_click(cx.listener(move |this, _, _, cx| this.navigate(page, cx)))
    }
}
fn sidebar_section(label: &'static str) -> Div {
    div()
        .px_2()
        .h(px(30.))
        .flex()
        .items_center()
        .text_size(px(10.))
        .text_color(rgb(muted()))
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::{SidebarRow, project_rows};
    use std::{collections::BTreeSet, path::Path};
    use uuid::Uuid;
    use xylo_core::Conversation;
    fn chat(root: &str, title: &str) -> Conversation {
        Conversation {
            id: Uuid::new_v4(),
            title: title.into(),
            workspace: root.into(),
            updated_at: 0,
        }
    }
    #[test]
    fn nests_chats_under_distinct_project_roots() {
        let chats = vec![
            chat("/a/app", "A"),
            chat("/b/app", "B"),
            chat("/a/app", "C"),
        ];
        assert_eq!(
            project_rows(&chats, None, &[], &BTreeSet::new(), "", false, false),
            vec![
                SidebarRow::Project("/a/app".into()),
                SidebarRow::Conversation(0),
                SidebarRow::Conversation(2),
                SidebarRow::Project("/b/app".into()),
                SidebarRow::Conversation(1)
            ]
        );
    }
    #[test]
    fn collapse_keeps_empty_open_projects_and_pagination() {
        assert_eq!(
            project_rows(
                &[chat("/a", "A")],
                Some(Path::new("/b")),
                &["/c".into()],
                &BTreeSet::from(["/a".into()]),
                "",
                false,
                true
            ),
            vec![
                SidebarRow::Project("/a".into()),
                SidebarRow::Project("/b".into()),
                SidebarRow::Project("/c".into()),
                SidebarRow::More
            ]
        );
    }
    #[test]
    fn query_keeps_results_inside_their_project() {
        assert_eq!(
            project_rows(
                &[chat("/a", "Build UI"), chat("/b", "Fix tests")],
                None,
                &[],
                &BTreeSet::new(),
                "BUILD",
                false,
                false
            ),
            vec![
                SidebarRow::Project("/a".into()),
                SidebarRow::Conversation(0),
                SidebarRow::Project("/b".into())
            ]
        );
    }
    #[test]
    fn unsaved_chat_is_nested_and_collapses_with_its_project() {
        let root = Path::new("/project");
        assert_eq!(
            project_rows(&[], Some(root), &[], &BTreeSet::new(), "", true, false),
            vec![SidebarRow::Project(root.into()), SidebarRow::DraftChat]
        );
        assert_eq!(
            project_rows(
                &[],
                Some(root),
                &[],
                &BTreeSet::from([root.into()]),
                "",
                true,
                false
            ),
            vec![SidebarRow::Project(root.into())]
        );
    }
}
