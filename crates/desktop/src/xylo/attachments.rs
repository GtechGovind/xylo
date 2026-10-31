//! Native user image attachment controls. The draft owns durable references;
//! this state holds only bounded preview paths and in-flight request correlation.
//! The composer's picker also accepts other files, which are referenced by path
//! in the message instead of attached.
use super::*;
use xylo_runtime::drafts::DraftKey;

enum PendingImage {
    Import { key: DraftKey },
    Preview { session: Uuid, image: Uuid },
}
pub(super) struct AttachmentUi {
    pending: BTreeMap<Uuid, PendingImage>,
    previews: BTreeMap<Uuid, PathBuf>,
    pub(super) notice: String,
}
impl AttachmentUi {
    pub(super) fn new() -> Self {
        Self {
            pending: BTreeMap::new(),
            previews: BTreeMap::new(),
            notice: String::new(),
        }
    }
    pub(super) fn has_import_for(&self, key: &DraftKey) -> bool {
        self.pending
            .values()
            .any(|pending| matches!(pending, PendingImage::Import { key: owned } if owned == key))
    }
    pub(super) fn importing_for(&self, session: Uuid) -> bool {
        self.pending.values().any(
            |pending| matches!(pending, PendingImage::Import { key } if key.session_id == session),
        )
    }
    #[cfg(test)]
    #[allow(dead_code)] // Used by the native validation harness.
    pub(super) fn importing(&self) -> bool {
        self.pending
            .values()
            .any(|pending| matches!(pending, PendingImage::Import { .. }))
    }
}
impl Xylo {
    /// The composer's "Attach files or images…": images become attachments,
    /// any other file is referenced by path in the message.
    pub(super) fn attachment_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session;
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |view, cx| {
            let mut picker = rfd::AsyncFileDialog::new().set_title("Attach files or images");
            if let Some(workspace) = workspace {
                picker = picker.set_directory(workspace);
            }
            if let Some(files) = picker.pick_files().await {
                let (images, files): (Vec<PathBuf>, Vec<PathBuf>) = files
                    .into_iter()
                    .map(|file| file.path().to_path_buf())
                    .partition(|path| chat::is_image_path(path));
                let _ = view.update_in(cx, |view, window, cx| {
                    if view.session == session {
                        view.attach_image_paths(images, AttachmentSource::Picker, cx);
                        view.reference_files_in_composer(files, window, cx);
                    }
                });
            }
        })
        .detach();
    }
    pub(super) fn attach_image_paths(
        &mut self,
        paths: Vec<PathBuf>,
        source: AttachmentSource,
        cx: &mut Context<Self>,
    ) {
        let pending = self
            .attachments
            .pending
            .values()
            .filter(
                |pending| matches!(pending,PendingImage::Import{key}if key.session_id==self.session),
            )
            .count();
        if paths.is_empty() {
            return;
        }
        if paths.len() + pending + self.draft_attachments().len() > MAX_ATTACHMENTS_PER_TURN {
            self.attachments.notice =
                "Attach at most four images per message. Remove an image before adding another."
                    .into();
            cx.notify();
            return;
        }
        for path in paths {
            let id = Uuid::new_v4();
            let result = self
                .host
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
                .and_then(|host| host.import_image(id, self.session, path, source));
            if result.is_ok() {
                self.attachments.pending.insert(
                    id,
                    PendingImage::Import {
                        key: self.current_draft_key(),
                    },
                );
                self.attachments.notice = "Preparing image…".into();
            }
            self.result(result, cx);
        }
        cx.notify();
    }
    /// The native input owns text insertion/selection/undo. Consume only image
    /// and file payloads; image bytes are copied only after validating the bound.
    pub(super) fn attachment_paste_item(
        &mut self,
        item: &ClipboardItem,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut paths = Vec::new();
        let mut images = Vec::new();
        for entry in item.entries() {
            match entry {
                ClipboardEntry::ExternalPaths(selected) => paths.extend(
                    selected
                        .0
                        .iter()
                        .take((MAX_ATTACHMENTS_PER_TURN + 1).saturating_sub(paths.len()))
                        .cloned(),
                ),
                ClipboardEntry::Image(image) if images.len() < 2 => images.push(image),
                _ => {}
            }
        }
        if !paths.is_empty() {
            self.attach_image_paths(paths, AttachmentSource::PastedFile, cx);
            return true;
        }
        if images.is_empty() {
            return false;
        }
        if images.len() != 1 {
            self.attachments.notice = "Paste one copied image or image files.".into();
            cx.notify();
            return true;
        }
        if self.attachments.importing_for(self.session)
            || self.draft_attachments().len() >= MAX_ATTACHMENTS_PER_TURN
        {
            self.attachments.notice =
                "Wait for the current import or remove an image before adding another.".into();
            cx.notify();
            return true;
        }
        let image = images.pop().unwrap();
        if image.bytes.len() as u64 > MAX_IMAGE_BYTES {
            self.attachments.notice = "Clipboard image exceeds the 8 MiB import limit.".into();
            cx.notify();
            return true;
        }
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| {
                host.import_image_bytes(
                    id,
                    self.session,
                    image.bytes.clone(),
                    AttachmentSource::Clipboard,
                )
            });
        if result.is_ok() {
            self.attachments.pending.insert(
                id,
                PendingImage::Import {
                    key: self.current_draft_key(),
                },
            );
            self.attachments.notice = "Preparing clipboard image…".into();
        }
        self.result(result, cx);
        cx.notify();
        true
    }
    pub(super) fn image_imported(
        &mut self,
        id: Uuid,
        session: Uuid,
        result: Result<(ImageAttachment, PathBuf), String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(PendingImage::Import { key }) = self.attachments.pending.remove(&id) else {
            return false;
        };
        if key.session_id != session {
            return true;
        }
        match result {
            Ok((image, path)) if image.session_id == session => {
                let image_id = image.id;
                if self.add_draft_attachment_to(&key, image, cx) {
                    self.attachments.previews.insert(image_id, path);
                    self.draft_attachment_notice(
                        &key,
                        "Image attached. The selected model must support image input.".into(),
                        cx,
                    );
                }
            }
            Ok(_) => self.draft_attachment_notice(
                &key,
                "Image belongs to another conversation. Attach it again in this chat.".into(),
                cx,
            ),
            Err(error) => self.draft_attachment_notice(&key, error, cx),
        }
        cx.notify();
        true
    }
    pub(super) fn image_preview(
        &mut self,
        id: Uuid,
        session: Uuid,
        image: Uuid,
        result: Result<PathBuf, String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(PendingImage::Preview {
            session: expected,
            image: expected_image,
        }) = self.attachments.pending.remove(&id)
        else {
            return false;
        };
        if session != expected || image != expected_image {
            return true;
        }
        match result {
            Ok(path) => {
                while self.attachments.previews.len() >= 128 {
                    self.attachments.previews.pop_first();
                }
                self.attachments.previews.insert(image, path);
            }
            Err(error) if session == self.session => self.attachments.notice = error,
            Err(_) => {}
        }
        if session == self.session
            && let Some(history) = self.histories.get_mut(&session)
        {
            for (index, message) in self.messages.iter().enumerate() {
                if message
                    .attachments
                    .iter()
                    .any(|attachment| attachment.id == image)
                {
                    let row = index + 1;
                    if row < history.list.item_count() {
                        history.list.remeasure_items(row..row + 1);
                    }
                }
            }
        }
        cx.notify();
        true
    }
    pub(super) fn attachment_previews(
        &mut self,
        images: &[ImageAttachment],
        cx: &mut Context<Self>,
    ) {
        for image in images {
            if self.attachments.pending.len() >= 32 {
                break;
            }
            if self.attachments.previews.contains_key(&image.id)
                || self.attachments.pending.values().any(
                    |pending| matches!(pending,PendingImage::Preview{image:id,..}if *id==image.id),
                )
            {
                continue;
            }
            let id = Uuid::new_v4();
            let result = self
                .host
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
                .and_then(|host| host.image_preview(id, image.clone()));
            if result.is_ok() {
                self.attachments.pending.insert(
                    id,
                    PendingImage::Preview {
                        session: image.session_id,
                        image: image.id,
                    },
                );
            } else {
                self.result(result, cx);
            }
        }
    }
    /// The draft's images as compact chips: a thumbnail, the name and size,
    /// and a remove control.
    pub(super) fn attachment_strip(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut strip = div().flex().flex_wrap().gap_2();
        for image in self.draft_attachments() {
            let id = image.id;
            let thumbnail = div()
                .size(px(40.))
                .flex_shrink_0()
                .rounded(px(6.))
                .overflow_hidden()
                .bg(rgb(well()))
                .flex()
                .items_center()
                .justify_center()
                .child(match self.attachments.previews.get(&id) {
                    Some(path) => img(path.clone())
                        .size(px(40.))
                        .object_fit(ObjectFit::Cover)
                        .into_any_element(),
                    None => icons::small(AppIcon::Image)
                        .text_color(rgb(icons::rest_color()))
                        .into_any_element(),
                });
            strip = strip.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .p(px(4.))
                    .pr(px(2.))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(rgb(border()))
                    .bg(rgb(bg()))
                    .child(thumbnail)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .max_w(px(160.))
                            .min_w_0()
                            .child(
                                div()
                                    .single_line()
                                    .text_size(px(12.5))
                                    .text_color(rgb(text()))
                                    .child(image.name),
                            )
                            .child(
                                div()
                                    .single_line()
                                    .text_size(px(11.5))
                                    .text_color(rgb(muted()))
                                    .child(format!(
                                        "{} × {} · {} KiB",
                                        image.width,
                                        image.height,
                                        image.bytes.div_ceil(1024)
                                    )),
                            ),
                    )
                    .child(
                        icons::icon_button(
                            SharedString::from(format!("remove-image-{id}")),
                            AppIcon::Close,
                            "Remove image",
                        )
                        .on_click(
                            cx.listener(move |view, _, _, cx| view.remove_draft_attachment(id, cx)),
                        ),
                    ),
            );
        }
        if self.attachments.importing_for(self.session) {
            strip = strip.child(
                div()
                    .h(px(50.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(crate::motion::spinner_inline())
                    .child(subtitle("Preparing image…")),
            );
        }
        strip.into_any_element()
    }
    pub(super) fn message_images(
        &self,
        message: &ChatMessage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut strip = div().flex().flex_wrap().gap_2();
        for image in &message.attachments {
            let mut card = div()
                .flex()
                .flex_col()
                .gap_1()
                .max_w(px(200.))
                .child(subtitle(image.name.clone()));
            if let Some(path) = self.attachments.previews.get(&image.id) {
                card = card.child(
                    img(path.clone())
                        .w(px(160.))
                        .h(px(120.))
                        .object_fit(ObjectFit::Contain),
                );
            } else {
                let image = image.clone();
                card = card.child(
                    button(
                        SharedString::from(format!("image-preview-{}", image.id)),
                        "Show preview",
                    )
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.attachment_previews(std::slice::from_ref(&image), cx)
                    })),
                );
            }
            strip = strip.child(card);
        }
        strip.into_any_element()
    }
}

#[cfg(test)]
#[allow(dead_code)]
impl AttachmentUi {
    pub(super) fn validation_preview(&self, id: Uuid) -> Option<PathBuf> {
        self.previews.get(&id).cloned()
    }
    pub(super) fn validation_import(&self) -> Option<(Uuid, Uuid)> {
        self.pending.iter().find_map(|(id, pending)| {
            if let PendingImage::Import { key } = pending {
                Some((*id, key.session_id))
            } else {
                None
            }
        })
    }
}
