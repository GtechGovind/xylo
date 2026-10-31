//! Application-level status surfaces: the transient status toast for host and
//! task notices, the persistent banner shown when the background service is
//! gone, and the one-line error summary. Host statuses are informational and
//! expire on their own; a stopped service stays visible until Xylo restarts.
use super::*;

/// How long a status notice stays on screen.
const STATUS_TTL: std::time::Duration = std::time::Duration::from_secs(4);
/// How long a notice with an Undo stays, so there is time to reach it.
const UNDO_TTL: std::time::Duration = std::time::Duration::from_secs(8);
/// The last part of a notice's time on screen, spent fading out.
const TOAST_EXIT: std::time::Duration = crate::motion::BASE;
/// The toast's distance from the window's bottom-right corner.
const TOAST_INSET: f32 = 16.;

/// What a toast's Undo reverses.
#[derive(Clone, Debug)]
pub(super) enum ToastUndo {
    /// Restore a chat that was just archived. The chat travels with the undo
    /// so it can return to the list even when it is older than the first page.
    Unarchive(Conversation),
}

/// The latest status notice and when it was posted. Assigning a new value
/// (`view.status = "…".into()`) restarts its display time.
pub(super) struct StatusLine {
    text: String,
    posted: std::time::Instant,
    undo: Option<ToastUndo>,
}

impl StatusLine {
    /// A notice that offers Undo for the action it reports.
    pub(super) fn with_undo(text: impl Into<String>, undo: ToastUndo) -> Self {
        Self {
            text: text.into(),
            posted: std::time::Instant::now(),
            undo: Some(undo),
        }
    }
    fn ttl(&self) -> std::time::Duration {
        if self.undo.is_some() {
            UNDO_TTL
        } else {
            STATUS_TTL
        }
    }
    fn visible(&self) -> bool {
        !self.text.is_empty() && self.posted.elapsed() < self.ttl()
    }
    #[cfg(test)]
    #[allow(dead_code)] // Used by the separate native validation crate.
    pub(super) fn undo(&self) -> Option<&ToastUndo> {
        self.undo.as_ref()
    }

    #[cfg(test)]
    #[allow(dead_code)] // Used by the separate native validation crate.
    pub(super) fn text(&self) -> &str {
        &self.text
    }
}

impl From<String> for StatusLine {
    fn from(text: String) -> Self {
        Self {
            text,
            posted: std::time::Instant::now(),
            undo: None,
        }
    }
}

impl From<&str> for StatusLine {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

/// Whether the background service that owns storage and workers is reachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HostHealth {
    Running,
    /// The service ended while Xylo was open.
    Stopped,
    /// The service never started.
    Unavailable,
}

/// The first line of an error: producers write product text, details stay in logs.
pub(super) fn product_error(error: &str) -> String {
    error
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Something went wrong. Please try again.")
        .to_owned()
}

impl Xylo {
    /// Called when the host event stream ends. During shutdown that is the
    /// expected end; otherwise the service stopped and only a restart recovers.
    pub(super) fn host_disconnected(&mut self, cx: &mut Context<Self>) {
        if self.quitting {
            cx.quit();
            return;
        }
        if self.host_health == HostHealth::Running {
            self.host_health = HostHealth::Stopped;
        }
        cx.notify();
    }

    pub(super) fn host_running(&self) -> bool {
        self.host_health == HostHealth::Running
    }

