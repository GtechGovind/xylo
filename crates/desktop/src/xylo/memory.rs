//! Request-correlated fact management. Mutations keep their input until the
//! durable host receipt; late reads never populate a different project/query.
use super::settings_ui;
use super::*;
use crate::motion;
use xylo_runtime::{MemoryCursor, MemoryExport, MemoryMutation, MemoryPage};

struct Query {
    id: Uuid,
    scope: String,
    text: String,
}
pub(super) struct Mutation {
    pub(super) id: Uuid,
    pub(super) record: MemoryRecord,
    pub(super) scope: String,
    pub(super) input: String,
    pub(super) dispatched: bool,
}
#[derive(Default)]
pub(super) struct MemoryUi {
    composing: bool,
    query: Option<Query>,
    pub(super) mutation: Option<Mutation>,
    pub(super) retry: Option<MemoryRecord>,
    before: Option<MemoryCursor>,
    has_more: bool,
    pub(super) selected: Option<Uuid>,
    pub(super) editing: Option<Uuid>,
    pub(super) export: Option<(Uuid, String)>,
    pub(super) notice: String,
    scroll: UniformListScrollHandle,
    /// When the pending memory read was sent, for the slow-wait caption.
    loading_since: Option<std::time::Instant>,
    pub(super) forms: super::memory_form::MemoryForms,
    pub(super) versions: super::memory_versions::VersionUi,
}
impl MemoryUi {
    pub(super) fn busy(&self) -> bool {
        self.mutation.is_some() || self.versions.mutating()
    }
    pub(super) fn status_message(&self) -> &str {
        &self.notice
    }
    fn reset_scope(&mut self) {
        self.composing = false;
        self.query = None;
        self.before = None;
        self.has_more = false;
        self.selected = None;
        self.editing = None;
        self.retry = None;
        self.notice.clear();
    }
}
impl Xylo {
    pub(super) fn memory_before_switch(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.memory_form_before_switch(cx) {
            return false;
        }
        self.cancel_memory_inspection(cx);
        true
    }
    pub(super) fn memory_after_switch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.facts.reset_scope();
        self.memory_form_activate(window, cx);
    }
    pub(super) fn search_memory(&mut self, cx: &mut Context<Self>) {
        self.request_memories(false, cx);
    }
    fn request_memories(&mut self, append: bool, cx: &mut Context<Self>) {
        let id = Uuid::new_v4();
        let scope = self.scope();
        let text = self.memory_query.read(cx).value().trim().to_owned();
        let before = if append {
            self.facts.before.clone()
        } else {
            None
        };
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.memory_search(id, scope.clone(), text.clone(), before));
        if result.is_ok() {
            self.facts.query = Some(Query { id, scope, text });
            self.facts.loading_since = Some(self.arm_slow_hint(cx));
            self.facts.notice = "Loading memories…".into();
        }
        self.result(result, cx);
    }
    pub(super) fn memory_received(
        &mut self,
        id: Uuid,
        result: Result<MemoryPage, String>,
        cx: &mut Context<Self>,
    ) {
        if self.facts.query.as_ref().is_none_or(|query| query.id != id) {
            return;
        }
        let query = self.facts.query.take().unwrap();
        self.facts.loading_since = None;
        if query.scope != self.scope() || query.text != self.memory_query.read(cx).value().trim() {
            return;
        }
        match result {
            Ok(page) if page.scope == query.scope && page.query.trim() == query.text => {
                // Keep one bounded page even after browsing a large archive.
                self.memories.clear();
                self.facts.scroll = UniformListScrollHandle::new();
                for record in page.records {
                    if !self.memories.iter().any(|saved| saved.id == record.id) {
                        self.memories.push(record);
                    }
                }
                self.facts.before = page.before;
                self.facts.has_more = page.has_more;
                self.facts.notice = if self.memories.is_empty() {
                    "No saved memories match this view.".into()
                } else if !query.text.is_empty() && self.memories.len() == 100 {
                    "Showing the best 100 matches. Refine your search for more specific results."
                        .into()
                } else {
                    String::new()
                };
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error,"Load project memories");
                self.facts.notice="Memories could not be loaded. Try again, or check Downloads if search needs setup.".into();
            }
        }
        cx.notify();
    }
    fn save_memory(&mut self, cx: &mut Context<Self>) {
        if self.facts.busy()
            || !self.memory_form_ready()
            || (self.workspace.is_some() && !self.workspace_ready)
        {
            return;
        }
        let input = self.memory_content.read(cx).value().to_string();
        let content = input.trim().to_owned();
        if content.is_empty() || content.len() > 262144 {
            return;
        }
        let scope = self.scope();
        let supersedes = self.facts.editing;
        let record = self
            .facts
            .retry
            .as_ref()
            .filter(|retry| {
                retry.scope == scope && retry.content == content && retry.supersedes == supersedes
            })
            .cloned()
            .unwrap_or_else(|| MemoryRecord {
                id: Uuid::new_v4(),
                scope: scope.clone(),
                content,
                source: if supersedes.is_some() {
                    "Explicit correction in Memory"
                } else {
                    "Explicit entry in Memory"
                }
                .into(),
                created_at: now_ms(),
                supersedes,
            });
        self.facts.retry = Some(record.clone());
        self.facts.mutation = Some(Mutation {
            id: Uuid::new_v4(),
            record: record.clone(),
            scope: scope.clone(),
            input,
            dispatched: false,
        });
        self.facts.notice = "Saving your entry before adding it to Memory…".into();
        self.memory_form_changed(cx);
        self.persist_memory_form(&scope, cx);
        if self.durable_memory_retry(&scope, record.id) {
            self.dispatch_memory_mutation(&scope, record.id, cx);
        }
        cx.notify();
    }
    pub(super) fn dispatch_memory_mutation(
        &mut self,
        scope: &str,
        target: Uuid,
        cx: &mut Context<Self>,
    ) {
        let Some(operation) = self.facts.mutation.as_mut().filter(|operation| {
            operation.scope == scope && operation.record.id == target && !operation.dispatched
        }) else {
            return;
        };
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.memory_remember(operation.id, operation.record.clone()));
        if result.is_ok() {
            operation.dispatched = true;
            self.facts.notice = "Saving memory…".into();
        } else {
            self.facts.mutation = None;
            self.facts.notice =
                "Your entry remains saved as an unsaved entry. Choose Save to retry.".into();
        }
        self.result(result, cx);
    }
    pub(super) fn memory_mutated(
        &mut self,
        id: Uuid,
        result: Result<MemoryMutation, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .facts
            .mutation
            .as_ref()
            .is_none_or(|operation| operation.id != id)
        {
            return;
        }
        let operation = self.facts.mutation.take().unwrap();
        match result {
            Ok(receipt)
                if receipt.scope == operation.scope
                    && receipt.id == operation.record.id
                    && !receipt.deleted =>
            {
                self.settle_memory_form(
                    &operation.scope,
                    &operation.record,
                    &operation.input,
                    window,
                    cx,
                );
                if operation.scope == self.scope() {
                    if self.memory_content.read(cx).value().trim().is_empty()
                        && self.facts.editing.is_none()
                    {
                        self.facts.composing = false;
                    }
                    self.facts.selected = None;
                    self.search_memory(cx);
                }
            }
            _ => {
                if operation.scope == self.scope() {
                    self.facts.notice="Memory could not be updated. Your entry remains here. Refresh and try again.".into();
                }
            }
        }
        self.advance_memory_flush(window, cx);
        cx.notify();
    }
    pub(super) fn memory_exported(
        &mut self,
        id: Uuid,
        result: Result<MemoryExport, String>,
        cx: &mut Context<Self>,
    ) {
        if self
            .facts
            .export
            .as_ref()
            .is_none_or(|(expected, _)| *expected != id)
        {
            return;
        }
        let (_, scope) = self.facts.export.take().unwrap();
        if scope != self.scope() {
            return;
        }
        self.facts.notice = match result {
            Ok(export) => format!(
                "Exported {} memory versions to {}",
                export.records,
                export.path.display()
            ),
            Err(error) => {
                tracing::warn!(%error,"Export project memories");
                "Memories could not be exported. Choose a writable location and try again.".into()
            }
        };
        cx.notify();
    }
    /// Memory, its detail sub-page or the version archive, depending on
    /// what the person opened.
    pub(super) fn memory_page(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(id) = self.facts.selected {
            return self.memory_detail_page(id, cx);
        }
        if self.facts.versions.archive_open() {
            return self.memory_archive_page(cx);
        }
        self.memory_list_page(cx)
    }
    /// Whether the entry form has something to show: a new entry being
    /// composed, a correction, or a save waiting for retry.
    fn memory_form_visible(&self, cx: &Context<Self>) -> bool {
        self.facts.composing
            || self.facts.editing.is_some()
            || self.facts.retry.is_some()
            || !self.memory_content.read(cx).value().trim().is_empty()
    }
    /// Starts correcting `record`: its text fills the form, then its detail
    /// page opens with the correction form.
    pub(super) fn begin_memory_correction(
        &mut self,
        record: MemoryRecord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.memory_content.read(cx).value().trim().is_empty() {
            if self.facts.editing != Some(record.id) {
                self.facts.notice =
                    "Save or clear your unsaved Memory entry before correcting a different fact."
                        .into();
                cx.notify();
            }
            return;
        }
        self.facts.editing = Some(record.id);
        self.facts.retry = None;
        self.memory_content.update(cx, |input, cx| {
            input.set_value(record.content.clone(), window, cx)
        });
        self.memory_form_changed(cx);
        cx.notify();
    }
    /// The entry form: the text area, draft status, then Cancel and Save.
    /// A correction cancels back to no edit; a new entry closes the form.
    pub(super) fn memory_form_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.facts.busy() || (self.workspace.is_some() && !self.workspace_ready);
        let correcting = self.facts.editing.is_some();
        let mut actions: Vec<AnyElement> = Vec::new();
        if correcting {
            actions.push(
                settings_ui::text_button("memory-cancel-edit", "Cancel correction")
                    .disabled(busy)
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.memory_content
                            .update(cx, |input, cx| input.set_value("", window, cx));
                        view.facts.composing = false;
                        view.facts.editing = None;
                        view.facts.retry = None;
                        view.memory_form_changed(cx);
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        } else if self.memory_content.read(cx).value().trim().is_empty() {
            actions.push(
                settings_ui::text_button("memory-cancel-new", "Cancel")
                    .disabled(busy)
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.facts.composing = false;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        actions.push(
            settings_ui::primary_button(
                "memory-add",
                if correcting {
                    "Save correction"
                } else {
                    "Save memory"
                },
            )
            .disabled(busy || !self.memory_form_ready())
            .on_click(cx.listener(|view, _, _, cx| view.save_memory(cx)))
            .map(|button| settings_ui::busy(button, self.facts.mutation.is_some()))
            .into_any_element(),
        );
        settings_ui::section(
            if correcting {
                "Correction"
            } else {
                "New memory"
            },
            Some(if correcting {
                "The correction becomes the current version; the earlier one stays in the history."
                    .into()
            } else {
                "Your text is kept as a draft until it is saved.".into()
            }),
            settings_ui::form_column([
                settings_ui::field(
                    "Memory",
                    Textarea::new(&self.memory_content)
                        .accessibility_id("memory-entry")
                        .aria_label("Unsaved Memory entry")
                        .disabled(self.workspace.is_some() && !self.workspace_ready),
                    None,
                )
                .into_any_element(),
                self.memory_form_status(cx),
                settings_ui::form_actions(actions).into_any_element(),
            ]),
        )
        .into_any_element()
    }
    /// The page's one primary action: open the new-entry form.
    fn memory_new_button(&self, busy: bool, cx: &mut Context<Self>) -> gpui_kit::base::Button {
        settings_ui::primary_button("memory-new", "Add memory")
            .disabled(busy || !self.memory_form_ready())
            .on_click(cx.listener(|view, _, window, cx| {
                view.facts.composing = true;
                view.memory_content
                    .update(cx, |input, cx| input.focus(window, cx));
                cx.notify();
            }))
    }
    fn memory_list_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.facts.busy() || (self.workspace.is_some() && !self.workspace_ready);
        let show_form = self.memory_form_visible(cx);
        let mut page = settings_ui::settings_page("memory-settings", "Memory").description(
            "Facts Xylo remembers about this project and uses when they are relevant.",
        );
        let empty = self.memories.is_empty();
        let loading = self.facts.query.is_some();
        let searching = !self.memory_query.read(cx).value().trim().is_empty();
        // Nothing saved yet: the page is only the empty state (or the new
        // entry form), without search, archive or danger zone. A search with
        // no results keeps the search field.
        let nothing_saved = empty && !loading && !searching;
        if !show_form && !nothing_saved {
            page = page.action(self.memory_new_button(busy, cx));
        }
        if show_form {
            page = page.child(self.memory_form_section(cx));
        } else if self.memory_form_has_status() {
            page = page.child(self.memory_form_status(cx));
        }
        if nothing_saved {
            if !show_form {
                page = page.child(settings_ui::empty_state(
                    AppIcon::Memory,
                    "No memories yet",
                    "Add facts you want Xylo to keep in mind in this project.",
                    Some(self.memory_new_button(busy, cx).into_any_element()),
                ));
            }
            return page.into_any_element();
        }
        let search = div()
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(div().flex_1().min_w_0().child(settings_ui::search_field(
                &self.memory_query,
                "Search memory",
            )))
            // Search runs as the person types; the spinner marks a search in
            // flight.
            .when(loading, |row| row.child(motion::spinner_inline()));
        let mut saved = div().w_full().flex().flex_col().gap(px(12.)).child(search);
        let status = self.facts.status_message();
        if empty && !loading {
            saved = saved.child(settings_ui::empty_state(
                AppIcon::Memory,
                "No matching memories",
                "Try different words, or add the fact you were looking for.",
                None,
            ));
        } else {
            if !status.is_empty() && !loading {
                saved = saved.child(settings_ui::caption(status.to_owned()));
            }
            if empty {
                saved = saved.child(settings_ui::loading_list(3, self.facts.loading_since));
            } else {
                saved = saved.child(self.memory_list(cx));
            }
        }
        let mut paging: Vec<AnyElement> = Vec::new();
        if self.facts.before.is_some() {
            paging.push(
                settings_ui::text_button("memory-first", "First memories")
                    .disabled(self.facts.query.is_some())
                    .on_click(cx.listener(|view, _, _, cx| view.request_memories(false, cx)))
                    .into_any_element(),
            );
        }
        if self.facts.has_more {
            paging.push(
                settings_ui::text_button("memory-more", "Next memories")
                    .disabled(self.facts.query.is_some())
                    .on_click(cx.listener(|view, _, _, cx| view.request_memories(true, cx)))
                    .into_any_element(),
            );
        }
        if !paging.is_empty() {
            saved = saved.child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(8.))
                    .children(paging),
            );
        }
        page.child(settings_ui::section("Saved memories", None, saved))
            .child(self.memory_archive_controls(cx))
            .into_any_element()
    }
    /// Current memories, virtualized: each opens its detail page, which holds
    /// Correct, the version history and Forget.
    fn memory_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.entity().downgrade();
        let count = self.memories.len();
        let rows = uniform_list("saved-memory-list", count, move |range, _, cx| {
            weak.update(cx, |view, cx| {
                range
                    .filter_map(|index| {
                        let record = view.memories.get(index)?.clone();
                        let id = record.id;
                        Some(
                            settings_ui::list_item(
                                ("memory-select", index),
                                None,
                                memory_title(&record.content),
                                Some(
                                    format!(
                                        "{} · {}",
                                        memory_age(record.created_at),
                                        record.source
                                    )
                                    .into(),
                                ),
                            )
                            .tags([scope_label(&record.scope)])
                            .divider(index > 0)
                            .on_click(
                                cx.listener(move |view, _, _, cx| view.open_fact_versions(id, cx)),
                            )
                            .into_any_element(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
        })
        .track_scroll(&self.facts.scroll)
        .h(px(
            (count as f32 * settings_ui::LIST_ITEM_HEIGHT).min(8. * settings_ui::LIST_ITEM_HEIGHT)
        ))
        .w_full();
        settings_ui::list_frame().child(rows).into_any_element()
    }
}

/// The first line of a memory, short enough for a list title.
pub(super) fn memory_title(content: &str) -> String {
    let line = content.trim().lines().next().unwrap_or_default().trim();
    let mut title: String = line.chars().take(80).collect();
    if line.chars().count() > 80 {
        title.push('…');
    }
    if title.is_empty() {
        "Untitled memory".into()
    } else {
        title
    }
}
/// The scope tag on a memory: "All projects" or the project folder name.
pub(super) fn scope_label(scope: &str) -> SharedString {
    if scope == "global" {
        return "All projects".into();
    }
    Path::new(scope)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| scope.to_owned())
        .into()
}

pub(super) fn memory_age(saved_at: i64) -> String {
    let seconds = now_ms().saturating_sub(saved_at).max(0) / 1000;
    match seconds {
        0..=59 => "Saved just now".into(),
        60..=3599 => format!("Saved {} minutes ago", seconds / 60),
        3600..=86399 => format!("Saved {} hours ago", seconds / 3600),
        _ => format!("Saved {} days ago", seconds / 86400),
    }
}

#[cfg(test)]
#[path = "memory/tests.rs"]
mod tests;
