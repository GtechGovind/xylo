//! Native user image attachment controls. The draft owns durable references;
//! this state holds only bounded preview paths and in-flight request correlation.
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
    pub(super) fn importing(&self) -> bool {
        self.pending
            .values()
            .any(|pending| matches!(pending, PendingImage::Import { .. }))
    }
}
impl Xylo {
    pub(super) fn attachment_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session;
        cx.spawn_in(window, async move |view, cx| {
            if let Some(files) = rfd::AsyncFileDialog::new()
                .set_title("Attach images")
                .add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif"])
                .pick_files()
                .await
            {
                let paths = files
                    .into_iter()
                    .map(|file| file.path().to_path_buf())
                    .collect();
                let _ = view.update_in(cx, |view, _, cx| {
                    if view.session == session {
                        view.attach_image_paths(paths, AttachmentSource::Picker, cx);
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
    pub(super) fn attachment_paste_files(&mut self, cx: &mut Context<Self>) {
        if let Some(item) = cx.read_from_clipboard()
            && self.attachment_paste_item(&item, cx)
        {
            return;
        }
        self.attachments.notice =
            "Copy an image or image files, then paste into the message.".into();
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
        if session == self.session {
            if let Some(history) = self.histories.get_mut(&session) {
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
    pub(super) fn attachment_strip(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut strip = div().flex().flex_wrap().gap_2();
        for image in self.draft_attachments() {
            let id = image.id;
            let mut card = div()
                .flex()
                .gap_2()
                .items_center()
                .p_2()
                .border_1()
                .border_color(rgb(border()))
                .rounded_md();
            if let Some(path) = self.attachments.previews.get(&id) {
                card = card.child(
                    img(path.clone())
                        .w(px(64.))
                        .h(px(64.))
                        .object_fit(ObjectFit::Contain),
                );
            }
            card = card
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .max_w(px(180.))
                        .child(subtitle(image.name))
                        .child(subtitle(format!(
                            "{} × {} · {} KiB",
                            image.width,
                            image.height,
                            image.bytes.div_ceil(1024)
                        ))),
                )
                .child(
                    button(SharedString::from(format!("remove-image-{id}")), "Remove").on_click(
                        cx.listener(move |view, _, _, cx| view.remove_draft_attachment(id, cx)),
                    ),
                );
            strip = strip.child(card);
        }
        if self.attachments.importing_for(self.session) {
            strip = strip.child(subtitle("Preparing image…"));
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
