//! Native user image attachment controls. The draft owns durable references;
//! this state holds only bounded preview paths and in-flight request correlation.
use super::*;

enum PendingImage {
    Import { session: Uuid },
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
                |pending| matches!(pending,PendingImage::Import{session}if *session==self.session),
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
                        session: self.session,
                    },
                );
                self.attachments.notice = "Preparing image…".into();
            }
            self.result(result, cx);
        }
        cx.notify();
    }
    pub(super) fn attachment_paste_files(&mut self, cx: &mut Context<Self>) {
        let mut paths = Vec::new();
        let mut images = Vec::new();
        if let Some(item) = cx.read_from_clipboard() {
            for entry in item.into_entries() {
                match entry {
                    ClipboardEntry::ExternalPaths(selected) => paths.extend(selected.0.into_vec()),
                    ClipboardEntry::Image(image) => images.push(image),
                    _ => {}
                }
            }
        }
        if !paths.is_empty() {
            self.attach_image_paths(paths, AttachmentSource::PastedFile, cx);
            return;
        }
        if images.len() != 1 {
            self.attachments.notice =
                "Copy an image or image files, then choose Paste image.".into();
            cx.notify();
            return;
        }
        if self.attachments.importing()
            || self.draft_attachments().len() >= MAX_ATTACHMENTS_PER_TURN
        {
            self.attachments.notice =
                "Wait for the current import or remove an image before adding another.".into();
            cx.notify();
            return;
        }
        let image = images.pop().unwrap();
        if image.bytes.len() as u64 > MAX_IMAGE_BYTES {
            self.attachments.notice = "Clipboard image exceeds the 8 MiB import limit.".into();
            cx.notify();
            return;
        }
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| {
                host.import_image_bytes(id, self.session, image.bytes, AttachmentSource::Clipboard)
            });
        if result.is_ok() {
            self.attachments.pending.insert(
                id,
                PendingImage::Import {
                    session: self.session,
                },
            );
            self.attachments.notice = "Preparing clipboard image…".into();
        }
        self.result(result, cx);
        cx.notify();
    }
    pub(super) fn image_imported(
        &mut self,
        id: Uuid,
        session: Uuid,
        result: Result<(ImageAttachment, PathBuf), String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(PendingImage::Import { session: expected }) = self.attachments.pending.remove(&id)
        else {
            return false;
        };
        if expected != session || session != self.session {
            return true;
        }
        match result {
            Ok((image, path)) if image.session_id == session => {
                let image_id = image.id;
                if self.add_draft_attachment(image, cx) {
                    self.attachments.previews.insert(image_id, path);
                    self.attachments.notice =
                        "Image attached. The selected model must support image input.".into();
                }
            }
            Ok(_) => {
                self.attachments.notice =
                    "Image belongs to another conversation. Attach it again in this chat.".into()
            }
            Err(error) => self.attachments.notice = error,
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
            Err(error) => self.attachments.notice = error,
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
    /// Drop target and explicit user import actions; no images import on render.
    pub(super) fn attachment_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                icon_button(
                    "attach-images",
                    "Attach images",
                    gpui_kit::component::IconName::Plus,
                )
                .disabled(self.attachments.importing())
                .on_click(cx.listener(|view, _, window, cx| view.attachment_picker(window, cx))),
            )
            .child(
                icon_button(
                    "paste-image-files",
                    "Paste image",
                    gpui_kit::component::IconName::Copy,
                )
                .disabled(self.attachments.importing())
                .on_click(cx.listener(|view, _, _, cx| view.attachment_paste_files(cx))),
            )
            .into_any_element()
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
        if self.attachments.importing() {
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
            if let PendingImage::Import { session } = pending {
                Some((*id, *session))
            } else {
                None
            }
        })
    }
}
