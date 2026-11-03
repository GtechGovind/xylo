//! The chat search palette (Cmd-K, or "Search chats" in the sidebar): a modal
//! over the workspace. With no query it lists recent chats; typing filters
//! chat titles instantly and, after a short pause, lists matching messages
//! from the whole database below them. Scrolling to the end loads the next
//! page of chats or matches. Enter opens the highlighted row, the arrow keys
//! move the highlight, and Escape or a click outside closes the palette.
//! Message search pages are bounded and request-correlated, so a late answer
//! for an older query or another project is dropped.
use super::*;
use crate::icons::{self, AppIcon};
use xylo_runtime::history::{HistoryMatch, HistorySearchPage};

/// Every palette row has this height, so the list can be virtual.
const ROW_HEIGHT: f32 = 44.;
/// Rows shown before the list scrolls.
const VISIBLE_ROWS: usize = 9;
/// How close to the end of the list scrolling starts loading the next page.
const LOAD_AHEAD: usize = 4;
/// How long typing must pause before message text is searched.
const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(220);

struct PendingSearch {
    id: Uuid,
    query: String,
    workspace: Option<PathBuf>,
    /// This request continues the current results rather than replacing them.
    more: bool,
}

/// One row of the palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaletteRow {
    /// A chat, by its index in the loaded conversation list.
    Chat(usize),
    /// The caption above message matches.
    MessagesHeading,
    /// A message match, by its index in the search results.
    Message(usize),
    /// A one-line state: searching, nothing found, or an error.
    Status,
}

pub(super) struct HistorySearchUi {
    chat_query: Entity<InputState>,
    _chat_subscription: Subscription,
    chat_session: Option<Uuid>,
    chat_request: Option<Uuid>,
    chat_hits: Vec<(Uuid, String)>,
    chat_error: Option<String>,
    open: bool,
    return_focus: Option<FocusHandle>,
    items: Vec<HistoryMatch>,
    before: Option<ConversationCursor>,
    has_more: bool,
    request: Option<PendingSearch>,
    debounce: Option<Task<()>>,
    generation: u64,
    /// Searching, nothing found, or a failure; empty when results speak.
    notice: String,
    /// The highlighted row, which Enter opens.
    highlighted: usize,
    /// A next page was requested from the list's render pass and is waiting
    /// for the frame to end.
    more_queued: bool,
    scroll: UniformListScrollHandle,
}
impl HistorySearchUi {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Xylo>) -> Self {
        let chat_query = cx.new(|cx| InputState::new(window, cx).placeholder("Search messages…"));
        let subscription = cx.subscribe(&chat_query, |view, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                view.chat_search_changed(cx);
            }
        });
        Self {
            chat_query,
            _chat_subscription: subscription,
            chat_session: None,
            chat_request: None,
            chat_hits: vec![],
            chat_error: None,
            open: false,
            return_focus: None,
            items: Vec::new(),
            before: None,
            has_more: false,
            request: None,
            debounce: None,
            generation: 0,
            notice: String::new(),
            highlighted: 0,
            more_queued: false,
            scroll: UniformListScrollHandle::new(),
        }
    }
    /// Whether the palette is showing.
    pub(super) fn is_open(&self) -> bool {
        self.open
    }
    /// Forgets message results and any search in flight.
    pub(super) fn reset(&mut self) {
        self.items.clear();
        self.before = None;
        self.has_more = false;
        self.request = None;
        self.debounce = None;
        self.generation = self.generation.wrapping_add(1);
        self.notice.clear();
        self.highlighted = 0;
    }
    /// Shows a renamed conversation's new title in the current results.
    pub(super) fn retitle(&mut self, conversation: &Conversation) {
        for hit in &mut self.items {
            if hit.conversation.id == conversation.id {
                hit.conversation.title = conversation.title.clone();
                hit.conversation.title_source = conversation.title_source;
            }
        }
    }
    #[cfg(test)]
    #[allow(dead_code)] // Used by the separate native validation crate.
    pub(super) fn validation_items(&self) -> &[HistoryMatch] {
        &self.items
    }
    #[cfg(test)]
    #[allow(dead_code)] // Used by the separate native validation crate.
    pub(super) fn validation_request(&self) -> Option<Uuid> {
        self.request.as_ref().map(|request| request.id)
    }

    #[cfg(test)]
    pub(super) fn validation_notice(&self) -> &str {
        &self.notice
    }
}

