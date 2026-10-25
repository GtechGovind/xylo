use super::*;
use gpui_kit::{TestSupportExt, component::IconName};
#[derive(Clone)]
enum SidebarRow {
    Header(&'static str),
    Team,
    Project(PathBuf),
    OpenProject,
    Conversation(usize),
    More,
}
pub(super) struct SidebarUi {
    rows: std::sync::Arc<Vec<SidebarRow>>,
    pub dirty: bool,
}
impl Default for SidebarUi {
    fn default() -> Self {
        Self {
            rows: Default::default(),
            dirty: true,
        }
    }
}
impl Workbench {
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
        sidebar = sidebar.child(
            div()
                .h(px(64.))
                .px_2()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .id("xylo-brand-mark")
                        .size(px(31.))
                        .rounded_lg()
                        .bg(rgb(0xffffff))
                        .overflow_hidden()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            img(crate::ui::brand_image())
                                .size(px(31.))
                                .object_fit(ObjectFit::Contain),
                        )
                        .test_support(),
                )
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Xylo"),
                ),
        );
        if self.settings_open {
            for (i, page, label, name) in [
                (6, Page::Providers, "Model", IconName::Bot),
                (5, Page::Connectors, "Tools", IconName::Folder),
                (14, Page::Plugins, "Plugins", IconName::Folder),
                (15, Page::Environments, "Environments", IconName::Folder),
                (4, Page::Memory, "Memory", IconName::BookOpen),
                (7, Page::Diagnostics, "Activity", IconName::Info),
                (8, Page::Appearance, "Appearance", IconName::Settings),
                (9, Page::Assets, "Downloads", IconName::Folder),
                (10, Page::Voice, "Voice", IconName::Info),
                (13, Page::Images, "Images", IconName::Folder),
                (11, Page::Updates, "Updates", IconName::Info),
                (12, Page::Notifications, "Notifications", IconName::Info),
            ] {
                sidebar = sidebar.child(
                    nav_button(SharedString::from(format!("nav-{i}")), label, name)
                        .bg(rgb(if self.page == page {
                            selected()
                        } else {
                            crate::ui::sidebar()
                        }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if page == Page::Assets {
                                this.open_assets(0, cx);
                            } else {
                                this.navigate(page, cx);
                            }
                        })),
                );
            }
            return sidebar
                .child(div().flex_1())
                .child(
                    nav_button("back-to-chat", "Back to Chat", IconName::ArrowLeft).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.settings_open = false;
                            this.page = Page::Chat;
                            this.persist_ui(cx);
                            cx.notify();
                        }),
                    ),
                )
                .into_any_element();
        }
        sidebar = sidebar
            .child(
                nav_button("new-chat", "New Chat", IconName::Plus).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.new_chat(cx);
                        this.persist_ui(cx);
                    },
                )),
            )
            .child(
                nav_button("search-chats", "Search", IconName::Search).on_click(cx.listener(
                    |this, _, window, cx| {
                        this.search_open = !this.search_open;
                        if this.search_open {
                            this.page = Page::HistorySearch;
                            this.conversation_query
                                .update(cx, |s, cx| s.focus(window, cx));
                        }
                        cx.notify();
                    },
                )),
            )
            .child(
                nav_button("scheduled-tasks", "Scheduled Tasks", IconName::Calendar).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.navigate(Page::Schedules, cx);
                        this.host_call(cx, |h| h.list_schedules());
                    }),
                ),
            )
            .child(div().h(px(1.)).my_2().mx_2().bg(rgb(border())));
        if self.search_open {
            sidebar = sidebar
                .child(Input::new(&self.conversation_query).aria_label("Search conversations"));
        }
        if self.sidebar_state.dirty {
            self.sidebar_state.dirty = false;
            let mut rows = vec![
                SidebarRow::Header("Teams"),
                SidebarRow::Team,
                SidebarRow::Header("Projects"),
            ];
            let mut projects: BTreeSet<_> = self
                .conversations
                .iter()
                .map(|conversation| conversation.workspace.clone())
                .collect();
            if let Some(path) = &self.workspace {
                projects.insert(path.clone());
            }
            rows.extend(projects.into_iter().map(SidebarRow::Project));
            rows.push(SidebarRow::OpenProject);
            rows.push(SidebarRow::Header("Conversations"));
            let query = self.conversation_query.read(cx).value().to_lowercase();
            rows.extend(
                self.conversations
                    .iter()
                    .enumerate()
                    .filter(|(_, conversation)| {
                        query.is_empty() || conversation.title.to_lowercase().contains(&query)
                    })
                    .map(|(index, _)| SidebarRow::Conversation(index)),
            );
            if self.conversations_has_more || self.conversations_loading_more {
                rows.push(SidebarRow::More);
            }
            self.sidebar_state.rows = std::sync::Arc::new(rows);
        }
        let rows = self.sidebar_state.rows.clone();
        let weak = cx.entity().downgrade();
        let content = uniform_list("conversation-list-rows", rows.len(), move |range, _, cx| {
            weak.update(cx, |view, cx| {
                range
                    .filter_map(|i| match rows.get(i)? {
                        SidebarRow::Header(label) => Some(
                            div()
                                .h(px(34.))
                                .px_3()
                                .flex()
                                .items_center()
                                .text_sm()
                                .text_color(rgb(muted()))
                                .child(*label)
                                .into_any_element(),
                        ),
                        SidebarRow::Team => Some(
                            nav_button("team-tasks", "Team tasks", IconName::CircleUser)
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.navigate(Page::Teams, cx);
                                    view.host_call(cx, |host| host.team_tasks());
                                }))
                                .into_any_element(),
                        ),
                        SidebarRow::OpenProject => Some(
                            nav_button("open-project", "Open project", IconName::Plus)
                                .text_color(rgb(muted()))
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.pick_workspace(window, cx)
                                }))
                                .into_any_element(),
                        ),
                        SidebarRow::Project(path) => {
                            let path = path.clone();
                            let label = path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned();
                            Some(
                                nav_button(("project", i), label, IconName::Folder)
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        view.set_workspace(path.clone(), window, cx)
                                    }))
                                    .into_any_element(),
                            )
                        }
                        SidebarRow::Conversation(index) => {
                            let conversation = view.conversations.get(*index)?;
                            let id = conversation.id;
                            let path = conversation.workspace.clone();
                            let title = if view.turns.get(&id).is_some_and(|turn| turn.running) {
                                format!("● {}", conversation.title)
                            } else {
                                conversation.title.clone()
                            };
                            Some(
                                nav_button(("conversation", i), title, IconName::Bot)
                                    .bg(rgb(if view.session == id {
                                        selected()
                                    } else {
                                        crate::ui::sidebar()
                                    }))
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        view.open_conversation(id, path.clone(), window, cx)
                                    }))
                                    .into_any_element(),
                            )
                        }
                        SidebarRow::More => Some(
                            nav_button(
                                "more-conversations",
                                if view.conversations_loading_more {
                                    "Loading…"
                                } else {
                                    "Load more conversations"
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
            .child(div().h(px(1.)).mx_2().bg(rgb(border())))
            .child(
                nav_button("settings", "Settings", IconName::Settings).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.settings_open = true;
                        this.navigate(Page::Providers, cx);
                    },
                )),
            )
            .into_any_element()
    }
}