    /// The toast for the latest status notice, while it is fresh. It rises
    /// and fades in when posted and fades out over its last moments. Arms one
    /// task per notice that redraws when the exit starts and when it ends, so
    /// the toast leaves without further events.
    pub(super) fn status_toast(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.status.visible() {
            return None;
        }
        let posted = self.status.posted;
        let ttl = self.status.ttl();
        if self.status_expiry.as_ref().map(|(at, _)| *at) != Some(posted) {
            let remaining = ttl.saturating_sub(posted.elapsed());
            let exit_at = remaining.saturating_sub(TOAST_EXIT);
            let task = cx.spawn(async move |view, cx| {
                cx.background_executor().timer(exit_at).await;
                let _ = view.update(cx, |_, cx| cx.notify());
                cx.background_executor()
                    .timer(remaining.saturating_sub(exit_at))
                    .await;
                let _ = view.update(cx, |_, cx| cx.notify());
            });
            self.status_expiry = Some((posted, task));
        }
        let exiting = posted.elapsed() + TOAST_EXIT >= ttl;
        let toast = div()
            .id("status-toast")
            .absolute()
            .bottom(px(TOAST_INSET))
            .right(px(TOAST_INSET))
            .max_w(px(420.))
            .px(px(14.))
            .py(px(8.))
            .rounded(px(10.))
            .bg(rgb(panel()))
            .border_1()
            .border_color(rgb(border()))
            .shadow_md()
            .text_size(px(12.5))
            .text_color(rgb(text_secondary()))
            .flex()
            .items_center()
            .gap(px(12.))
            .child(self.status.text.clone())
            .when_some(self.status.undo.clone(), |toast, undo| {
                toast.child(
                    focus_ring(gpui_kit::base::Button::new("status-toast-undo"))
                        .accessibility_label("Undo")
                        .px(px(6.))
                        .rounded(px(6.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(rgb(text()))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(hover())))
                        .child("Undo")
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.status = String::new().into();
                            match undo.clone() {
                                ToastUndo::Unarchive(chat) => view.unarchive_chat(chat, cx),
                            }
                        })),
                )
            });
        // The toast is positioned absolutely, so it moves by its inset rather
        // than by a relative offset.
        Some(if exiting {
            toast
                .with_animation(
                    crate::motion::keyed_id("status-toast-exit", &posted),
                    Animation::new(TOAST_EXIT).with_easing(crate::motion::exit),
                    |toast, t| toast.opacity(1. - t).bottom(px(TOAST_INSET - 6. * t)),
                )
                .into_any_element()
        } else {
            toast
                .with_animation(
                    crate::motion::keyed_id("status-toast-enter", &posted),
                    Animation::new(crate::motion::BASE).with_easing(crate::motion::enter),
                    |toast, t| toast.opacity(t).bottom(px(TOAST_INSET - 10. * (1. - t))),
                )
                .into_any_element()
        })
    }

    /// Persistent banner while the background service is gone. It replaces the
    /// error notice, whose text becomes the banner's detail.
    pub(super) fn host_stopped_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        let title = match self.host_health {
            HostHealth::Unavailable => "Xylo's background service couldn't start",
            _ => "Xylo's background service stopped",
        };
        let detail = self
            .error
            .as_deref()
            .map(product_error)
            .unwrap_or_else(|| "Restart Xylo to continue. Your saved chats are kept.".into());
        div()
            .px(px(20.))
            .pb_2()
            .flex()
            .justify_center()
            .child(
                div()
                    .w_full()
                    .max_w(px(760.))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px(px(14.))
                    .py(px(10.))
                    .rounded(px(10.))
                    .bg(rgb(danger_bg()))
                    .text_size(px(13.))
                    .text_color(rgb(danger()))
                    .child(crate::icons::status(AppIcon::Error))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().font_weight(FontWeight::MEDIUM).child(title))
                            .child(detail),
                    )
                    .child(
                        primary("restart-xylo", "Restart Xylo")
                            .h(px(28.))
                            .text_size(px(12.5))
                            .on_click(cx.listener(|_, _, _, cx| cx.restart())),
                    ),
            )
            .into_any_element()
    }

    /// The error notice with a Dismiss action.
    pub(super) fn error_notice(&self, error: &str, cx: &mut Context<Self>) -> AnyElement {
        let entrance = crate::motion::keyed_id("error-notice-enter", &error);
        let notice_row = div().px(px(20.)).pb_2().flex().justify_center().child(
            notice(BadgeTone::Danger, product_error(error))
                .max_w(px(760.))
                .items_center()
                .child(
                    button("dismiss-error", "Dismiss")
                        .h(px(26.))
                        .text_size(px(12.))
                        .text_color(rgb(danger()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.error = None;
                            cx.notify();
                        })),
                ),
        );
        crate::motion::fade_slide_in(entrance, notice_row, -6.).into_any_element()
    }
}

#[cfg(test)]
#[path = "app_status/tests.rs"]
mod tests;
