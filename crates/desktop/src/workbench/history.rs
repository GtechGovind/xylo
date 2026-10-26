//! Variable-height native history list. Older pages splice in without moving the
//! visible message; tail following pauses when the reader scrolls upward.
use super::*;
use workbench_runtime::history::HistoryWindow;

pub(super) struct ChatHistory {
    pub list: ListState,
    messages: Vec<ChatMessage>,
    before: Option<i64>,
    pub has_more: bool,
    pub loading: bool,
    loaded: bool,
    older_request: bool,
    pub stale: bool,
    windowed: bool,
    after: Option<i64>,
    has_newer: bool,
    pending_anchor: Option<(Uuid, PathBuf)>,
    pending_after: Option<(i64, PathBuf)>,
    highlight: Option<Uuid>,
}
impl ChatHistory {
    fn new() -> Self {
        let list = ListState::new(0, ListAlignment::Top, px(500.));
        list.set_follow_mode(FollowMode::Tail);
        Self {
            list,
            messages: vec![],
            before: None,
            has_more: false,
            loading: false,
            loaded: false,
            older_request: false,
            stale: false,
            windowed: false,
            after: None,
            has_newer: false,
            pending_anchor: None,
            pending_after: None,
            highlight: None,
        }
    }
}
impl Workbench {
    pub(super) fn history_loading(&self) -> bool {
        self.histories
            .get(&self.session)
            .is_some_and(|history| history.loading)
    }
    pub(super) fn stash_history(&mut self) {
        self.histories
            .entry(self.session)
            .or_insert_with(ChatHistory::new)
            .messages = self.messages.clone();
    }
    pub(super) fn open_conversation(
        &mut self,
        id: Uuid,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.recovery_before_switch(cx) {
            return;
        }
        if self.editor_dirty {
            self.error = Some("Save your changes before switching conversations.".into());
            cx.notify();
            return;
        }
        if !self.draft_before_switch(cx) {
            return;
        }
        self.stash_history();
        self.set_workspace(path.clone(), window, cx);
        if self.workspace.as_ref() != Some(&path) {
            return;
        }
        self.session = id;
        self.recovery_scope_changed(cx);
        self.draft_switch_deferred(cx);
        if let Some(history) = self.histories.get_mut(&id) {
            if history.windowed {
                history.loaded = false;
            }
            history.windowed = false;
            history.pending_anchor = None;
            history.pending_after = None;
            history.highlight = None;
            history.loading = false;
        }
        self.messages = self
            .histories
            .get(&id)
            .map(|h| h.messages.clone())
            .unwrap_or_default();
        self.page = Page::Chat;
        let refresh = self.histories.get(&id).is_none_or(|h| !h.loaded || h.stale);
        if refresh {
            self.request_history(false, cx);
        }
        self.recovery_activate(window, cx);
        self.persist_ui(cx);
        cx.notify();
    }
    fn request_history(&mut self, older: bool, cx: &mut Context<Self>) {
        let history = self
            .histories
            .entry(self.session)
            .or_insert_with(ChatHistory::new);
        if history.loading || (older && !history.has_more) {
            return;
        }
        history.loading = true;
        history.older_request = older;
        let before = if older { history.before } else { None };
        let session = self.session;
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.load_session_page(session, before, 100));
        if result.is_err() {
            history.loading = false;
        }
        self.result(result, cx);
    }
    pub(super) fn load_older(&mut self, cx: &mut Context<Self>) {
        if self
            .histories
            .get(&self.session)
            .is_some_and(|history| history.loading || !history.has_more)
        {
            return;
        }
        if self
            .histories
            .get(&self.session)
            .is_some_and(|history| history.windowed)
            || self.messages.len() >= 300
        {
            if let Some(anchor) = self.messages.first().map(|message| message.id) {
                self.request_message_window(anchor, cx);
            }
            return;
        }
        self.request_history(true, cx);
    }
    pub(super) fn history_page(&mut self, page: HistoryPage, cx: &mut Context<Self>) {
        let history = self
            .histories
            .entry(page.session_id)
            .or_insert_with(ChatHistory::new);
        // A pending centered/newer request takes precedence over an older latest-
        // page refresh. Host responses are serialized; its own window is correlated.
        if history.pending_anchor.is_some() || history.pending_after.is_some() {
            return;
        }
        if history.windowed {
            history.loading = false;
            history.stale = true;
            history.has_newer = true;
            cx.notify();
            return;
        }
        let active = page.session_id == self.session;
        let messages = if active {
            &mut self.messages
        } else {
            &mut history.messages
        };
        let mut retained_cursor = false;
        if history.older_request {
            let existing: BTreeSet<_> = messages.iter().map(|message| message.id).collect();
            let mut older: Vec<_> = page
                .messages
                .into_iter()
                .filter(|message| !existing.contains(&message.id))
                .collect();
            let count = older.len();
            older.append(messages);
            *messages = older;
            // The header is always index zero. Splicing after it preserves
            // GPUI's logical anchor, including its pixel offset within a message.
            history.list.splice(1..1, count);
        } else if history.loaded
            && let Some((existing, incoming)) =
                messages.iter().enumerate().find_map(|(index, message)| {
                    page.messages
                        .iter()
                        .position(|item| item.id == message.id)
                        .map(|incoming| (index, incoming))
                })
        {
            // Reconcile a completed background turn against the durable IDs,
            // keeping older pages and the reader's existing scroll anchor.
            let previous_count = messages.len();
            messages.truncate(existing);
            messages.extend(page.messages.into_iter().skip(incoming));
            history.list.splice(
                (existing + 1)..(previous_count + 1),
                messages.len() - existing,
            );
            retained_cursor = true;
        } else {
            *messages = page.messages;
            history.list.reset(0);
        }
        if !retained_cursor {
            history.before = page.before;
            history.has_more = page.has_more;
        }
        history.stale = false;
        history.loading = false;
        history.loaded = true;
        if active {
            self.sync_history_list(cx);
        }
        cx.notify();
    }
    pub(super) fn sync_history_list(&mut self, cx: &mut Context<Self>) {
        let count = 1
            + self.messages.len()
            + self.turns.get(&self.session).map_or(0, |turn| {
                turn.tools.len()
                    + usize::from(
                        !turn.text.is_empty() && (turn.running || turn.status == "Cancelled"),
                    )
                    + usize::from(turn.running)
            });
        let history = self
            .histories
            .entry(self.session)
            .or_insert_with(ChatHistory::new);
        let old = history.list.item_count();
        if count != old {
            let shared = count.min(old);
            history.list.splice(shared..old, count - shared);
        }
        // Streamed text and expanded action details change height in place.
        let first = (1 + self.messages.len()).min(count);
        history.list.remeasure_items(first..count);
        let weak = cx.entity().downgrade();
        let session = self.session;
        history.list.set_scroll_handler(move |event, _, cx| {
            if event.is_scrolled && event.visible_range.start <= 2 {
                let _ = weak.update(cx, |view, cx| {
                    if view.session == session {
                        view.load_older(cx);
                    }
                });
            }
        });
    }
}