/// Indices of the chats whose shown title contains `query`, ignoring case,
/// in list order. An empty query matches every chat.
fn title_matches(conversations: &[Conversation], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    conversations
        .iter()
        .enumerate()
        .filter(|(_, chat)| {
            query.is_empty() || sidebar::display_title(chat).to_lowercase().contains(&query)
        })
        .map(|(index, _)| index)
        .collect()
}

/// The palette's rows: chats first, then message matches under a caption,
/// then a status line when there is something to say.
fn palette_rows(chats: &[usize], messages: usize, notice: &str) -> Vec<PaletteRow> {
    let mut rows: Vec<PaletteRow> = chats.iter().copied().map(PaletteRow::Chat).collect();
    if messages > 0 {
        rows.push(PaletteRow::MessagesHeading);
        rows.extend((0..messages).map(PaletteRow::Message));
    }
    if !notice.is_empty() {
        rows.push(PaletteRow::Status);
    }
    rows
}

/// A short relative age for a Unix timestamp in seconds or milliseconds.
pub(super) fn age_label(timestamp: i64) -> String {
    let seconds = if timestamp > 100_000_000_000 {
        timestamp / 1000
    } else {
        timestamp
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    let elapsed = now.saturating_sub(seconds).max(0);
    match elapsed {
        0..60 => "now".into(),
        60..3600 => format!("{}m", elapsed / 60),
        3600..86_400 => format!("{}h", elapsed / 3600),
        86_400..604_800 => format!("{}d", elapsed / 86_400),
        _ => format!("{}w", elapsed / 604_800),
    }
}

impl Xylo {
    pub(super) fn chat_search_received(
        &mut self,
        id: Uuid,
        session: Uuid,
        result: Result<Vec<(Uuid, String)>, String>,
        cx: &mut Context<Self>,
    ) {
        if self.history_search.chat_request != Some(id)
            || self.history_search.chat_session != Some(session)
            || self.session != session
        {
            return;
        }
        self.history_search.chat_request = None;
        match result {
            Ok(items) => self.history_search.chat_hits = items,
            Err(_) => {
                self.history_search.chat_error =
                    Some("Search couldn’t be completed. Try again.".into())
            }
        }
        cx.notify();
    }
    fn chat_search_changed(&mut self, cx: &mut Context<Self>) {
        self.history_search.chat_hits.clear();
        self.history_search.chat_error = None;
        self.history_search.chat_request = None;
        let Some(session) = self
            .history_search
            .chat_session
            .filter(|session| *session == self.session)
        else {
            return;
        };
        let query = self
            .history_search
            .chat_query
            .read(cx)
            .value()
            .trim()
            .to_owned();
        if query.is_empty() {
            cx.notify();
            return;
        }
        if query.len() > 512 {
            self.history_search.chat_error = Some("Try a shorter search.".into());
            cx.notify();
            return;
        }
        let id = Uuid::new_v4();
        if let Some(host) = &self.host {
            match host.search_chat(id, session, query) {
                Ok(()) => self.history_search.chat_request = Some(id),
                Err(_) => {
                    self.history_search.chat_error = Some("Saved history is unavailable.".into())
                }
            }
        } else {
            self.history_search.chat_error = Some("Saved history is unavailable.".into());
        }
        cx.notify();
    }
    pub(super) fn open_chat_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::WindowExt;
        self.history_search.chat_session = Some(self.session);
        self.history_search.chat_hits.clear();
        self.history_search.chat_error = None;
        self.history_search.chat_request = None;
        self.history_search
            .chat_query
            .update(cx, |input, cx| input.set_value("", window, cx));
        let session = self.session;
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let body = weak.update(cx, |view, cx| {
                let query = view.history_search.chat_query.read(cx).value().trim().to_lowercase();
                let body = div().flex().flex_col().gap(px(14.)).child(Input::new(&view.history_search.chat_query).aria_label("Search messages").w_full());
                let mut matches = div().id("chat-search-results").test_support().h(px(320.)).overflow_y_scroll().flex().flex_col().gap(px(10.));
                let mut results=view.history_search.chat_hits.clone();
                if !query.is_empty() && view.session==session {
                    for message in &view.messages {if message.content.to_lowercase().contains(&query) && !results.iter().any(|(id,_)|*id==message.id) {results.push((message.id,message.content.chars().take(500).collect()));}}
                    if let Some(turn)=view.turns.get(&session) {for segment in &turn.segments {if let crate::state::Segment::Text(text)=segment&& text.to_lowercase().contains(&query) {results.push((Uuid::nil(),text.chars().take(500).collect()));}}}
                }
                results.truncate(100);
                if results.is_empty() && !query.is_empty() {matches=matches.child(subtitle(if view.history_search.chat_request.is_some() {"Searching…"} else {"No matching messages."}));}
                if results.len()>=100 {matches=matches.child(subtitle("Showing the first 100 matches. Refine your search for more specific results."));}
                for (index,(id,snippet)) in results.into_iter().enumerate() {
                    matches = matches.child(focus_ring(gpui_kit::base::Button::new(("chat-search-result",index))).w_full().p(px(12.)).rounded(px(8.)).text_size(px(13.)).text_color(rgb(text_secondary())).hover(|style|style.bg(rgb(hover()))).child(snippet).on_click(cx.listener(move |view,_,window,cx| {
                        window.close_dialog(cx);
                        if view.session != session {return;}
                        if id.is_nil() {view.jump_to_latest(cx);}
                        else if let Some(index)=view.messages.iter().position(|message|message.id==id) {if let Some(history)=view.histories.get(&session) {history.list.scroll_to(gpui_kit::ListOffset {item_ix:index+1,offset_in_item:px(0.)});}}
                        else if let Some(chat)=view.conversations.iter().find(|chat|chat.id==session).cloned() {view.open_history_message(chat,id,window,cx);}
                        cx.notify();
                    })));
                }
                body.when_some(view.history_search.chat_error.clone(),|body,error|body.child(subtitle(error))).child(matches).into_any_element()
            }).unwrap_or_else(|_|div().into_any_element());
            let close=weak.clone();
            dialog.title("Find in chat").width(px(720.)).rounded(px(16.)).bg(rgb(panel())).border_color(rgb(border_strong())).on_ok(|_,_,_|false).on_close(move |_,_,cx|{let _=close.update(cx,|view,_|{view.history_search.chat_session=None;view.history_search.chat_request=None;});}).child(body)
        });
    }
    /// Opens the palette with an empty query, or closes it when open.
    pub(super) fn open_history_search(
        &mut self,
        query: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.history_search.open && query.is_none() {
            self.close_history_search(window, cx);
            return;
        }
        if !self.history_search.open {
            self.history_search.return_focus = window.focused(cx);
        }
        self.history_search.reset();
        self.history_search.open = true;
        // The input's change event starts the search for a seeded query.
        let query = query.unwrap_or_default();
        self.conversation_query
            .update(cx, |input, cx| input.set_value(query, window, cx));
        self.conversation_query
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }
    pub(super) fn close_history_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.history_search.reset();
        self.history_search.open = false;
        if let Some(focus) = self.history_search.return_focus.take() {
            focus.focus(window, cx);
        }
        cx.notify();
    }
    pub(super) fn history_search_changed(&mut self, cx: &mut Context<Self>) {
        self.history_search.reset();
        let query = self.conversation_query.read(cx).value().trim().to_string();
        if query.is_empty() {
            cx.notify();
            return;
        }
        self.history_search.notice = "Searching messages…".into();
        let generation = self.history_search.generation;
        self.history_search.debounce = Some(cx.spawn(async move |view, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let _ = view.update(cx, |view, cx| {
                if view.history_search.generation == generation {
                    view.run_history_search(false, cx);
                }
            });
        }));
        cx.notify();
    }
    /// Searches message text for the current query now, without waiting for
    /// typing to pause.
    #[cfg(test)]
    #[allow(dead_code)] // Used by the separate native validation crate.
    pub(super) fn history_search_submit(&mut self, cx: &mut Context<Self>) {
        self.history_search.debounce = None;
        self.run_history_search(false, cx);
    }
    fn run_history_search(&mut self, more: bool, cx: &mut Context<Self>) {
        let query = self.conversation_query.read(cx).value().trim().to_string();
        if query.is_empty() {
            self.history_search.reset();
            cx.notify();
            return;
        }
        if query.len() > 512 {
            self.history_search.notice = "Try a shorter search.".into();
            cx.notify();
            return;
        }
        if more && (!self.history_search.has_more || self.history_search.request.is_some()) {
            return;
        }
        let id = Uuid::new_v4();
        let before = if more {
            self.history_search.before.clone()
        } else {
            None
        };
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.search_history(id, query.clone(), before, 100));
        if result.is_err() {
            // Scrolling must not retry a page that could not be requested.
            self.history_search.has_more = false;
            self.history_search.notice = "Saved message search is unavailable. Try again.".into();
        }
        if result.is_ok() {
            self.history_search.request = Some(PendingSearch {
                id,
                query,
                workspace: self.workspace.clone(),
                more,
            });
            if !more {
                self.history_search.notice = "Searching messages…".into();
            }
        }
        self.result(result, cx);
    }
    pub(super) fn history_search_result(
        &mut self,
        id: Uuid,
        result: std::result::Result<HistorySearchPage, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self
            .history_search
            .request
            .take_if(|pending| pending.id == id)
        else {
            return;
        };
        let current = self.conversation_query.read(cx).value().trim().to_string();
        if pending.query != current || pending.workspace != self.workspace {
            return;
        }
        match result {
            Ok(page) if page.query == current => {
                if pending.more {
                    self.history_search.items.extend(page.items);
                } else {
                    self.history_search.items = page.items;
                }
                self.history_search.before = page.before;
                self.history_search.has_more = page.has_more;
                self.history_search.notice.clear();
                if self.history_search.items.is_empty()
                    && title_matches(&self.conversations, &current).is_empty()
                {
                    self.history_search.notice = format!("No chats match “{current}”.");
                }
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(detail=%error,"History search failed");
                self.history_search.notice = "Messages could not be searched. Try again.".into();
                self.history_search.has_more = false;
            }
        }
        cx.notify();
    }
    fn palette_rows(&self, cx: &App) -> (Vec<usize>, Vec<PaletteRow>) {
        let query = self.conversation_query.read(cx).value().trim().to_owned();
        let chats = title_matches(&self.conversations, &query);
        let notice = if query.is_empty() && self.conversations.is_empty() {
            "No chats yet."
        } else {
            self.history_search.notice.as_str()
        };
        let rows = palette_rows(&chats, self.history_search.items.len(), notice);
        (chats, rows)
    }
    /// Loads the next page of whatever the palette is listing once scrolling
    /// nears the end: recent chats for an empty query, message matches
    /// otherwise. Runs after the frame, never during it.
    fn palette_load_more(&mut self, cx: &mut Context<Self>) {
        if self.history_search.more_queued {
            return;
        }
        let empty = self.conversation_query.read(cx).value().trim().is_empty();
        let wanted = if empty {
            self.conversations_has_more && !self.conversations_loading_more
        } else {
            self.history_search.has_more && self.history_search.request.is_none()
        };
        if !wanted {
            return;
        }
        self.history_search.more_queued = true;
        cx.spawn(async move |view, cx| {
            let _ = view.update(cx, |view, cx| {
                view.history_search.more_queued = false;
                if view.conversation_query.read(cx).value().trim().is_empty() {
                    view.more_conversations(cx);
                } else {
                    view.run_history_search(true, cx);
                }
            });
        })
        .detach();
    }
    /// Opens a palette row and closes the palette.
    fn palette_open(&mut self, row: PaletteRow, window: &mut Window, cx: &mut Context<Self>) {
        match row {
            PaletteRow::Chat(index) => {
                let Some(chat) = self.conversations.get(index) else {
                    return;
                };
                let (id, workspace) = (chat.id, chat.workspace.clone());
                self.close_history_search(window, cx);
                self.open_conversation(id, workspace, window, cx);
            }
            PaletteRow::Message(index) => {
                let Some(hit) = self.history_search.items.get(index).cloned() else {
                    return;
                };
                self.close_history_search(window, cx);
                match hit.message_id {
                    Some(anchor) => self.open_history_message(hit.conversation, anchor, window, cx),
                    None => self.open_conversation(
                        hit.conversation.id,
                        hit.conversation.workspace,
                        window,
                        cx,
                    ),
                }
            }
            PaletteRow::MessagesHeading | PaletteRow::Status => {}
        }
    }
    /// Enter in the palette's input: opens the highlighted row.
    pub(super) fn palette_activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (_, rows) = self.palette_rows(cx);
        if let Some(row) = rows.get(self.history_search.highlighted).copied() {
            self.palette_open(row, window, cx);
        }
    }
    /// Moves the highlight by `delta` rows, skipping captions.
    fn palette_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let (_, rows) = self.palette_rows(cx);
        let openable: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| matches!(row, PaletteRow::Chat(_) | PaletteRow::Message(_)))
            .map(|(index, _)| index)
            .collect();
        let Some(position) = openable
            .iter()
            .position(|index| *index >= self.history_search.highlighted)
            .or(openable.len().checked_sub(1))
        else {
            return;
        };
        let next = position
            .saturating_add_signed(delta)
            .min(openable.len().saturating_sub(1));
        self.history_search.highlighted = openable[next];
        self.history_search
            .scroll
            .scroll_to_item(openable[next], ScrollStrategy::Nearest);
        cx.notify();
    }
    pub(super) fn history_search_palette(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (chats, rows) = self.palette_rows(cx);
        let row_count = rows.len();
        let rows = std::sync::Arc::new(rows);
        let chats = std::sync::Arc::new(chats);
        let weak = cx.entity().downgrade();
        let list_rows = rows.clone();
        let list = uniform_list("history-palette-rows", row_count, move |range, _, cx| {
            weak.update(cx, |view, cx| {
                if range.end + LOAD_AHEAD >= list_rows.len() {
                    view.palette_load_more(cx);
                }
                range
                    .filter_map(|index| {
                        let row = *list_rows.get(index)?;
                        Some(view.palette_row(index, row, &chats, cx))
                    })
                    .collect()
            })
            .unwrap_or_default()
        })
        .track_scroll(&self.history_search.scroll)
        .h(px(ROW_HEIGHT * row_count.clamp(1, VISIBLE_ROWS) as f32))
        .px(px(6.))
        .w_full();
        let card = div()
            .id("history-palette")
            .test_support()
            .w(px(600.))
            .max_w(relative(0.92))
            .bg(rgb(panel()))
            .border_1()
            .border_color(rgb(border()))
            .rounded(px(14.))
            .shadow(crate::motion::surface_shadow(1.))
            .overflow_hidden()
            .flex()
            .flex_col()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(52.))
                    .px(px(14.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .text_color(rgb(muted()))
                    .child(icons::icon(AppIcon::Search))
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.conversation_query)
                                .aria_label("Search chats")
                                .appearance(false)
                                .bordered(false),
                        ),
                    ),
            )
            .child(divider())
            .child(div().py(px(6.)).child(list));
        div()
            .id("history-palette-scrim")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(hsla(
                0.,
                0.,
                0.,
                if crate::ui::is_dark() { 0.45 } else { 0.18 },
            ))
            .flex()
            .justify_center()
            .items_start()
            .pt(px(88.))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _, window, cx| view.close_history_search(window, cx)),
            )
            .capture_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => view.close_history_search(window, cx),
                    "down" => view.palette_move(1, cx),
                    "up" => view.palette_move(-1, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(card)
            .into_any_element()
    }
    fn palette_row(
        &self,
        index: usize,
        row: PaletteRow,
        chats: &[usize],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let highlighted = index == self.history_search.highlighted;
        let frame = |id: ElementId, label: String| {
            focus_ring(gpui_kit::base::Button::new(id))
                .accessibility_label(label)
                .w_full()
                .h(px(ROW_HEIGHT))
                .px(px(10.))
                .rounded(px(8.))
                .flex()
                .items_center()
                .gap(px(10.))
                .text_color(rgb(muted()))
                .bg(rgb(if highlighted { selected() } else { panel() }))
                .hover(|style| style.bg(rgb(hover())))
                .cursor_pointer()
        };
        let lines = |primary: String, secondary: String| {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .single_line()
                        .text_size(px(13.))
                        .text_color(rgb(text()))
                        .child(primary),
                )
                .child(
                    div()
                        .single_line()
                        .text_size(px(11.5))
                        .text_color(rgb(muted()))
                        .child(secondary),
                )
        };
        match row {
            PaletteRow::Chat(conversation) => {
                let Some(chat) = self.conversations.get(conversation) else {
                    return div().into_any_element();
                };
                let title = sidebar::display_title(chat);
                let project = project_name(&chat.workspace);
                let position = chats.iter().position(|item| *item == conversation);
                frame(
                    ("history-chat", position.unwrap_or(index)).into(),
                    format!("Open chat: {title}"),
                )
                .child(icons::icon(AppIcon::Chat))
                .child(lines(title, project))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(11.5))
                        .text_color(rgb(faint()))
                        .child(age_label(chat.updated_at)),
                )
                .on_click(
                    cx.listener(move |view, _, window, cx| view.palette_open(row, window, cx)),
                )
                .into_any_element()
            }
            PaletteRow::Message(item) => {
                let Some(hit) = self.history_search.items.get(item) else {
                    return div().into_any_element();
                };
                let title = sidebar::display_title(&hit.conversation);
                let snippet = hit.snippet.split_whitespace().collect::<Vec<_>>().join(" ");
                frame(
                    ("history-match", item).into(),
                    format!("Open matching message in {title}"),
                )
                .child(icons::icon(AppIcon::Search))
                .child(lines(snippet, title))
                .on_click(
                    cx.listener(move |view, _, window, cx| view.palette_open(row, window, cx)),
                )
                .into_any_element()
            }
            PaletteRow::MessagesHeading => div()
                .h(px(ROW_HEIGHT))
                .px(px(10.))
                .pb(px(6.))
                .flex()
                .items_end()
                .text_size(px(11.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(muted()))
                .child("Messages")
                .into_any_element(),
            PaletteRow::Status => {
                let empty = self.conversation_query.read(cx).value().trim().is_empty()
                    && self.conversations.is_empty();
                div()
                    .id("history-palette-status")
                    .test_support()
                    .h(px(ROW_HEIGHT))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(12.5))
                    .text_color(rgb(muted()))
                    .when(self.history_search.request.is_some(), |row| {
                        row.child(crate::motion::spinner_inline())
                    })
                    .child(if empty {
                        "No chats yet.".to_owned()
                    } else {
                        self.history_search.notice.clone()
                    })
                    .into_any_element()
            }
        }
    }
}

/// A project's folder name, for a chat's second line.
fn project_name(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Project".into())
}

#[cfg(all(test, not(feature = "validation")))]
#[path = "history_search/tests.rs"]
mod tests;
