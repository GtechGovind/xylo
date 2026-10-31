//! Pending messages are scoped to their chat; the host owns durable execution.
use super::*;
use xylo_runtime::turn_queue::{QueueReceipt, QueuedTurn};

#[derive(Default)]
pub(super) struct TurnQueueUi {
    pub(super) items: BTreeMap<Uuid, Vec<QueuedTurn>>,
    requests: BTreeMap<Uuid, Uuid>,
    operations: BTreeMap<Uuid, (Uuid, Uuid)>,
    notice: BTreeMap<Uuid, String>,
}
impl Xylo {
    pub(super) fn refresh_turn_queue(&mut self, session: Uuid, cx: &mut Context<Self>) {
        let Some(host) = &self.host else {
            return;
        };
        let id = Uuid::new_v4();
        self.turn_queue
            .requests
            .retain(|_, owned| *owned != session);
        self.turn_queue.requests.insert(id, session);
        if let Err(error) = host.list_turn_queue(id, session) {
            self.turn_queue.requests.remove(&id);
            tracing::warn!(%error, "Load pending messages");
            self.turn_queue.notice.insert(
                session,
                "Pending messages could not be loaded. Reopen this chat to retry.".into(),
            );
        }
        cx.notify();
    }
    pub(super) fn turn_queue_result(
        &mut self,
        id: Uuid,
        session: Uuid,
        result: Result<Vec<QueuedTurn>, String>,
        cx: &mut Context<Self>,
    ) {
        let expected = self.turn_queue.requests.remove(&id) == Some(session);
        let operation = self
            .turn_queue
            .operations
            .remove(&id)
            .is_some_and(|(owned, _)| owned == session);
        if !id.is_nil() && !expected && !operation {
            return;
        }
        match result {
            Ok(items) if items.iter().all(|item| item.turn.session_id == session) => {
                self.turn_queue.notice.remove(&session);
                if items.is_empty() {
                    self.turn_queue.items.remove(&session);
                } else {
                    self.turn_queue.items.insert(session, items);
                }
            }
            _ => {
                self.turn_queue.notice.insert(
                    session,
                    "The message queue could not be updated. Your saved messages are retained."
                        .into(),
                );
            }
        }
        cx.notify();
    }
    pub(super) fn queue_added(
        &mut self,
        id: Uuid,
        result: Result<QueueReceipt, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = result.and_then(|mut receipt| {
            if !self.reconcile_queue_receipt(id, &mut receipt) {
                return Err("Queued message acknowledgement did not match".into());
            }
            let session = receipt.item.turn.session_id;
            self.turn_queue
                .requests
                .retain(|_, owned| *owned != session);
            let items = self.turn_queue.items.entry(session).or_default();
            if !items.iter().any(|item| item.id == receipt.item.id) {
                items.push(receipt.item.clone());
            }
            self.turn_queue.notice.remove(&session);
            Ok(xylo_runtime::drafts::DraftAdmission {
                turn_id: receipt.item.turn.turn_id,
                draft: receipt.draft,
                cleared: receipt.cleared,
            })
        });
        self.draft_admitted(id, result, window, cx);
    }
    pub(super) fn queued_turn_started(
        &mut self,
        session: Uuid,
        item: QueuedTurn,
        cx: &mut Context<Self>,
    ) {
        if item.turn.session_id != session {
            return;
        }
        self.turn_queue
            .requests
            .retain(|_, owned| *owned != session);
        if let Some(items) = self.turn_queue.items.get_mut(&session) {
            items.retain(|queued| queued.id != item.id);
        }
        self.turn_queue.notice.remove(&session);
        self.turns.entry(session).or_default().request = Some(item.turn.clone());
        if self.session == session && self.workspace.as_ref() == Some(&item.turn.workspace) {
            self.archive_finished_turn(session);
            let mut user = message(session, "user", item.turn.prompt);
            user.id = Uuid::new_v5(&item.turn.turn_id, b"user-message");
            user.attachments = item.turn.attachments;
            if !self.messages.iter().any(|existing| existing.id == user.id) {
                self.messages.push(user);
            }
        } else if let Some(history) = self.histories.get_mut(&session) {
            history.stale = true;
        }
        cx.notify();
    }
    fn manage_queued_turn(&mut self, item: Uuid, send: bool, cx: &mut Context<Self>) {
        let session = self.session;
        if send && self.running() {
            return;
        }
        if self
            .turn_queue
            .operations
            .values()
            .any(|entry| *entry == (session, item))
        {
            return;
        }
        let Some(host) = &self.host else {
            return;
        };
        let id = Uuid::new_v4();
        let result = if send {
            host.run_queued_turn(id, session, item)
        } else {
            host.remove_queued_turn(id, session, item)
        };
        if result.is_ok() {
            self.turn_queue
                .requests
                .retain(|_, owned| *owned != session);
            self.turn_queue.operations.insert(id, (session, item));
        } else {
            self.turn_queue
                .notice
                .insert(session, "The queue is busy. Try again.".into());
        }
        cx.notify();
    }
    pub(super) fn pending_messages(&self, cx: &mut Context<Self>) -> AnyElement {
        let items = self.turn_queue.items.get(&self.session);
        let guidance = self.pending_guidance_text();
        let notice = self.turn_queue.notice.get(&self.session);
        if items.is_none_or(|items| items.is_empty()) && guidance.is_none() && notice.is_none() {
            return div().into_any_element();
        }
        let mut rows = div()
            .id("composer-pending")
            .test_support()
            .w_full()
            .max_h(px(156.))
            .overflow_y_scroll()
            .mb_2()
            .flex()
            .flex_col()
            .gap_1();
        if let Some(text) = guidance {
            rows = rows.child(
                div()
                    .id("pending-guidance")
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded(px(10.))
                    .bg(rgb(panel()))
                    .child(icons::small(AppIcon::ChevronRight).text_color(rgb(icons::rest_color())))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .child(div().single_line().child(text)),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(muted()))
                            .child("Steering…"),
                    ),
            );
        }
        for (index, item) in items.into_iter().flatten().enumerate() {
            let id = item.id;
            let pending = self
                .turn_queue
                .operations
                .values()
                .any(|entry| *entry == (self.session, id));
            let image_count = item.turn.attachments.len();
            let title = if item.turn.prompt.trim().is_empty() {
                format!(
                    "{image_count} image{}",
                    if image_count == 1 { "" } else { "s" }
                )
            } else {
                item.turn.prompt.clone()
            };
            rows =
                rows.child(
                    div()
                        .id(("queued-message", index))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .rounded(px(10.))
                        .bg(rgb(panel()))
                        .border_1()
                        .border_color(rgb(border()))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(muted()))
                                .child(format!("{}", index + 1)),
                        )
                        .child(
                            div()
                                .id(("queued-message-text", index))
                                .flex_1()
                                .min_w_0()
                                .text_size(px(12.))
                                .child(div().single_line().child(title.clone()))
                                .tooltip(move |_, cx| {
                                    cx.new(|_| XyloTooltip(title.clone().into())).into()
                                }),
                        )
                        .when(
                            image_count > 0 && !item.turn.prompt.trim().is_empty(),
                            |row| {
                                row.child(div().text_size(px(11.)).text_color(rgb(muted())).child(
                                    format!(
                                        "{image_count} image{}",
                                        if image_count == 1 { "" } else { "s" }
                                    ),
                                ))
                            },
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(muted()))
                                .child(if item.paused { "Paused" } else { "Queued" }),
                        )
                        .when(!self.running(), |row| {
                            row.child(
                                button(("send-queued", index), "Send")
                                    .text_xs()
                                    .disabled(pending)
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        view.manage_queued_turn(id, true, cx)
                                    })),
                            )
                        })
                        .child(
                            icons::icon_button(
                                ("remove-queued", index),
                                AppIcon::Close,
                                "Remove queued message",
                            )
                            .disabled(pending)
                            .on_click(cx.listener(
                                move |view, _, _, cx| view.manage_queued_turn(id, false, cx),
                            )),
                        ),
                );
        }
        // The host pauses the queue when a task stops or fails; say why and let
        // the user resume it from the first message.
        let paused = items
            .into_iter()
            .flatten()
            .find(|item| item.paused)
            .filter(|_| !self.running());
        if let Some(item) = paused {
            let first = items.and_then(|items| items.first()).map(|item| item.id);
            let reason = item
                .reason
                .clone()
                .unwrap_or_else(|| "The queue is paused.".into());
            rows = rows.child(
                div()
                    .id("queue-paused")
                    .test_support()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .child(div().flex_1().min_w_0().child(subtitle(reason).text_xs()))
                    .when_some(first, |row, first| {
                        let pending = self
                            .turn_queue
                            .operations
                            .values()
                            .any(|entry| *entry == (self.session, first));
                        row.child(
                            button("resume-queue", "Resume queue")
                                .text_xs()
                                .disabled(pending)
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    view.manage_queued_turn(first, true, cx)
                                })),
                        )
                    }),
            );
        }
        if let Some(notice) = notice {
            rows = rows.child(subtitle(notice.clone()).text_xs());
        }
        rows.into_any_element()
    }
}
