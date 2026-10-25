//! Request-correlated fact management. Mutations keep their input until the
//! durable host receipt; late reads never populate a different project/query.
use super::*;
use workbench_runtime::{MemoryCursor, MemoryExport, MemoryMutation, MemoryPage};

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
        self.query = None;
        self.before = None;
        self.has_more = false;
        self.selected = None;
        self.editing = None;
        self.retry = None;
        self.notice.clear();
    }
}
impl Workbench {
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
    fn export_memory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.export_fact_selection(workbench_runtime::ExportSelection::Project, window, cx);
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
    pub(super) fn memory_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.facts.busy() || (self.workspace.is_some() && !self.workspace_ready);
        let weak = cx.entity().downgrade();
        let rows = uniform_list(
            "saved-memory-list",
            self.memories.len(),
            move |range, _, cx| {
                weak.update(cx, |view, cx| {
                    range
                        .filter_map(|i| {
                            let record = view.memories.get(i)?;
                            let id = record.id;
                            let preview: String = record.content.chars().take(160).collect();
                            Some(
                                div()
                                    .h(px(64.))
                                    .w_full()
                                    .p_2()
                                    .border_b_1()
                                    .border_color(rgb(border()))
                                    .child(
                                        button(("memory-select", i), preview)
                                            .w_full()
                                            .justify_start()
                                            .on_click(cx.listener(move |view, _, _, cx| {
                                                view.open_fact_versions(id, cx);
                                            })),
                                    )
                                    .into_any_element(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
            },
        )
        .track_scroll(&self.facts.scroll)
        .h(px(320.))
        .w_full();
        let mut page = div()
            .id("memory-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(heading(
                "Memory",
                "Decisions and preferences remembered for this project.",
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.memory_query)))
                    .child(
                        button("memory-search", "Search")
                            .on_click(cx.listener(|view, _, _, cx| view.search_memory(cx))),
                    )
                    .child(
                        button("memory-export", "Export project facts…")
                            .disabled(self.facts.export.is_some())
                            .on_click(
                                cx.listener(|view, _, window, cx| view.export_memory(window, cx)),
                            ),
                    ),
            )
            .child(
                card()
                    .child(
                        Textarea::new(&self.memory_content)
                            .accessibility_id("memory-entry")
                            .aria_label("Unsaved Memory entry")
                            .disabled(self.workspace.is_some() && !self.workspace_ready),
                    )
                    .child(
                        primary(
                            "memory-add",
                            if self.facts.editing.is_some() {
                                "Save correction"
                            } else {
                                "Remember for this project"
                            },
                        )
                        .disabled(busy || !self.memory_form_ready())
                        .on_click(cx.listener(|view, _, _, cx| view.save_memory(cx))),
                    )
                    .when(self.facts.editing.is_some(), |panel| {
                        panel.child(
                            button("memory-cancel-edit", "Cancel correction")
                                .disabled(busy)
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.facts.editing = None;
                                    view.facts.retry = None;
                                    view.memory_form_changed(cx);
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(self.memory_form_status(cx))
            .child(self.memory_archive_controls(cx))
            .child(subtitle(self.facts.status_message().to_owned()))
            .child(rows)
            .when(self.facts.has_more, |panel| {
                panel.child(
                    button("memory-more", "Next memories")
                        .disabled(self.facts.query.is_some())
                        .on_click(cx.listener(|view, _, _, cx| view.request_memories(true, cx))),
                )
            });
        page = page.child(self.memory_versions_panel(cx));
        page.child(self.learning_card(cx)).into_any_element()
    }
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