impl Workbench {
    pub(super) fn open_history_message(
        &mut self,
        conversation: Conversation,
        anchor: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.recovery_before_switch(cx) {
            return;
        }
        if self.editor_dirty {
            self.error = Some("Save your changes before opening another conversation.".into());
            cx.notify();
            return;
        }
        if !self.draft_before_switch(cx) {
            return;
        }
        self.stash_history();
        self.set_workspace(conversation.workspace.clone(), window, cx);
        if self.workspace.as_ref() != Some(&conversation.workspace) {
            return;
        }
        self.session = conversation.id;
        self.recovery_scope_changed(cx);
        self.draft_switch_deferred(cx);
        self.page = Page::Chat;
        self.settings_open = false;
        self.messages = self
            .histories
            .get(&self.session)
            .map(|history| history.messages.clone())
            .unwrap_or_default();
        self.histories
            .entry(self.session)
            .or_insert_with(ChatHistory::new)
            .highlight = Some(anchor);
        self.request_message_window(anchor, cx);
        self.recovery_activate(window, cx);
        self.persist_ui(cx);
    }
    fn request_message_window(&mut self, anchor: Uuid, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        let session = self.session;
        let history = self
            .histories
            .entry(session)
            .or_insert_with(ChatHistory::new);
        history.windowed = true;
        history.pending_anchor = Some((anchor, workspace.clone()));
        history.pending_after = None;
        history.loading = true;
        history.older_request = false;
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.history_window(session, anchor, 200));
        if result.is_err() {
            history.loading = false;
            history.pending_anchor = None;
        }
        self.result(result, cx);
        cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(10))
                .await;
            let _ = view.update(cx, |view, cx| {
                if let Some(history) = view.histories.get_mut(&session)
                    && history.pending_anchor.as_ref() == Some(&(anchor, workspace))
                {
                    history.pending_anchor = None;
                    history.loading = false;
                    if view.session == session {
                        view.error =
                            Some("The conversation could not be opened. Try again.".into());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }
    pub(super) fn history_window_result(&mut self, page: HistoryWindow, cx: &mut Context<Self>) {
        if page.session_id != self.session {
            return;
        }
        let Some(history) = self.histories.get_mut(&page.session_id) else {
            return;
        };
        let current_workspace = self.workspace.as_ref();
        let matches = if page.append {
            history
                .pending_after
                .as_ref()
                .is_some_and(|(after, workspace)| {
                    Some(*after) == page.request_after && Some(workspace) == current_workspace
                })
        } else {
            history
                .pending_anchor
                .as_ref()
                .is_some_and(|(anchor, workspace)| {
                    Some(*anchor) == page.anchor && Some(workspace) == current_workspace
                })
        };
        if !matches {
            return;
        }
        history.pending_anchor = None;
        history.pending_after = None;
        history.loading = false;
        history.older_request = false;
        history.windowed = true;
        history.loaded = true;
        history.stale = false;
        let focus;
        if page.append && self.messages.len() + page.messages.len() <= 400 {
            let existing: BTreeSet<_> = self.messages.iter().map(|message| message.id).collect();
            focus = self.messages.len();
            self.messages.extend(
                page.messages
                    .into_iter()
                    .filter(|message| !existing.contains(&message.id)),
            );
        } else {
            focus = page
                .anchor
                .and_then(|anchor| {
                    page.messages
                        .iter()
                        .position(|message| message.id == anchor)
                })
                .unwrap_or(0);
            self.messages = page.messages;
            history.before = page.before;
            history.has_more = page.has_older;
            history.list.reset(0);
        }
        history.after = page.after.or(history.after);
        history.has_newer = page.has_newer;
        history.messages = self.messages.clone();
        self.sync_history_list(cx);
        if let Some(history) = self.histories.get(&self.session) {
            // A new centered window contains unmeasured rows. Scrolling by its
            // stable item index reveals the source immediately and pauses tail
            // following; a height-based reveal could resolve to the old tail.
            history.list.scroll_to(ListOffset {
                item_ix: (focus + 1).min(self.messages.len()),
                offset_in_item: px(0.),
            });
        }
        cx.notify();
    }
    pub(super) fn load_later(&mut self, cx: &mut Context<Self>) {
        let Some(history) = self.histories.get_mut(&self.session) else {
            return;
        };
        if history.loading || !history.has_newer {
            return;
        }
        let (Some(after), Some(workspace)) = (history.after, self.workspace.clone()) else {
            return;
        };
        history.pending_after = Some((after, workspace.clone()));
        history.loading = true;
        let session = self.session;
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.history_after(session, after, 100));
        if result.is_err() {
            history.pending_after = None;
            history.loading = false;
        }
        self.result(result, cx);
        cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(10))
                .await;
            let _ = view.update(cx, |view, cx| {
                if let Some(history) = view.histories.get_mut(&session)
                    && history.pending_after.as_ref() == Some(&(after, workspace))
                {
                    history.pending_after = None;
                    history.loading = false;
                    if view.session == session {
                        view.error = Some("Later messages could not be loaded. Try again.".into());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }
    pub(super) fn return_to_latest(&mut self, cx: &mut Context<Self>) {
        if let Some(history) = self.histories.get_mut(&self.session) {
            history.windowed = false;
            history.pending_anchor = None;
            history.pending_after = None;
            history.highlight = None;
            history.loading = false;
            history.loaded = false;
            history.list.set_follow_mode(FollowMode::Tail);
        }
        self.request_history(false, cx);
    }
    pub(super) fn history_message_highlight(&self, id: Uuid) -> bool {
        self.histories
            .get(&self.session)
            .is_some_and(|history| history.highlight == Some(id))
    }
    #[cfg(test)]
    #[allow(dead_code)] // Used by the separate native validation crate.
    pub(super) fn validation_history_window(&self) -> (bool, Option<i64>, bool) {
        self.histories
            .get(&self.session)
            .map(|history| (history.windowed, history.after, history.has_newer))
            .unwrap_or_default()
    }
    pub(super) fn history_later_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(history) = self
            .histories
            .get(&self.session)
            .filter(|history| history.windowed)
        else {
            return div().into_any_element();
        };
        div()
            .px_4()
            .py_2()
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .child(subtitle(if history.loading {
                "Loading saved messages…"
            } else {
                "Viewing saved messages"
            }))
            .child(
                button("later-messages", "Load later messages")
                    .disabled(history.loading || !history.has_newer)
                    .on_click(cx.listener(|view, _, _, cx| view.load_later(cx))),
            )
            .child(
                button("latest-messages", "Latest messages")
                    .disabled(history.loading)
                    .on_click(cx.listener(|view, _, _, cx| view.return_to_latest(cx))),
            )
            .into_any_element()
    }
}
