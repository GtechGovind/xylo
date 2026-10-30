//! Native, debounced database-wide conversation search. Search pages are bounded,
//! request-correlated and independent of the sidebar's loaded metadata cache.
use super::*;
use xylo_runtime::history::{HistoryMatch, HistorySearchPage};

struct PendingSearch {
    id: Uuid,
    query: String,
    workspace: Option<PathBuf>,
}
pub(super) struct HistorySearchUi {
    items: Vec<HistoryMatch>,
    before: Option<ConversationCursor>,
    has_more: bool,
    request: Option<PendingSearch>,
    debounce: Option<Task<()>>,
    generation: u64,
    indexing: bool,
    index_ready: bool,
    notice: String,
    scroll: UniformListScrollHandle,
}
impl HistorySearchUi {
    pub(super) fn new(_window: &mut Window, _cx: &mut Context<Xylo>) -> Self {
        Self {
            items: Vec::new(),
            before: None,
            has_more: false,
            request: None,
            debounce: None,
            generation: 0,
            indexing: false,
            index_ready: false,
            notice: String::new(),
            scroll: UniformListScrollHandle::new(),
        }
    }
    pub(super) fn reset(&mut self) {
        self.items.clear();
        self.before = None;
        self.has_more = false;
        self.request = None;
        self.debounce = None;
        self.generation = self.generation.wrapping_add(1);
        self.notice.clear();
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
}
impl Xylo {
    pub(super) fn history_search_changed(&mut self, cx: &mut Context<Self>) {
        self.history_search.reset();
        let query = self.conversation_query.read(cx).value().trim().to_string();
        if query.is_empty() {
            cx.notify();
            return;
        }
        self.settings_open = false;
        self.page = Page::HistorySearch;
        self.history_search.notice = "Searching conversations…".into();
        let generation = self.history_search.generation;
        self.history_search.debounce = Some(cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(220))
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.history_search.generation == generation {
                    view.run_history_search(false, cx);
                }
            });
        }));
        cx.notify();
    }
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
        if result.is_ok() {
            self.history_search.request = Some(PendingSearch {
                id,
                query,
                workspace: self.workspace.clone(),
            });
            self.history_search.notice = "Searching conversations…".into();
        }
        self.result(result, cx);
    }
    pub(super) fn history_search_result(
        &mut self,
        id: Uuid,
        result: std::result::Result<HistorySearchPage, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = &self.history_search.request else {
            return;
        };
        if pending.id != id {
            return;
        }
        let current = self.conversation_query.read(cx).value().trim().to_string();
        if pending.query != current || pending.workspace != self.workspace {
            self.history_search.request = None;
            return;
        }
        self.history_search.request = None;
        match result {
            Ok(page) if page.query == current => {
                self.history_search.items = page.items;
                self.history_search.before = page.before;
                self.history_search.has_more = page.has_more;
                self.history_search.indexing = page.indexing;
                self.history_search.notice = if page.indexing {
                    "Searching more saved conversations…".into()
                } else if self.history_search.items.is_empty() {
                    "No matching conversations.".into()
                } else {
                    format!(
                        "{} matching conversations on this page",
                        self.history_search.items.len()
                    )
                };
                if page.indexing && self.history_search.index_ready {
                    self.run_history_search(false, cx);
                }
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(detail=%error,"History search failed");
                self.history_search.notice =
                    "Conversations could not be searched. Try again.".into();
                self.history_search.has_more = false;
            }
        }
        cx.notify();
    }
    pub(super) fn history_index_ready(&mut self, cx: &mut Context<Self>) {
        self.history_search.index_ready = true;
        if self.history_search.indexing
            && self.history_search.request.is_none()
            && !self.conversation_query.read(cx).value().trim().is_empty()
        {
            self.run_history_search(false, cx);
        }
    }
    pub(super) fn history_search_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.entity().downgrade();
        let list = uniform_list(
            "history-search-results",
            self.history_search.items.len(),
            move |range, _, cx| {
                weak.update(cx, |view, cx| {
                    range
                        .filter_map(|index| {
                            let hit = view.history_search.items.get(index)?.clone();
                            let title = if hit.conversation.title.trim().is_empty() {
                                "Untitled conversation".into()
                            } else {
                                hit.conversation.title.clone()
                            };
                            let project = hit
                                .conversation
                                .workspace
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "Project".into());
                            let snippet = hit
                                .snippet
                                .split_whitespace()
                                .collect::<Vec<_>>()
                                .join(" ")
                                .chars()
                                .take(240)
                                .collect::<String>();
                            Some(
                                button(("history-match", index), title.clone())
                                    .accessibility_label(format!(
                                        "Open matching conversation: {title}"
                                    ))
                                    .h(px(84.))
                                    .w_full()
                                    .justify_start()
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .items_start()
                                            .gap_1()
                                            .overflow_hidden()
                                            .child(subtitle(project))
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(rgb(muted()))
                                                    .child(snippet),
                                            ),
                                    )
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        if let Some(anchor) = hit.message_id {
                                            view.open_history_message(
                                                hit.conversation.clone(),
                                                anchor,
                                                window,
                                                cx,
                                            );
                                        } else {
                                            view.open_conversation(
                                                hit.conversation.id,
                                                hit.conversation.workspace.clone(),
                                                window,
                                                cx,
                                            );
                                        }
                                    }))
                                    .into_any_element(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
            },
        )
        .track_scroll(&self.history_search.scroll)
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
                "Search conversations",
                "Find words in saved conversations and open the matching message.",
            ))
            .child(subtitle(self.history_search.notice.clone()))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        button("history-search-refresh", "Search again")
                            .on_click(cx.listener(|view, _, _, cx| view.history_search_submit(cx))),
                    )
                    .child(
                        button("history-search-focus", "Edit search").on_click(cx.listener(
                            |view, _, window, cx| {
                                view.search_open = true;
                                view.conversation_query
                                    .update(cx, |input, cx| input.focus(window, cx));
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(list)
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        button("history-search-first", "First results")
                            .disabled(self.history_search.request.is_some())
                            .on_click(
                                cx.listener(|view, _, _, cx| view.run_history_search(false, cx)),
                            ),
                    )
                    .child(
                        button("history-search-next", "Next results")
                            .disabled(
                                !self.history_search.has_more
                                    || self.history_search.request.is_some(),
                            )
                            .on_click(
                                cx.listener(|view, _, _, cx| view.run_history_search(true, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }
}
