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
            .px(px(10.))
            .pb(px(10.))
            .child(self.resize_edge(Panel::Sidebar));
        if self.settings_open {
            sidebar = sidebar
                .child(div().h(px(52.)).flex().items_center().child(
                    nav_button("back-to-chat", "Back to workspace", IconName::ArrowLeft).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.reset_provider_discovery(cx);
                            this.cancel_account_command(cx);
                            this.settings_open = false;
                            this.page = Page::Chat;
                            this.persist_ui(cx);
                            cx.notify();
                        }),
                    ),
                ))
                .child(
                    div()
                        .px(px(10.))
                        .pt(px(6.))
                        .pb(px(14.))
                        .child(title("Settings").text_size(px(20.))),
                )
                .child(sidebar_label("General"));
            // Keep every destination reachable in short windows.
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
            sidebar = sidebar
                .child(sidebar_label("Models & accounts").mt_3())
                .child(self.settings_nav(
                    "nav-providers",
                    Page::Providers,
                    "Providers",
                    IconName::Bot,
                    cx,
                ))
                .child(sidebar_label("Tools & integrations").mt_3());
            for (id, page, label, name) in [
                (
                    "nav-connections",
                    Page::Connectors,
                    "Connections",
                    IconName::Folder,
                ),
                ("nav-plugins", Page::Plugins, "Plugins", IconName::BookOpen),
                (
                    "nav-environments",
                    Page::Environments,
                    "Environments",
                    IconName::Folder,
                ),
            ] {
                sidebar = sidebar.child(self.settings_nav(id, page, label, name, cx));
            }
            sidebar = sidebar.child(sidebar_label("Knowledge & context").mt_3());
            for (id, page, label, name) in [
                ("nav-skills", Page::Skills, "Skills", IconName::BookOpen),
                ("nav-memory", Page::Memory, "Memory", IconName::BookOpen),
                ("nav-context", Page::Context, "Context", IconName::Folder),
            ] {
                sidebar = sidebar.child(self.settings_nav(id, page, label, name, cx));
            }
            return sidebar
                .id("settings-navigation")
                .overflow_y_scroll()
                .into_any_element();
        }
        // Brand row: mark, wordmark and the collapse control.
        sidebar = sidebar
            .child(
                div()
                    .h(px(52.))
                    .flex_shrink_0()
                    .px(px(6.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .id("xylo-brand-mark")
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(brand_mark(22.))
                            .child(
                                div()
                                    .text_size(px(17.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(text()))
                                    .child("Xylo"),
                            )
                            .test_support(),
                    )
                    .child(
                        icon_button("collapse-sidebar", "Hide sidebar", IconName::PanelLeft)
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.settings.ui.sidebar_visible = false;
                                view.persist_ui(cx);
                                cx.notify();
                            })),
                    ),
            )
            // Primary actions.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .pb(px(6.))
                    .child(
                        nav_button("sidebar-new-chat", "New chat", IconName::Plus)
                            .text_color(rgb(text()))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(11.))
                                    .text_color(rgb(faint()))
                                    .child(if cfg!(target_os = "macos") {
                                        "⌘N"
                                    } else {
                                        "Ctrl N"
                                    }),
                            )
                            .on_click(cx.listener(|view, _, _, cx| view.new_chat(cx))),
                    )
                    .child(
                        nav_button("search-chats", "Search chats", IconName::Search)
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(11.))
                                    .text_color(rgb(faint()))
                                    .child(if cfg!(target_os = "macos") {
                                        "⌘K"
                                    } else {
                                        "Ctrl K"
                                    }),
                            )
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.search_open = true;
                                this.page = Page::HistorySearch;
                                this.conversation_query
                                    .update(cx, |s, cx| s.focus(window, cx));
                                cx.notify();
                            })),
                    )
                    .child(
                        nav_button("open-project", "Open project", Glyph::FolderOpen).on_click(
                            cx.listener(|view, _, window, cx| view.pick_workspace(window, cx)),
                        ),
                    ),
            )
            .child(
                div()
                    .h(px(30.))
                    .mt(px(8.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(sidebar_label("Projects"))
                    .child(
                        icon_button("add-project", "Add project", IconName::Plus)
                            .size(px(24.))
                            .on_click(
                                cx.listener(|view, _, window, cx| view.pick_workspace(window, cx)),
                            ),
                    ),
            );
        if self.search_open {
            sidebar =
                sidebar.child(div().px(px(2.)).pb(px(6.)).child(
                    Input::new(&self.conversation_query).aria_label("Search conversations"),
                ));
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
                            let current = view.workspace.as_ref() == Some(&path);
                            let label = path
                                .file_name()
                                .unwrap_or(path.as_os_str())
                                .to_string_lossy()
                                .into_owned();
                            Some(
                                div()
                                    .w_full()
                                    .min_w_0()
                                    .h(px(32.))
                                    .group("sidebar-project")
                                    .flex()
                                    .items_center()
                                    .gap(px(2.))
                                    .child(
                                        focus_ring(gpui_kit::base::Button::new(("project", i)))
                                            .w(px(0.))
                                            .flex_1()
                                            .min_w_0()
                                            .h(px(30.))
                                            .px(px(8.))
                                            .rounded(px(8.))
                                            .flex()
                                            .items_center()
                                            .justify_start()
                                            .gap(px(6.))
                                            .text_size(px(13.))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(rgb(if current {
                                                text()
                                            } else {
                                                text_secondary()
                                            }))
                                            .cursor_pointer()
                                            .hover(|style| {
                                                style.bg(rgb(hover())).text_color(rgb(text()))
                                            })
                                            .child(
                                                icon(if expanded {
                                                    IconName::ChevronDown
                                                } else {
                                                    IconName::ChevronRight
                                                })
                                                .size(px(13.))
                                                .text_color(rgb(faint())),
                                            )
                                            .child(icon(IconName::Folder).size(px(15.)).text_color(
                                                rgb(if current { accent_text() } else { muted() }),
                                            ))
                                            .child(div().flex_1().min_w_0().truncate().child(label))
                                            .accessibility_label(format!(
                                                "{} project {}",
                                                if expanded { "Collapse" } else { "Expand" },
                                                path.display()
                                            ))
                                            .on_click(cx.listener(move |view, _, _, cx| {
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
                                            })),
                                    )
                                    .child(
                                        icon_button(
                                            ("project-new-chat", i),
                                            format!("New chat in {}", new_path.display()),
                                            Glyph::SquarePen,
                                        )
                                        .size(px(26.))
                                        .opacity(0.)
                                        .group_hover("sidebar-project", |style| style.opacity(1.))
                                        .focus(|style| style.opacity(1.))
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
                            sidebar_child_row(
                                sidebar_chat_button("unsaved-chat", "New chat".into(), true, false)
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
                            Some(
                                sidebar_child_row(
                                    sidebar_chat_button(
                                        ("conversation", i),
                                        conversation.title.clone(),
                                        view.session == id,
                                        running,
                                    )
                                    .on_click(cx.listener(
                                        move |view, _, window, cx| {
                                            view.open_conversation(id, path.clone(), window, cx)
                                        },
                                    )),
                                )
                                .into_any_element(),
                            )
                        }
                        SidebarRow::More => Some(
                            div()
                                .h(px(32.))
                                .pl(px(26.))
                                .flex()
                                .items_center()
                                .child(
                                    button(
                                        "more-conversations",
                                        if view.conversations_loading_more {
                                            "Loading…"
                                        } else {
                                            "Load more"
                                        },
                                    )
                                    .h(px(28.))
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::NORMAL)
                                    .text_color(rgb(muted()))
                                    .disabled(view.conversations_loading_more)
                                    .on_click(
                                        cx.listener(|view, _, _, cx| view.more_conversations(cx)),
                                    ),
                                )
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
        let person = person_name();
        let initial = person
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "X".into());
        sidebar
            .child(content)
            .child(divider().my(px(8.)))
            .child(
                focus_ring(gpui_kit::base::Button::new("settings"))
                    .accessibility_label("Settings")
                    .w_full()
                    .h(px(44.))
                    .px(px(8.))
                    .rounded(px(10.))
                    .flex()
                    .items_center()
                    .justify_start()
                    .gap(px(10.))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(hover())))
                    .child(avatar(initial, 28.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .items_start()
                            .gap(px(1.))
                            .child(
                                div()
                                    .w_full()
                                    .truncate()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(rgb(text()))
                                    .child(person),
                            )
                            .child(div().text_size(px(11.5)).text_color(rgb(muted())).child(
                                if self.settings.provider.model.is_empty() {
                                    "No model selected".to_owned()
                                } else {
                                    self.settings.provider.model.clone()
                                },
                            )),
                    )
                    .child(
                        icon(IconName::Settings)
                            .size(px(15.))
                            .text_color(rgb(muted())),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.settings_open = true;
                        this.navigate(Page::Appearance, cx);
                    })),
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
        let active = self.page == page || (page == Page::Providers && self.page == Page::Images);
        nav_item(id, label, name, active)
            .on_click(cx.listener(move |this, _, _, cx| this.navigate(page, cx)))
    }
}
/// The person's name as the operating system knows it, for the account row.
pub(super) fn person_name() -> String {
    let raw = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        return "You".into();
    }
    let mut chars = raw.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => "You".into(),
    }
}
/// All virtual rows share a 32px stride; chat rows are inset under their project.
fn sidebar_child_row(button: gpui_kit::base::Button) -> Div {
    div()
        .w_full()
        .min_w_0()
        .h(px(32.))
        .pl(px(22.))
        .flex()
        .items_center()
        .child(button)
}
fn sidebar_chat_button(
    id: impl Into<ElementId>,
    title: String,
    active: bool,
    running: bool,
) -> gpui_kit::base::Button {
    recent_item(id, title.clone(), active)
        .accessibility_label(if running {
            format!("{title}, running")
        } else {
            title
        })
        .h(px(30.))
        .when(running, |button| {
            button.child(
                div()
                    .size(px(6.))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(rgb(brand())),
            )
        })
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
