//! The workspace sidebar: New chat, Search chats (the Cmd-K palette), Saved
//! drafts and Scheduled; then projects with their chats,
//! each chat with a context menu that the hover "…" also opens; and the
//! account row that opens settings. Rows show generated titles;
//! prompt-derived titles that are only a greeting read "New chat" until a
//! real title arrives. Renaming happens in place, in the row or, when started
//! from the chat's "…" menu, in the toolbar title.
use super::*;
use crate::icons::{self, AppIcon};
use crate::motion::{self, Disclosure};
use gpui_kit::TestSupportExt;
use gpui_kit::component::menu::{ContextMenuExt, DropdownMenu, PopupMenu, PopupMenuItem};
use std::time::Instant;

/// Words that make a first prompt a greeting rather than a topic.
const GREETINGS: &[&str] = &[
    "hi",
    "hello",
    "hey",
    "heya",
    "hiya",
    "yo",
    "howdy",
    "hola",
    "greetings",
    "sup",
];
/// Words that may accompany a greeting without giving it a topic.
const GREETING_FILLERS: &[&str] = &[
    "good",
    "morning",
    "afternoon",
    "evening",
    "day",
    "there",
    "all",
    "everyone",
    "xylo",
];

/// Whether a prompt is only a salutation ("hi", "Hey there!", "good morning").
fn is_greeting(text: &str) -> bool {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|word| {
            word.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect();
    !words.is_empty()
        && words.len() <= 4
        && words.iter().all(|word| {
            GREETINGS.contains(&word.as_str()) || GREETING_FILLERS.contains(&word.as_str())
        })
        && words.iter().any(|word| {
            GREETINGS.contains(&word.as_str())
                || matches!(word.as_str(), "morning" | "afternoon" | "evening")
        })
}

/// The title a conversation is shown with.
pub(super) fn display_title(conversation: &Conversation) -> String {
    let title = conversation.title.trim();
    if title.is_empty() || (conversation.title_source == TitleSource::Prompt && is_greeting(title))
    {
        "New chat".into()
    } else {
        title.to_owned()
    }
}

/// A chat title being edited in place.
struct Rename {
    id: Uuid,
    input: Entity<InputState>,
    /// Edited in the toolbar title rather than in the sidebar row.
    in_header: bool,
    _subscription: Subscription,
}

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
    active_draft: bool,
    more: bool,
) -> Vec<SidebarRow> {
    let mut projects: BTreeMap<&Path, Vec<usize>> = open
        .iter()
        .map(|root| (root.as_path(), Vec::new()))
        .collect();
    for (index, chat) in conversations.iter().enumerate() {
        let children = projects.entry(chat.workspace.as_path()).or_default();
        if !collapsed.contains(&chat.workspace) {
            children.push(index);
        }
    }
    if let Some(root) = workspace {
        projects.entry(root).or_default();
    }
    // Pinned chats come first in their project; each group keeps list order.
    for children in projects.values_mut() {
        children.sort_by_key(|index| !conversations[*index].pinned);
    }
    let mut rows = Vec::with_capacity(conversations.len() + projects.len());
    for (root, children) in projects {
        rows.push(SidebarRow::Project(root.to_path_buf()));
        if !collapsed.contains(root) {
            if active_draft && workspace == Some(root) {
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

/// A sidebar change that animates for a moment after it happens.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Change {
    /// A project was expanded; its chats fade in under it.
    Expanded(PathBuf),
    /// A chat appeared after the first list (a new chat); it slides in.
    Arrived(Uuid),
    /// A chat's title changed; it cross-fades.
    Retitled(Uuid),
}

pub(super) struct SidebarUi {
    rows: std::sync::Arc<Vec<SidebarRow>>,
    pub dirty: bool,
    pub collapsed: BTreeSet<PathBuf>,
    renaming: Option<Rename>,
    /// When each recent change happened. Rows animate only while their change
    /// is fresh: the list is virtual, so a row scrolled back into view mounts
    /// again and would otherwise replay its entrance.
    changes: BTreeMap<Change, Instant>,
    /// Chats archived here that no host page has confirmed yet. A page that
    /// was already on its way still lists them; it must not bring them back.
    archiving: BTreeSet<Uuid>,
}
impl Default for SidebarUi {
    fn default() -> Self {
        Self {
            rows: Default::default(),
            dirty: true,
            collapsed: Default::default(),
            renaming: None,
            changes: BTreeMap::new(),
            archiving: BTreeSet::new(),
        }
    }
}
impl SidebarUi {
    /// A new, unsaved chat's row slides in at the top of its project.
    pub(super) fn note_new_chat(&mut self, session: Uuid) {
        self.note(Change::Arrived(session));
    }
    fn note(&mut self, change: Change) {
        self.changes.insert(change, Instant::now());
    }
    /// Whether `change` happened recently enough to still be animating.
    fn fresh(&self, change: &Change) -> bool {
        self.changes
            .get(change)
            .is_some_and(|at| at.elapsed() < motion::SLOW)
    }
    fn forget_settled(&mut self) {
        self.changes.retain(|_, at| at.elapsed() < motion::SLOW);
    }
    /// Whether `id` was archived here and no host page has confirmed it.
    pub(super) fn archiving(&self, id: Uuid) -> bool {
        self.archiving.contains(&id)
    }
    /// A host page that leaves out an archived chat was built after the
    /// archive, so that chat no longer needs filtering.
    pub(super) fn confirm_archived(&mut self, page: &BTreeSet<Uuid>) {
        self.archiving.retain(|id| page.contains(id));
    }
    /// The title input while a rename started from the toolbar is open.
    pub(super) fn header_rename_input(&self) -> Option<Entity<InputState>> {
        self.renaming
            .as_ref()
            .filter(|rename| rename.in_header)
            .map(|rename| rename.input.clone())
    }
    pub(super) fn renaming_focused(&self, window: &Window, cx: &App) -> bool {
        self.renaming.as_ref().is_some_and(|rename| {
            rename
                .input
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
        })
    }
}
impl Xylo {
    pub(super) fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.settings_open {
            return self.settings_navigation(cx);
        }
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
            .px(px(8.))
            .pb(px(8.))
            .child(self.resize_edge(Panel::Sidebar));
        let shortcut = |mac: &'static str, other: &'static str| {
            div()
                .flex_shrink_0()
                .text_size(px(11.))
                .text_color(rgb(faint()))
                .child(if cfg!(target_os = "macos") {
                    mac
                } else {
                    other
                })
        };
        let mut primary = div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .pb(px(6.))
            .child(
                nav_button("sidebar-new-chat", "New chat", AppIcon::NewChat)
                    .child(shortcut("⌘N", "Ctrl N"))
                    .on_click(cx.listener(|view, _, _, cx| view.new_chat(cx))),
            )
            .child(
                nav_button("search-chats", "Search chats", AppIcon::Search)
                    .child(shortcut("⌘K", "Ctrl K"))
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.open_history_search(None, window, cx)
                    })),
            )
            .child(
                nav_item(
                    "sidebar-saved-drafts",
                    "Saved drafts",
                    AppIcon::Drafts,
                    self.page == Page::Drafts,
                )
                .on_click(cx.listener(|view, _, _, cx| view.open_saved_drafts(cx))),
            );
        primary = primary.child(
            nav_item(
                "scheduled",
                "Scheduled",
                AppIcon::Scheduled,
                self.page == Page::Schedules,
            )
            .on_click(cx.listener(|view, _, _, cx| view.navigate(Page::Schedules, cx))),
        );
        // Brand row: mark, wordmark and the collapse control.
        sidebar = sidebar
            .child(
                div()
                    .h(px(52.))
                    .flex_shrink_0()
                    .pl(px(8.))
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
                        icons::icon_button(
                            "collapse-sidebar",
                            AppIcon::SidebarToggle,
                            "Hide sidebar",
                        )
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.settings.ui.sidebar_visible = false;
                            view.persist_ui(cx);
                            cx.notify();
                        })),
                    ),
            )
            .child(primary)
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
                        icons::icon_button("add-project", AppIcon::OpenProject, "Open project…")
                            .on_click(
                                cx.listener(|view, _, window, cx| view.pick_workspace(window, cx)),
                            ),
                    ),
            );
        if self.sidebar_state.dirty {
            self.sidebar_state.dirty = false;
            self.sidebar_state.rows = std::sync::Arc::new(project_rows(
                &self.conversations,
                self.workspace.as_deref(),
                &self.settings.ui.open_projects,
                &self.sidebar_state.collapsed,
                self.messages.is_empty()
                    && !self
                        .conversations
                        .iter()
                        .any(|chat| chat.id == self.session),
                self.conversations_has_more || self.conversations_loading_more,
            ));
        }
        self.sidebar_state.forget_settled();
        let rows = self.sidebar_state.rows.clone();
        let weak = cx.entity().downgrade();
        let content = uniform_list(
            "conversation-list-rows",
            rows.len(),
            move |range, window, cx| {
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
                                let disclosure = Disclosure::new(
                                    ElementId::Name(format!("project-{}", path.display()).into()),
                                    expanded,
                                    window,
                                    cx,
                                );
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
                                                    disclosure
                                                        .chevron()
                                                        .text_color(rgb(icons::rest_color())),
                                                )
                                                .child(icons::icon(AppIcon::Folder).text_color(
                                                    rgb(if current {
                                                        accent_text()
                                                    } else {
                                                        icons::rest_color()
                                                    }),
                                                ))
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .single_line()
                                                        .child(label),
                                                )
                                                .accessibility_label(format!(
                                                    "{} project {}",
                                                    if expanded { "Collapse" } else { "Expand" },
                                                    path.display()
                                                ))
                                                .on_click(cx.listener(move |view, _, _, cx| {
                                                    if view.sidebar_state.collapsed.remove(&path) {
                                                        view.sidebar_state
                                                            .note(Change::Expanded(path.clone()));
                                                    } else {
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
                                            icons::icon_button(
                                                ("project-new-chat", i),
                                                AppIcon::NewChat,
                                                format!("New chat in {}", new_path.display()),
                                            )
                                            .opacity(0.)
                                            .group_hover("sidebar-project", |style| {
                                                style.opacity(1.)
                                            })
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
                            SidebarRow::DraftChat => {
                                let row = sidebar_child_row(
                                    sidebar_chat_button(
                                        "unsaved-chat",
                                        "New chat".into(),
                                        true,
                                        false,
                                        false,
                                    )
                                    .on_click(cx.listener(
                                        |view, _, _, cx| {
                                            view.page = Page::Chat;
                                            view.settings_open = false;
                                            cx.notify();
                                        },
                                    )),
                                );
                                Some(
                                    if view.sidebar_state.fresh(&Change::Arrived(view.session)) {
                                        motion::fade_slide_in(
                                            ElementId::Name(
                                                format!("chat-enter-{}", view.session).into(),
                                            ),
                                            row,
                                            -10.,
                                        )
                                        .into_any_element()
                                    } else {
                                        row.into_any_element()
                                    },
                                )
                            }
                            SidebarRow::Conversation(index) => {
                                let conversation = view.conversations.get(*index)?;
                                let id = conversation.id;
                                if let Some(rename) = view
                                    .sidebar_state
                                    .renaming
                                    .as_ref()
                                    .filter(|r| r.id == id && !r.in_header)
                                {
                                    return Some(
                                        div()
                                            .w_full()
                                            .h(px(32.))
                                            .pl(px(CHAT_INSET))
                                            .flex()
                                            .items_center()
                                            .child(
                                                Input::new(&rename.input)
                                                    .aria_label("Chat title")
                                                    .h(px(28.)),
                                            )
                                            .into_any_element(),
                                    );
                                }
                                let path = conversation.workspace.clone();
                                let running = view.turns.get(&id).is_some_and(|turn| turn.running);
                                let pinned = conversation.pinned;
                                let busy = view.chat_busy(id);
                                let weak = cx.entity().downgrade();
                                let title = display_title(conversation);
                                let state = &view.sidebar_state;
                                // A new chat drops in from above; chats revealed by
                                // expanding their project settle a shorter way.
                                let entrance = if state.fresh(&Change::Arrived(id)) {
                                    Some(-10.)
                                } else if state.fresh(&Change::Expanded(path.clone())) {
                                    Some(-4.)
                                } else {
                                    None
                                };
                                let retitled = state.fresh(&Change::Retitled(id));
                                let button = sidebar_chat_button(
                                    ("conversation", i),
                                    title.clone(),
                                    view.session == id,
                                    running,
                                    pinned,
                                )
                                .on_click(cx.listener(
                                    move |view, event: &ClickEvent, window, cx| {
                                        if event.click_count() >= 2 {
                                            view.start_rename(id, false, window, cx);
                                        } else {
                                            view.open_conversation(id, path.clone(), window, cx)
                                        }
                                    },
                                ));
                                let button = if retitled {
                                    motion::fade_on_change(
                                        format!("chat-title-{id}"),
                                        &title,
                                        button,
                                    )
                                    .into_any_element()
                                } else {
                                    button.into_any_element()
                                };
                                let hover_weak = weak.clone();
                                // The hover "…" opens the same menu as a right-click.
                                let more =
                                    div()
                                        .absolute()
                                        .right(px(2.))
                                        .top(px(2.))
                                        .opacity(0.)
                                        .group_hover("sidebar-chat", |style| style.opacity(1.))
                                        .child(
                                            icons::menu_trigger(
                                                ("chat-actions", i),
                                                AppIcon::More,
                                                format!("Actions for {title}"),
                                            )
                                            .dropdown_menu(move |menu, _, _| {
                                                chat_menu(
                                                    menu,
                                                    hover_weak.clone(),
                                                    id,
                                                    pinned,
                                                    busy,
                                                )
                                            }),
                                        );
                                let row = sidebar_child_row(button)
                                    .id(("conversation-row", i))
                                    .relative()
                                    .group("sidebar-chat")
                                    .child(more)
                                    .context_menu(move |menu, _, _| {
                                        chat_menu(menu, weak.clone(), id, pinned, busy)
                                    });
                                Some(match entrance {
                                    Some(from) => motion::fade_slide_in(
                                        ElementId::Name(format!("chat-enter-{id}").into()),
                                        row,
                                        from,
                                    )
                                    .into_any_element(),
                                    None => row.into_any_element(),
                                })
                            }
                            SidebarRow::More => {
                                // Reaching the end of the list loads the next
                                // page; no button to press.
                                if !view.conversations_loading_more && view.host_running() {
                                    cx.spawn(async move |view, cx| {
                                        let _ =
                                            view.update(cx, |view, cx| view.more_conversations(cx));
                                    })
                                    .detach();
                                }
                                Some(
                                    div()
                                        .id("more-conversations")
                                        .h(px(32.))
                                        .pl(px(CHAT_INSET + 10.))
                                        .flex()
                                        .items_center()
                                        .child(motion::spinner_inline())
                                        .into_any_element(),
                                )
                            }
                        })
                        .collect()
                })
                .unwrap_or_default()
            },
        )
        .track_scroll(&self.conversation_scroll)
        .flex_1()
        .min_h_0();
        let person = person_name();
        let initial = person
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "X".into());
        // Until the host answers the first list, placeholder rows hold the
        // place of the chats instead of an empty column.
        let content = if self.conversations_loaded || self.host.is_none() {
            content.into_any_element()
        } else {
            div()
                .id("sidebar-loading")
                .test_support()
                .aria_label("Loading chats")
                .flex_1()
                .min_h_0()
                .pt(px(2.))
                .child(motion::skeleton_row(0))
                .child(motion::skeleton_list(5).pl(px(22.)))
                .into_any_element()
        };
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
                                    .single_line()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(rgb(text()))
                                    .child(person),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .single_line()
                                    .text_size(px(11.5))
                                    .text_color(rgb(muted()))
                                    .child(self.account_caption()),
                            ),
                    )
                    .child(icons::icon(AppIcon::Settings).text_color(rgb(icons::rest_color())))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.settings_open = true;
                        this.navigate(Page::Appearance, cx);
                    })),
            )
            .into_any_element()
    }
    /// Who the footer says is using Xylo: the ChatGPT account's email or
    /// plan when that account is the connection, otherwise the provider's
    /// name. The model already shows on the composer chip.
    fn account_caption(&self) -> String {
        let provider = &self.settings.provider;
        if provider.credential_ref.as_deref() == Some(CHATGPT_REF) {
            return self
                .account_email()
                .map(str::to_owned)
                .unwrap_or_else(|| "ChatGPT plan".into());
        }
        let name = provider.name.trim();
        if name.is_empty() {
            "No provider connected".into()
        } else {
            name.to_owned()
        }
    }
    /// Pins or unpins a chat. The row moves at once; the host's fresh page
    /// confirms it.
    pub(super) fn set_chat_pinned(&mut self, id: Uuid, pinned: bool, cx: &mut Context<Self>) {
        if let Some(chat) = self.conversations.iter_mut().find(|chat| chat.id == id) {
            chat.pinned = pinned;
            self.sidebar_state.dirty = true;
        }
        self.host_call(cx, |host| host.set_conversation_pinned(id, pinned));
    }
    /// Archives a chat: it leaves the list and search but keeps its history.
    /// Archiving needs no confirmation; a toast offers Undo. Archiving the
    /// open chat moves to a new chat.
    pub(super) fn archive_chat(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if self.chat_busy(id) || !self.conversations.iter().any(|chat| chat.id == id) {
            return;
        }
        // Leave the open chat first; when leaving is blocked (unsaved draft,
        // interrupted action) nothing is archived.
        if self.session == id {
            self.new_chat(cx);
            if self.session == id {
                return;
            }
        }
        let Some(index) = self.conversations.iter().position(|chat| chat.id == id) else {
            return;
        };
        let mut chat = self.conversations.remove(index);
        chat.archived = true;
        self.sidebar_state.archiving.insert(id);
        self.sidebar_state.dirty = true;
        self.status =
            StatusLine::with_undo("Chat archived", app_status::ToastUndo::Unarchive(chat));
        self.host_call(cx, |host| host.set_conversation_archived(id, true));
    }
    /// A chat with a running turn, an approval waiting or turns queued keeps
    /// working; archiving it would hide that work, so Archive is off.
    pub(super) fn chat_busy(&self, id: Uuid) -> bool {
        self.turns
            .get(&id)
            .is_some_and(|turn| turn.running || turn.pending_approvals().next().is_some())
            || self
                .turn_queue
                .items
                .get(&id)
                .is_some_and(|queued| !queued.is_empty())
    }
    /// Undo for Archive: the chat returns to its place in the list at once,
    /// even when it is older than the first page the host sends back.
    pub(super) fn unarchive_chat(&mut self, mut chat: Conversation, cx: &mut Context<Self>) {
        let id = chat.id;
        chat.archived = false;
        self.sidebar_state.archiving.remove(&id);
        if !self.conversations.iter().any(|existing| existing.id == id) {
            self.conversations.push(chat);
            self.conversations.sort_by(|left, right| {
                right
                    .updated_at
                    .cmp(&left.updated_at)
                    .then_with(|| right.id.cmp(&left.id))
            });
            self.sidebar_state.dirty = true;
        }
        self.host_call(cx, |host| host.set_conversation_archived(id, false));
    }
    /// Renames the open chat in the toolbar title.
    pub(super) fn start_header_rename(
        &mut self,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_rename(id, true, window, cx);
    }
    pub(super) fn start_rename(
        &mut self,
        id: Uuid,
        in_header: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(conversation) = self.conversations.iter().find(|chat| chat.id == id) else {
            return;
        };
        let title = display_title(conversation);
        let input = cx.new(|cx| InputState::new(window, cx).default_value(title));
        let subscription =
            cx.subscribe(&input, move |view, _, event: &InputEvent, cx| match event {
                InputEvent::PressEnter { .. } | InputEvent::Blur => view.commit_rename(id, cx),
                _ => {}
            });
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        self.sidebar_state.renaming = Some(Rename {
            id,
            input,
            in_header,
            _subscription: subscription,
        });
        cx.notify();
    }
    pub(super) fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.sidebar_state.renaming = None;
        cx.notify();
    }
    /// Sends a changed, non-empty title to the host and shows it right away;
    /// the host's `ConversationRenamed` answer is authoritative.
    fn commit_rename(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some(rename) = self
            .sidebar_state
            .renaming
            .take_if(|rename| rename.id == id)
        else {
            return;
        };
        let title = rename.input.read(cx).value().trim().to_owned();
        let Some(conversation) = self.conversations.iter_mut().find(|chat| chat.id == id) else {
            cx.notify();
            return;
        };
        if !title.is_empty() && title != display_title(conversation) {
            conversation.title = title.clone();
            conversation.title_source = TitleSource::User;
            self.sidebar_state.note(Change::Retitled(id));
            self.sidebar_state.dirty = true;
            self.host_call(cx, |host| host.rename_conversation(id, title));
        }
        cx.notify();
    }
    /// Notes which chats are new or renamed since `before`, so their rows
    /// animate once.
    pub(super) fn note_conversation_changes(&mut self, before: &[Conversation]) {
        for change in conversation_changes(
            before,
            &self.conversations,
            self.conversations_loaded,
            self.session,
        ) {
            self.sidebar_state.note(change);
        }
    }
    /// Applies a stored title to the sidebar and to open search results.
    pub(super) fn conversation_renamed(&mut self, conversation: Conversation) {
        self.history_search.retitle(&conversation);
        match self
            .conversations
            .iter_mut()
            .find(|chat| chat.id == conversation.id)
        {
            Some(existing) => {
                if display_title(existing) != display_title(&conversation) {
                    self.sidebar_state.note(Change::Retitled(conversation.id));
                }
                *existing = conversation;
            }
            None => {
                self.conversations.push(conversation);
                self.conversations.sort_by(|left, right| {
                    right
                        .updated_at
                        .cmp(&left.updated_at)
                        .then_with(|| right.id.cmp(&left.id))
                });
            }
        }
        self.sidebar_state.dirty = true;
    }
}
/// The sidebar changes between two conversation lists: chats that arrived and
/// chats whose shown title changed. The first list (`loaded` false) is not
/// news, and the open chat never "arrives": its row already entered as the
/// unsaved draft.
fn conversation_changes(
    before: &[Conversation],
    after: &[Conversation],
    loaded: bool,
    session: Uuid,
) -> Vec<Change> {
    if !loaded {
        return Vec::new();
    }
    let previous: BTreeMap<Uuid, String> = before
        .iter()
        .map(|chat| (chat.id, display_title(chat)))
        .collect();
    after
        .iter()
        .filter_map(|chat| match previous.get(&chat.id) {
            None if chat.id == session => None,
            None => Some(Change::Arrived(chat.id)),
            Some(title) if *title != display_title(chat) => Some(Change::Retitled(chat.id)),
            Some(_) => None,
        })
        .collect()
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
/// How far chat rows sit in from their project row.
const CHAT_INSET: f32 = 32.;
/// The chat context menu, shared by right-click and the row's hover "…".
fn chat_menu(
    menu: PopupMenu,
    view: WeakEntity<Xylo>,
    id: Uuid,
    pinned: bool,
    busy: bool,
) -> PopupMenu {
    let rename = view.clone();
    let pin = view.clone();
    menu.item(PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
        let _ = rename.update(cx, |view, cx| view.start_rename(id, false, window, cx));
    }))
    .item(
        PopupMenuItem::new(if pinned { "Unpin" } else { "Pin" }).on_click(move |_, _, cx| {
            let _ = pin.update(cx, |view, cx| view.set_chat_pinned(id, !pinned, cx));
        }),
    )
    .item(archive_item(busy, move |cx| {
        let _ = view.update(cx, |view, cx| view.archive_chat(id, cx));
    }))
}
/// Archive, or a greyed Archive that says why while the chat is busy.
pub(super) fn archive_item(busy: bool, archive: impl Fn(&mut App) + 'static) -> PopupMenuItem {
    if !busy {
        return PopupMenuItem::new("Archive").on_click(move |_, _, cx| archive(cx));
    }
    PopupMenuItem::element(|_, _| {
        div()
            .id("archive-unavailable")
            .tooltip(|_, cx| {
                cx.new(|_| XyloTooltip("Stop the task before archiving".into()))
                    .into()
            })
            .child("Archive")
    })
    .disabled(true)
}
/// All virtual rows share a 32px stride; chat rows are inset under their project.
fn sidebar_child_row(button: impl IntoElement) -> Div {
    div()
        .w_full()
        .min_w_0()
        .h(px(32.))
        .pl(px(CHAT_INSET))
        .flex()
        .items_center()
        .child(button)
}
fn sidebar_chat_button(
    id: impl Into<ElementId>,
    title: String,
    active: bool,
    running: bool,
    pinned: bool,
) -> gpui_kit::base::Button {
    let mut label = title.clone();
    if pinned {
        label.push_str(", pinned");
    }
    if running {
        label.push_str(", running");
    }
    recent_item(id, title, active)
        .accessibility_label(label)
        .h(px(28.))
        // A quiet pin marks a pinned chat; the running dot takes its place
        // while a turn runs.
        .when(pinned && !running, |button| {
            button.child(
                div()
                    .id("chat-pinned-mark")
                    .flex_shrink_0()
                    .child(icons::small(AppIcon::Pin).text_color(rgb(icons::rest_color()))),
            )
        })
        .when(running, |button| {
            button.child(motion::pulse(
                "chat-running-dot",
                div()
                    .size(px(6.))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(rgb(brand())),
            ))
        })
}

#[cfg(test)]
#[path = "sidebar/motion_tests.rs"]
mod motion_tests;
#[cfg(test)]
#[path = "sidebar/tests.rs"]
mod tests;
