//! Explicit scoped inspection, export and confirmed destruction of saved facts.
//! A preview revision fences changes made while a confirmation dialog is open.
use super::settings_ui;
use super::*;
use xylo_runtime::{
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
    /// The archive sub-page (every saved version in the project) is open.
    archive: bool,
    /// When the pending version read was sent, for the slow-wait caption.
    read_since: Option<std::time::Instant>,
}
impl VersionUi {
    pub(super) fn mutating(&self) -> bool {
        self.preview.is_some() || self.prompt.is_some() || self.deletion.is_some()
    }
    /// Whether a deletion matching `kind` is being previewed, confirmed or
    /// carried out, so its button can show that it is working.
    fn deleting(&self, kind: &FactDeleteKind) -> bool {
        self.preview
            .as_ref()
            .is_some_and(|(_, _, pending)| pending == kind)
            || self
                .deletion
                .as_ref()
                .is_some_and(|(_, request)| &request.preview.kind == kind)
    }
    /// Whether the archive sub-page is showing.
    pub(super) fn archive_open(&self) -> bool {
        self.archive
    }
}
impl Xylo {
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
        self.facts.versions.archive = false;
        cx.notify();
    }
    /// Leaves the detail or archive sub-page for the Memory list.
    fn close_memory_inspection(&mut self, cx: &mut Context<Self>) {
        self.facts.selected = None;
        self.cancel_memory_inspection(cx);
    }
    pub(super) fn open_fact_versions(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.facts.selected = Some(id);
        self.facts.versions.archive = false;
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
            self.facts.versions.read_since = Some(self.arm_slow_hint(cx));
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
        self.facts.versions.read_since = None;
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
    pub(super) fn preview_fact_delete(&mut self, kind: FactDeleteKind, cx: &mut Context<Self>) {
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
                PromptButton::cancel("Cancel"),
                PromptButton::new("Delete saved versions"),
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
                if matches!(confirmed, Ok(1)) {
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
    /// The list page's archive and danger-zone sections.
    pub(super) fn memory_archive_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let archive = settings_ui::section(
            "Archive",
            None,
            settings_ui::rows([
                settings_ui::setting_row(
                    "Saved versions",
                    Some("Every saved version in this project, including corrected ones.".into()),
                    Some(
                        settings_ui::busy(
                            settings_ui::secondary_button("memory-archive", "Browse").on_click(
                                cx.listener(|view, _, _, cx| {
                                    view.facts.versions.detail = None;
                                    view.facts.versions.archive = true;
                                    view.request_fact_versions(None, None, cx);
                                }),
                            ),
                            self.facts.versions.read.is_some(),
                        )
                        .into_any_element(),
                    ),
                )
                .into_any_element(),
                settings_ui::setting_row(
                    "Export",
                    Some("Save every version to a JSON file.".into()),
                    Some(
                        settings_ui::busy(
                            settings_ui::secondary_button("memory-export", "Export…").on_click(
                                cx.listener(|view, _, window, cx| {
                                    view.export_fact_selection(ExportSelection::Project, window, cx)
                                }),
                            ),
                            self.facts.export.is_some(),
                        )
                        .into_any_element(),
                    ),
                )
                .into_any_element(),
            ]),
        );
        let danger = settings_ui::danger_zone([settings_ui::setting_row(
            "Delete all project facts",
            Some("Every saved version in this project is deleted. Chats and skills stay.".into()),
            Some(
                settings_ui::busy(
                    settings_ui::destructive_button("memory-purge", "Delete all…")
                        .disabled(self.facts.busy())
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.preview_fact_delete(FactDeleteKind::Project, cx)
                        })),
                    self.facts.versions.deleting(&FactDeleteKind::Project),
                )
                .into_any_element(),
            ),
        )
        .inset()
        .into_any_element()]);
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(settings_ui::SECTION_GAP))
            .child(self.memory_inspection_status(cx))
            .child(archive)
            .child(danger)
            .into_any_element()
    }
    /// Progress and recovery for version reads, deletions and exports: the
    /// latest notice with its Cancel or Retry action.
    fn memory_inspection_status(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.facts.busy();
        let mut actions: Vec<AnyElement> = Vec::new();
        if self.facts.versions.read.is_some() {
            actions.push(
                settings_ui::text_button("memory-versions-cancel", "Cancel loading")
                    .on_click(cx.listener(|view, _, _, cx| view.cancel_memory_inspection(cx)))
                    .into_any_element(),
            );
        }
        if let Some((id, _)) = &self.facts.versions.deletion {
            let id = *id;
            actions.push(
                settings_ui::text_button("memory-delete-cancel", "Cancel deletion")
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if let Some(host) = &view.host {
                            let _ = host.fact_cancel(id);
                        }
                        view.facts.versions.notice = "Cancellation requested. Refresh after the request finishes to review your saved facts.".into();
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        if let Some((id, scope)) = &self.facts.export
            && *scope == self.scope()
        {
            let id = *id;
            actions.push(
                settings_ui::text_button("memory-export-cancel", "Cancel export")
                    .on_click(cx.listener(move |view, _, _, _| {
                        if let Some(host) = &view.host {
                            let _ = host.fact_cancel(id);
                        }
                    }))
                    .into_any_element(),
            );
        }
        if let Some(request) = self
            .facts
            .versions
            .retry
            .as_ref()
            .filter(|request| request.preview.scope == self.scope())
        {
            let request = request.clone();
            actions.push(
                settings_ui::text_button("memory-delete-retry", "Retry deletion")
                    .disabled(busy)
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.submit_fact_delete(request.clone(), cx)
                    }))
                    .into_any_element(),
            );
        }
        let message = &self.facts.versions.notice;
        if message.is_empty() && actions.is_empty() {
            return div().into_any_element();
        }
        div()
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(settings_ui::caption(message.clone())),
            )
            .children(actions)
            .into_any_element()
    }
    /// The loaded versions as list items; each opens its own detail page.
    fn memory_version_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(page) = &self.facts.versions.page else {
            return div().into_any_element();
        };
        if page.versions.is_empty() {
            return div().into_any_element();
        }
        let count = page.versions.len();
        let weak = cx.entity().downgrade();
        let list = uniform_list("memory-version-list", count, move |range, _, cx| {
            weak.update(cx, |view, cx| {
                let shown = view
                    .facts
                    .versions
                    .detail
                    .as_ref()
                    .map(|version| version.record.id);
                range
                    .filter_map(|index| {
                        let version = view.facts.versions.page.as_ref()?.versions.get(index)?;
                        let id = version.record.id;
                        Some(
                            settings_ui::list_item(
                                ("memory-version", index),
                                None,
                                super::memory::memory_title(&version.record.content),
                                Some(super::memory::memory_age(version.record.created_at).into()),
                            )
                            .trailing(if version.active {
                                settings_ui::pill("Current", settings_ui::Tone::Success)
                            } else {
                                settings_ui::pill("Previous", settings_ui::Tone::Neutral)
                            })
                            .divider(index > 0)
                            .selected(shown == Some(id))
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
        .track_scroll(&self.facts.versions.scroll)
        .h(px(
            (count as f32 * settings_ui::LIST_ITEM_HEIGHT).min(6. * settings_ui::LIST_ITEM_HEIGHT)
        ))
        .w_full();
        let mut column = div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(settings_ui::list_frame().child(list));
        if page.has_more {
            let anchor = page.anchor;
            let before = page.before.clone();
            column = column.child(
                div().flex().justify_end().child(
                    settings_ui::text_button("memory-versions-more", "Older versions")
                        .disabled(self.facts.versions.read.is_some())
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.request_fact_versions(anchor, before.clone(), cx)
                        })),
                ),
            );
        }
        column.into_any_element()
    }
    fn memory_crumb(&self, cx: &mut Context<Self>) -> gpui_kit::base::Button {
        settings_ui::crumb("memory-crumb", "Memory")
            .on_click(cx.listener(|view, _, _, cx| view.close_memory_inspection(cx)))
    }
    /// One fact: its text and provenance, its version history, and the
    /// correction form or the actions that change it.
    pub(super) fn memory_detail_page(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.facts.busy();
        let detail = self.facts.versions.detail.as_ref();
        let title = detail
            .map(|version| super::memory::memory_title(&version.record.content))
            .or_else(|| {
                self.memories
                    .iter()
                    .find(|record| record.id == id)
                    .map(|record| super::memory::memory_title(&record.content))
            })
            .unwrap_or_else(|| "Memory".into());
        let mut page = settings_ui::settings_page("memory-detail", title.clone())
            .breadcrumb(settings_ui::breadcrumb([self.memory_crumb(cx)], title));
        let correcting = self.facts.editing.is_some();
        if let Some(version) = detail {
            let record = version.record.clone();
            let chain = record.id;
            page = page.action(
                settings_ui::secondary_button("memory-export-chain", "Export…")
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.export_fact_selection(ExportSelection::Chain { id: chain }, window, cx)
                    }))
                    .map(|button| settings_ui::busy(button, self.facts.export.is_some())),
            );
            if !correcting {
                let correction = record.clone();
                page = page.action(
                    settings_ui::primary_button("memory-edit", "Correct")
                        .disabled(busy || !version.active)
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.begin_memory_correction(correction.clone(), window, cx)
                        })),
                );
            }
        }
        page = page.child(self.memory_inspection_status(cx));
        if !self.facts.status_message().is_empty() {
            page = page.child(settings_ui::caption(self.facts.status_message().to_owned()));
        }
        let Some(version) = detail else {
            return page
                .child(settings_ui::loading_list(2, self.facts.versions.read_since))
                .into_any_element();
        };
        let record = &version.record;
        let about = vec![
            settings_ui::setting_row(
                "Status",
                record
                    .supersedes
                    .is_some()
                    .then(|| "This version corrects an earlier saved version.".into()),
                Some(
                    if version.active {
                        settings_ui::pill("Current version", settings_ui::Tone::Success)
                    } else {
                        settings_ui::pill("Previous version", settings_ui::Tone::Neutral)
                    }
                    .into_any_element(),
                ),
            )
            .into_any_element(),
            settings_ui::setting_row(
                "Saved",
                None,
                Some(
                    settings_ui::value_text(super::memory::memory_age(record.created_at))
                        .into_any_element(),
                ),
            )
            .into_any_element(),
            settings_ui::setting_row(
                "Source",
                None,
                Some(settings_ui::value_text(record.source.clone()).into_any_element()),
            )
            .into_any_element(),
            settings_ui::setting_row(
                "Origin",
                record
                    .origin
                    .untrusted()
                    .then(|| "Shaped by content from outside the project, so Xylo never recalls it on its own.".into()),
                Some(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .when(record.origin.untrusted(), |control| {
                            control.child(settings_ui::toned_tag(
                                super::memory::UNTRUSTED_TAG,
                                settings_ui::Tone::Warning,
                            ))
                        })
                        .child(settings_ui::value_text(super::memory::origin_text(record.origin)))
                        .into_any_element(),
                ),
            )
            .into_any_element(),
            settings_ui::setting_row(
                "Project",
                None,
                Some(
                    settings_ui::value_text(super::memory::scope_label(&record.scope))
                        .into_any_element(),
                ),
            )
            .into_any_element(),
        ];
        page = page.child(settings_ui::section(
            "Saved text",
            None,
            settings_ui::list_frame().p(px(16.)).child(
                TextView::markdown("fact-version-detail", record.content.clone()).selectable(true),
            ),
        ));
        if correcting {
            page = page.child(self.memory_form_section(cx));
        }
        let id = record.id;
        page.child(settings_ui::section(
            "Details",
            None,
            settings_ui::rows(about),
        ))
        .child(settings_ui::section(
            "Version history",
            Some("Open a version to read it or delete only that version.".into()),
            self.memory_version_list(cx),
        ))
        .child(settings_ui::danger_zone([
            settings_ui::setting_row(
                "Delete this version",
                Some("Other versions stay saved; none is restored as current.".into()),
                Some(
                    settings_ui::busy(
                        settings_ui::destructive_button("memory-forget", "Delete version…")
                            .disabled(busy)
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.preview_fact_delete(FactDeleteKind::Version { id }, cx)
                            })),
                        self.facts
                            .versions
                            .deleting(&FactDeleteKind::Version { id }),
                    )
                    .into_any_element(),
                ),
            )
            .inset()
            .into_any_element(),
            settings_ui::setting_row(
                "Forget this fact",
                Some("Every saved version of this fact is deleted.".into()),
                Some(
                    settings_ui::busy(
                        settings_ui::destructive_button("memory-forget-chain", "Forget…")
                            .disabled(busy)
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.preview_fact_delete(FactDeleteKind::Chain { id }, cx)
                            })),
                        self.facts.versions.deleting(&FactDeleteKind::Chain { id }),
                    )
                    .into_any_element(),
                ),
            )
            .inset()
            .into_any_element(),
        ]))
        .into_any_element()
    }
    /// Every saved version in the project, newest first.
    pub(super) fn memory_archive_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut page = settings_ui::settings_page("memory-archive-page", "All saved versions")
            .description("Every version saved in this project, including corrected ones.")
            .breadcrumb(settings_ui::breadcrumb(
                [self.memory_crumb(cx)],
                "All saved versions",
            ))
            .child(self.memory_inspection_status(cx));
        let empty = self
            .facts
            .versions
            .page
            .as_ref()
            .is_none_or(|page| page.versions.is_empty());
        page = if empty && self.facts.versions.read.is_some() {
            page.child(settings_ui::loading_list(3, self.facts.versions.read_since))
        } else if empty {
            page.child(settings_ui::empty_state(
                AppIcon::Memory,
                "No saved versions",
                "Versions appear here once a memory is saved.",
                None,
            ))
        } else {
            page.child(self.memory_version_list(cx))
        };
        page.into_any_element()
    }
}
