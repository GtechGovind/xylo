//! Explicit scoped inspection, export and confirmed destruction of saved facts.
//! A preview revision fences changes made while a confirmation dialog is open.
use super::*;
use workbench_runtime::{
    ExportSelection, FactCursor, FactDeleteKind, FactDeletePreview, FactDeleteRequest,
    FactDeletion, FactPage, FactVersion,
};

struct Read {
    id: Uuid,
    scope: String,
    anchor: Option<Uuid>,
}
#[derive(Default)]
pub(super) struct VersionUi {
    read: Option<Read>,
    page: Option<FactPage>,
    detail: Option<FactVersion>,
    preview: Option<(Uuid, String, FactDeleteKind)>,
    deletion: Option<(Uuid, FactDeleteRequest)>,
    retry: Option<FactDeleteRequest>,
    prompt: Option<Uuid>,
    notice: String,
    scroll: UniformListScrollHandle,
}
impl VersionUi {
    pub(super) fn mutating(&self) -> bool {
        self.preview.is_some() || self.prompt.is_some() || self.deletion.is_some()
    }
}
impl Workbench {
    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn validation_fact_ready(&self) -> bool {
        self.facts.versions.read.is_none() && self.facts.versions.detail.is_some()
    }
    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn validation_fact_prompt(&self) -> bool {
        self.facts.versions.prompt.is_some()
    }
    pub(super) fn cancel_memory_inspection(&mut self, cx: &mut Context<Self>) {
        if let Some(host) = &self.host {
            if let Some(read) = self.facts.versions.read.take() {
                let _ = host.fact_cancel(read.id);
            }
            if let Some((id, _, _)) = self.facts.versions.preview.take() {
                let _ = host.fact_cancel(id);
            }
        }
        self.facts.versions.page = None;
        self.facts.versions.detail = None;
        self.facts.versions.prompt = None;
        self.facts.versions.notice.clear();
        cx.notify();
    }
    pub(super) fn open_fact_versions(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.facts.selected = Some(id);
        self.facts.versions.detail = None;
        self.request_fact_versions(Some(id), None, cx);
    }
    fn request_fact_versions(
        &mut self,
        anchor: Option<Uuid>,
        before: Option<FactCursor>,
        cx: &mut Context<Self>,
    ) {
        if let Some(host) = &self.host
            && let Some(read) = self.facts.versions.read.take()
        {
            let _ = host.fact_cancel(read.id);
        }
        let id = Uuid::new_v4();
        let scope = self.scope();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.fact_versions(id, scope.clone(), anchor, before));
        if result.is_ok() {
            self.facts.versions.read = Some(Read { id, scope, anchor });
            self.facts.versions.notice = "Loading saved versions…".into();
        }
        self.result(result, cx);
    }
    pub(super) fn fact_versions_received(
        &mut self,
        id: Uuid,
        result: Result<FactPage, String>,
        cx: &mut Context<Self>,
    ) {
        if self
            .facts
            .versions
            .read
            .as_ref()
            .is_none_or(|read| read.id != id)
        {
            return;
        }
        let read = self.facts.versions.read.take().unwrap();
        if read.scope != self.scope() {
            return;
        }
        match result {
            Ok(page) if page.scope == read.scope && page.anchor == read.anchor => {
                if page.anchor.is_some() {
                    self.facts.versions.detail = page.anchor_version.clone();
                }
                self.facts.versions.notice = if page.versions.is_empty() {
                    "No saved versions remain in this view."
                } else {
                    ""
                }
                .into();
                self.facts.versions.page = Some(page);
                self.facts.versions.scroll = UniformListScrollHandle::new();
            }
            _ => {
                self.facts.versions.notice =
                    "Saved versions changed or could not be loaded. Open the fact again to refresh."
                        .into()
            }
        }
        cx.notify();
    }
    fn preview_fact_delete(&mut self, kind: FactDeleteKind, cx: &mut Context<Self>) {
        if self.facts.busy() {
            return;
        }
        let id = Uuid::new_v4();
        let scope = self.scope();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.fact_delete_preview(id, scope.clone(), kind.clone()));
        if result.is_ok() {
            self.facts.versions.preview = Some((id, scope, kind));
            self.facts.versions.notice = "Checking which saved versions will be deleted…".into();
        }
        self.result(result, cx);
    }
    pub(super) fn fact_delete_previewed(
        &mut self,
        id: Uuid,
        result: Result<FactDeletePreview, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .facts
            .versions
            .preview
            .as_ref()
            .is_none_or(|(expected, _, _)| *expected != id)
        {
            return;
        }
        let (_, scope, kind) = self.facts.versions.preview.take().unwrap();
        if scope != self.scope() {
            return;
        }
        let preview = match result {
            Ok(preview) if preview.scope == scope && preview.kind == kind => preview,
            _ => {
                self.facts.versions.notice =
                    "The deletion could not be prepared. Refresh your facts and try again.".into();
                cx.notify();
                return;
            }
        };
        if preview.versions == 0 {
            self.facts.versions.notice = "No saved versions remain to delete.".into();
            cx.notify();
            return;
        }
        let (title, consequence) = match kind {
            FactDeleteKind::Version { .. } => (
                "Delete selected version?",
                "Only this version is deleted. Other versions remain saved and none is restored as current.",
            ),
            FactDeleteKind::Chain { .. } => (
                "Forget this entire fact?",
                "All saved versions of this fact are deleted.",
            ),
            FactDeleteKind::Project => (
                "Delete all project facts?",
                "All saved fact versions in this project are deleted.",
            ),
        };
        let body = format!(
            "{} saved version(s) in {}. {consequence}\n\nChat history, learned skills, checkpoints and unsaved Memory entries remain saved.",
            preview.versions, scope
        );
        let answer = window.prompt(
            PromptLevel::Warning,
            title,
            Some(&body),
            &[
                PromptButton::new("Delete saved versions"),
                PromptButton::cancel("Cancel"),
            ],
            cx,
        );
        self.facts.versions.prompt = Some(id);
        cx.spawn_in(window, async move |view, cx| {
            let confirmed = answer.await;
            let _ = view.update_in(cx, |view, _, cx| {
                if view.scope() != scope || view.facts.versions.prompt != Some(id) {
                    return;
                }
                view.facts.versions.prompt = None;
                if matches!(confirmed, Ok(0)) {
                    view.submit_fact_delete(
                        FactDeleteRequest {
                            operation_id: Uuid::new_v4(),
                            preview,
                        },
                        cx,
                    );
                } else {
                    view.facts.versions.notice = "Deletion cancelled.".into();
                    cx.notify();
                }
            });
        })
        .detach();
    }
    fn submit_fact_delete(&mut self, request: FactDeleteRequest, cx: &mut Context<Self>) {
        if request.preview.scope != self.scope() || self.facts.versions.deletion.is_some() {
            return;
        }
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.fact_delete(id, request.clone()));
        if result.is_ok() {
            self.facts.versions.deletion = Some((id, request));
            self.facts.versions.retry = None;
            self.facts.versions.notice = "Deleting saved versions…".into();
        }
        self.result(result, cx);
    }
    pub(super) fn fact_deleted(
        &mut self,
        id: Uuid,
        result: Result<FactDeletion, String>,
        cx: &mut Context<Self>,
    ) {
        if self
            .facts
            .versions
            .deletion
            .as_ref()
            .is_none_or(|(expected, _)| *expected != id)
        {
            return;
        }
        let (_, request) = self.facts.versions.deletion.take().unwrap();
        match result {
            Ok(receipt)
                if receipt.operation_id == request.operation_id
                    && receipt.scope == request.preview.scope
                    && receipt.kind == request.preview.kind
                    && receipt.removed == request.preview.versions =>
            {
                if receipt.scope == self.scope() {
                    self.facts.versions.detail = None;
                    self.facts.versions.page = None;
                    self.facts.selected = None;
                    self.search_memory(cx);
                    self.facts.versions.notice = format!(
                        "Deleted {} saved version(s). Other project data is unchanged.",
                        receipt.removed
                    );
                }
            }
            _ => {
                if request.preview.scope == self.scope() {
                    self.facts.versions.notice="Deletion could not be confirmed. Facts may have changed; refresh to review them, or retry the same request.".into();
                }
                self.facts.versions.retry = Some(request);
            }
        }
        cx.notify();
    }
    pub(super) fn export_fact_selection(
        &mut self,
        selection: ExportSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.facts.export.is_some() {
            return;
        }
        let scope = self.scope();
        cx.spawn_in(window, async move |view, cx| {
            let file = rfd::AsyncFileDialog::new()
                .set_title("Export saved fact versions")
                .set_file_name("memories.json")
                .add_filter("JSON", &["json"])
                .save_file()
                .await;
            if let Some(file) = file {
                let _ = view.update_in(cx, |view, _, cx| {
                    if view.scope() != scope {
                        return;
                    }
                    let id = Uuid::new_v4();
                    let result = view
                        .host
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
                        .and_then(|host| {
                            host.fact_export(id, scope.clone(), selection, file.path().to_owned())
                        });
                    if result.is_ok() {
                        view.facts.export = Some((id, scope));
                        view.facts.notice = "Exporting saved fact versions…".into();
                    }
                    view.result(result, cx);
                });
            }
        })
        .detach();
    }
    pub(super) fn memory_archive_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_wrap()
            .gap_2()
            .child(
                button("memory-archive", "Browse all saved versions")
                    .disabled(self.facts.versions.read.is_some())
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.facts.versions.detail = None;
                        view.request_fact_versions(None, None, cx);
                    })),
            )
            .child(
                button("memory-purge", "Delete all project facts…")
                    .disabled(self.facts.busy())
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.preview_fact_delete(FactDeleteKind::Project, cx)
                    })),
            )
            .into_any_element()
    }
    pub(super) fn memory_versions_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut panel = div().flex().flex_col().gap_3();
        let busy = self.facts.busy();
        if let Some(page) = &self.facts.versions.page {
            let weak = cx.entity().downgrade();
            panel = panel.child(subtitle(if page.anchor.is_some() {
                "Version history for this fact"
            } else {
                "All saved fact versions in this project"
            }));
            panel = panel.child(
                uniform_list(
                    "memory-version-list",
                    page.versions.len(),
                    move |range, _, cx| {
                        weak.update(cx, |view, cx| {
                            range
                                .filter_map(|i| {
                                    let version =
                                        view.facts.versions.page.as_ref()?.versions.get(i)?;
                                    let id = version.record.id;
                                    let preview: String =
                                        version.record.content.chars().take(120).collect();
                                    Some(
                                        div()
                                            .h(px(64.))
                                            .w_full()
                                            .border_b_1()
                                            .border_color(rgb(border()))
                                            .child(
                                                button(("memory-version", i), preview)
                                                    .w_full()
                                                    .justify_start()
                                                    .on_click(cx.listener(
                                                        move |view, _, _, cx| {
                                                            view.open_fact_versions(id, cx)
                                                        },
                                                    )),
                                            )
                                            .child(subtitle(format!(
                                                "{} · {}",
                                                if version.active {
                                                    "Current version"
                                                } else {
                                                    "Previous version"
                                                },
                                                super::memory::memory_age(
                                                    version.record.created_at
                                                )
                                            )))
                                            .into_any_element(),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                    },
                )
                .track_scroll(&self.facts.versions.scroll)
                .h(px(240.))
                .w_full(),
            );
            if page.has_more {
                let anchor = page.anchor;
                let before = page.before.clone();
                panel = panel.child(
                    button("memory-versions-more", "Next saved versions")
                        .disabled(self.facts.versions.read.is_some())
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.request_fact_versions(anchor, before.clone(), cx)
                        })),
                );
            }
        }
        if let Some(version) = &self.facts.versions.detail {
            let id = version.record.id;
            let record = version.record.clone();
            panel=panel.child(card().child(TextView::markdown("fact-version-detail",record.content.clone()).selectable(true))
                .child(subtitle(format!("{} · {} · {}",record.source,super::memory::memory_age(record.created_at),if version.active{"Current version"}else{"Previous version"})))
                .child(subtitle(format!("Project: {}",record.scope)))
                .when(record.supersedes.is_some(),|panel|panel.child(subtitle("This version corrects an earlier saved version.")))
                .child(div().flex().flex_wrap().gap_2()
                    .child(button("memory-edit","Correct current fact").disabled(busy||!version.active).on_click(cx.listener(move|view,_,window,cx| {
                        if !view.memory_content.read(cx).value().trim().is_empty() {
                            if view.facts.editing!=Some(record.id){view.facts.notice="Save or clear your unsaved Memory entry before correcting a different fact.".into();cx.notify();}
                            return;
                        }
                        view.facts.editing=Some(record.id);view.facts.retry=None;
                        view.memory_content.update(cx,|input,cx|input.set_value(record.content.clone(),window,cx));view.memory_form_changed(cx);cx.notify();
                    })))
                    .child(button("memory-forget","Delete selected version…").disabled(busy).on_click(cx.listener(move|view,_,_,cx|view.preview_fact_delete(FactDeleteKind::Version{id},cx))))
                    .child(button("memory-forget-chain","Forget entire fact…").disabled(busy).on_click(cx.listener(move|view,_,_,cx|view.preview_fact_delete(FactDeleteKind::Chain{id},cx))))
                    .child(button("memory-export-chain","Export this fact…").disabled(self.facts.export.is_some()).on_click(cx.listener(move|view,_,window,cx|view.export_fact_selection(ExportSelection::Chain{id},window,cx))))));
        }
        if self.facts.versions.read.is_some() {
            panel = panel.child(
                button("memory-versions-cancel", "Cancel loading versions")
                    .on_click(cx.listener(|view, _, _, cx| view.cancel_memory_inspection(cx))),
            );
        }
        if let Some((id, _)) = &self.facts.versions.deletion {
            let id = *id;
            panel=panel.child(button("memory-delete-cancel","Cancel deletion").on_click(cx.listener(move|view,_,_,cx|{
                if let Some(host)=&view.host{let _=host.fact_cancel(id);}
                view.facts.versions.notice="Cancellation requested. Refresh after the request finishes to review your saved facts.".into();cx.notify();
            })));
        }
        if let Some((id, scope)) = &self.facts.export
            && *scope == self.scope()
        {
            let id = *id;
            panel = panel.child(button("memory-export-cancel", "Cancel export").on_click(
                cx.listener(move |view, _, _, _| {
                    if let Some(host) = &view.host {
                        let _ = host.fact_cancel(id);
                    }
                }),
            ));
        }
        if let Some(request) = self
            .facts
            .versions
            .retry
            .as_ref()
            .filter(|request| request.preview.scope == self.scope())
        {
            let request = request.clone();
            panel = panel.child(
                button("memory-delete-retry", "Retry same deletion request")
                    .disabled(busy)
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.submit_fact_delete(request.clone(), cx)
                    })),
            );
        }
        panel
            .child(subtitle(self.facts.versions.notice.clone()))
            .into_any_element()
    }
}
