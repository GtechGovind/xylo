//! Pending messages are scoped to their chat; the host owns durable execution.
use super::*;
use xylo_runtime::turn_queue::{QueueReceipt, QueuedTurn};

#[derive(Default)]
pub(super) struct TurnQueueUi {
    pub(super) items: BTreeMap<Uuid, Vec<QueuedTurn>>,
    requests: BTreeMap<Uuid, Uuid>,
    operations: BTreeMap<Uuid, (Uuid, Uuid)>,
    notice: BTreeMap<Uuid, String>,
    show_all: BTreeSet<Uuid>,
    edits: BTreeMap<Uuid, QueuedTurn>,
    undo: BTreeMap<Uuid, Uuid>,
    removals: BTreeSet<Uuid>,
    restores: BTreeSet<Uuid>,
    pauses: BTreeMap<Uuid, Uuid>,
    recovered: BTreeMap<Uuid, QueuedTurn>,
    guidance: BTreeMap<Uuid, QueuedTurn>,
}
impl TurnQueueUi {
    fn operation_pending(&self, session: Uuid, item: Uuid) -> bool {
        self.operations
            .values()
            .any(|owned| *owned == (session, item))
    }

    /// Only retire a transfer's recovery link once the composer payload is durable.
    pub(super) fn draft_saved(&mut self, draft: &xylo_runtime::drafts::DraftSnapshot) {
        self.recovered.retain(|_, item| {
            item.turn.session_id != draft.key.session_id
                || draft.key.workspace.as_ref() != Some(&item.turn.workspace)
                || item.turn.prompt != draft.text
                || item.turn.attachments != draft.attachments
        });
    }
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let expected = self.turn_queue.requests.remove(&id) == Some(session);
        let pause = self.turn_queue.pauses.get(&session) == Some(&id);
        if pause {
            self.turn_queue.pauses.remove(&session);
        }
        let removed = self.turn_queue.operations.get(&id).copied();
        let operation = self
            .turn_queue
            .operations
            .remove(&id)
            .is_some_and(|(owned, _)| owned == session);
        if !id.is_nil() && !expected && !operation && !pause {
            return;
        }
        let edit = self.turn_queue.edits.remove(&id);
        let succeeded = result
            .as_ref()
            .is_ok_and(|items| items.iter().all(|item| item.turn.session_id == session));
        let removal = self.turn_queue.removals.remove(&id);
        let restore = self.turn_queue.restores.remove(&id);
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
        if !succeeded && let Some((_, item)) = removed {
            self.turn_queue.guidance.remove(&item);
        }
        if succeeded
            && restore
            && let Some((owned, item)) = removed
            && owned == session
            && self.turn_queue.undo.get(&session) == Some(&item)
        {
            self.turn_queue.undo.remove(&session);
        }
        if let Some(item) = edit {
            self.turn_queue.recovered.insert(item.id, item.clone());
            if self.session == session && self.workspace.as_ref() == Some(&item.turn.workspace) {
                self.replace_composer_payload(
                    drafts::MessagePayload {
                        text: item.turn.prompt,
                        attachments: item.turn.attachments,
                    },
                    window,
                    cx,
                );
                if !succeeded {
                    self.turn_queue.notice.insert(session, "This message may already have started. Its text was recovered as a draft; review before sending.".into());
                }
            } else {
                self.turn_queue.notice.insert(
                    session,
                    "Return to this chat to recover the message for editing.".into(),
                );
            }
        } else if succeeded
            && removal
            && let Some((_, item)) = removed
        {
            if self
                .turn_queue
                .items
                .get(&session)
                .is_none_or(|items| !items.iter().any(|x| x.id == item))
            {
                self.turn_queue.undo.insert(session, item);
            } else if self.turn_queue.undo.get(&session) == Some(&item) {
                self.turn_queue.undo.remove(&session);
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
        if self.turn_queue.recovered.contains_key(&item.id) {
            self.turn_queue.notice.insert(session, "This message already started. Its text is retained for review as a draft; sending it again creates a new task.".into());
        } else {
            self.turn_queue.notice.remove(&session);
        }
        if self.session == session && self.workspace.as_ref() == Some(&item.turn.workspace) {
            self.archive_finished_turn(session);
            let mut user = message(session, "user", item.turn.prompt.clone());
            user.id = Uuid::new_v5(&item.turn.turn_id, b"user-message");
            user.turn_id = Some(item.turn.turn_id);
            user.attachments = item.turn.attachments.clone();
            if !self.messages.iter().any(|existing| existing.id == user.id) {
                self.messages.push(user);
            }
        } else if let Some(history) = self.histories.get_mut(&session) {
            history.stale = true;
        }
        self.turns.entry(session).or_default().request = Some(item.turn);
        cx.notify();
    }
    fn manage_queued_turn(&mut self, item: Uuid, send: bool, cx: &mut Context<Self>) {
        let _ = self.dispatch_queued_turn(item, send, cx);
    }

    fn dispatch_queued_turn(
        &mut self,
        item: Uuid,
        send: bool,
        cx: &mut Context<Self>,
    ) -> Option<Uuid> {
        if send {
            self.cancel_setup_close_intents(cx);
        }
        let session = self.session;
        if (send && self.running()) || self.turn_queue.operation_pending(session, item) {
            return None;
        }
        let host = self.host.as_ref()?;
        let id = Uuid::new_v4();
        let result = if send {
            host.run_queued_turn(id, session, item)
        } else {
            host.remove_queued_turn(id, session, item)
        };
        let accepted = result.is_ok();
        if accepted {
            self.turn_queue
                .requests
                .retain(|_, owned| *owned != session);
            self.turn_queue.operations.insert(id, (session, item));
            if !send {
                self.turn_queue.removals.insert(id);
            }
        } else {
            self.turn_queue
                .notice
                .insert(session, "The queue is busy. Try again.".into());
        }
        cx.notify();
        accepted.then_some(id)
    }
    fn edit_queued_turn(&mut self, item: QueuedTurn, cx: &mut Context<Self>) {
        // Never attach an edit to a Send/Remove already in flight.
        if let Some(request) = self.dispatch_queued_turn(item.id, false, cx) {
            self.turn_queue.edits.insert(request, item);
        }
    }
    fn undo_queued_turn(&mut self, item: Uuid, cx: &mut Context<Self>) {
        if self.turn_queue.undo.get(&self.session) != Some(&item)
            || self.turn_queue.operation_pending(self.session, item)
        {
            return;
        }
        let Some(host) = &self.host else {
            return;
        };
        let id = Uuid::new_v4();
        if host.restore_queued_turn(id, self.session, item).is_ok() {
            self.turn_queue.operations.insert(id, (self.session, item));
            self.turn_queue.restores.insert(id);
        } else {
            self.turn_queue.notice.insert(
                self.session,
                "The message could not be restored. Use Undo to retry.".into(),
            );
        }
        cx.notify();
    }
    pub(super) fn queued_guidance_result(
        &mut self,
        id: Uuid,
        result: &Result<xylo_runtime::steering::SteeringOutcome, String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(item) = self.turn_queue.guidance.get(&id).cloned() else {
            return false;
        };
        let session = item.turn.session_id;
        match result {
            Ok(outcome)
                if outcome.request.id == id
                    && outcome.request.session_id == session
                    && outcome.request.text == item.turn.prompt.trim()
                    && outcome.draft.key.workspace.as_ref() == Some(&item.turn.workspace) =>
            {
                match outcome.receipt.state {
                    xylo_core::SteeringState::Queued => {}
                    xylo_core::SteeringState::Applied => {
                        self.turn_queue.guidance.remove(&id);
                        let mut user = message(session, "user", outcome.request.text.clone());
                        user.id = Uuid::new_v5(&id, b"steering-message");
                        user.turn_id = Some(outcome.request.turn_id);
                        if self.session == session
                            && self.workspace.as_ref() == Some(&item.turn.workspace)
                        {
                            if !self.messages.iter().any(|message| message.id == user.id) {
                                self.messages.push(user);
                            }
                        } else if let Some(history) = self.histories.get_mut(&session) {
                            history.stale = true;
                        }
                    }
                    xylo_core::SteeringState::Cancelled => {
                        self.turn_queue.guidance.remove(&id);
                        self.turn_queue.notice.insert(session, outcome.receipt.reason.clone().unwrap_or_else(|| "The message was not added to the task; it remains in the queue.".into()));
                        self.refresh_turn_queue(session, cx);
                    }
                }
            }
            Err(error) => {
                self.turn_queue.guidance.remove(&id);
                self.turn_queue.notice.insert(session, error.clone());
            }
            _ => return true,
        }
        self.invalidate_live_history();
        self.sync_history_list(cx);
        cx.notify();
        true
    }
    fn steer_queued_turn(&mut self, item: Uuid, cx: &mut Context<Self>) {
        if self.turn_queue.operation_pending(self.session, item) {
            return;
        }
        let Some(host) = &self.host else {
            return;
        };
        let request = Uuid::new_v4();
        if host.steer_queued_turn(request, self.session, item).is_ok() {
            if let Some(snapshot) = self
                .turn_queue
                .items
                .get(&self.session)
                .and_then(|items| items.iter().find(|queued| queued.id == item))
                .cloned()
            {
                self.turn_queue.guidance.insert(item, snapshot);
            }
            self.turn_queue
                .operations
                .insert(request, (self.session, item));
        } else {
            self.turn_queue.notice.insert(
                self.session,
                "The task could not be updated. Your message remains in the queue.".into(),
            );
        }
        cx.notify();
    }
    fn set_queue_paused(&mut self, paused: bool, cx: &mut Context<Self>) {
        if self.turn_queue.pauses.contains_key(&self.session)
            || self
                .turn_queue
                .items
                .get(&self.session)
                .is_none_or(Vec::is_empty)
        {
            return;
        }
        let Some(host) = &self.host else {
            return;
        };
        let request = Uuid::new_v4();
        if host
            .set_turn_queue_paused(request, self.session, paused)
            .is_ok()
        {
            self.turn_queue.pauses.insert(self.session, request);
        } else {
            self.turn_queue.notice.insert(
                self.session,
                "The queue could not be paused or resumed. Try again.".into(),
            );
        }
        cx.notify();
    }
    fn preview_queued_turn(
        &mut self,
        item: QueuedTurn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui_kit::component::WindowExt;
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let item = item.clone();
            let content = weak
                .update(cx, |view, cx| {
                    let mut preview =
                        message(item.turn.session_id, "user", item.turn.prompt.clone());
                    preview.attachments = item.turn.attachments.clone();
                    let edit = item.clone();
                    let id = item.id;
                    let pending = view.turn_queue.operation_pending(item.turn.session_id, id);
                    div()
                        .id("queued-message-dialog")
                        .test_support()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            div()
                                .text_size(px(15.))
                                .whitespace_normal()
                                .child(item.turn.prompt.clone()),
                        )
                        .child(view.message_images(&preview, cx))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    primary("edit-queued-dialog", "Edit message")
                                        .disabled(pending)
                                        .on_click(cx.listener(move |view, _, window, cx| {
                                            window.close_dialog(cx);
                                            view.edit_queued_turn(edit.clone(), cx);
                                        })),
                                )
                                .child(
                                    secondary(
                                        "send-queued-dialog",
                                        if view.running() {
                                            "Update task"
                                        } else {
                                            "Send now"
                                        },
                                    )
                                    .disabled(
                                        pending
                                            || (view.running()
                                                && (!item.turn.attachments.is_empty()
                                                    || item.turn.prompt.len()
                                                        > xylo_core::MAX_STEERING_BYTES)),
                                    )
                                    .on_click(cx.listener(
                                        move |view, _, window, cx| {
                                            window.close_dialog(cx);
                                            if view.running() {
                                                view.steer_queued_turn(id, cx);
                                            } else {
                                                view.manage_queued_turn(id, true, cx);
                                            }
                                        },
                                    )),
                                )
                                .child(
                                    button("remove-queued-dialog", "Remove")
                                        .disabled(pending)
                                        .on_click(cx.listener(move |view, _, window, cx| {
                                            window.close_dialog(cx);
                                            view.manage_queued_turn(id, false, cx);
                                        })),
                                ),
                        )
                        .into_any_element()
                })
                .unwrap_or_else(|_| div().into_any_element());
            dialog
                .title("Next message")
                .width(px(720.))
                .rounded(px(16.))
                .bg(rgb(panel()))
                .border_color(rgb(border()))
                .child(content)
        });
    }
    pub(super) fn pending_messages(&self, cx: &mut Context<Self>) -> AnyElement {
        let items = self.turn_queue.items.get(&self.session);
        let guidance = self.pending_guidance_text();
        let notice = self.turn_queue.notice.get(&self.session);
        let has_items = items.is_some_and(|items| !items.is_empty());
        let has_rows = has_items
            || guidance.is_some()
            || self
                .turn_queue
                .guidance
                .values()
                .any(|item| item.turn.session_id == self.session);
        if !has_rows
            && notice.is_none()
            && !self.turn_queue.undo.contains_key(&self.session)
            && !self
                .turn_queue
                .recovered
                .values()
                .any(|item| item.turn.session_id == self.session)
        {
            return div().into_any_element();
        }
        let mut rows = div()
            .id("composer-pending")
            .test_support()
            .w_full()
            .max_h(px(135.))
            .flex_shrink_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(0.));
        for item in self
            .turn_queue
            .guidance
            .values()
            .filter(|item| item.turn.session_id == self.session)
        {
            rows = rows.child(
                div()
                    .id(ElementId::Name(
                        format!("pending-queued-guidance-{}", item.id).into(),
                    ))
                    .test_support()
                    .text_size(px(12.))
                    .text_color(rgb(muted()))
                    .child(format!("Updating task… {}", item.turn.prompt)),
            );
        }
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
                            .text_size(px(13.))
                            .child(div().single_line().child(text)),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(muted()))
                            .child("Steering…"),
                    ),
            );
        }
        for (index, item) in items
            .into_iter()
            .flatten()
            .enumerate()
            .filter(|(index, _)| *index == 0 || self.turn_queue.show_all.contains(&self.session))
        {
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
            rows = rows.child(
                div().flex_shrink_0().flex().flex_col().gap_1().child(
                    div()
                        .id(("queued-message", index))
                        .flex()
                        .items_center()
                        .gap_2()
                        .min_h(px(34.))
                        .flex_shrink_0()
                        .child(div().text_size(px(12.)).text_color(rgb(muted())).child("↳"))
                        .child(
                            div()
                                .id(("queued-message-text", index))
                                .cursor_pointer()
                                .on_click({
                                    let item = item.clone();
                                    cx.listener(move |view, _, window, cx| {
                                        view.preview_queued_turn(item.clone(), window, cx);
                                    })
                                })
                                .flex_1()
                                .min_w_0()
                                .text_size(px(13.))
                                .child(div().single_line().child(title.clone()))
                                .tooltip(move |_, cx| {
                                    cx.new(|_| XyloTooltip(title.clone().into())).into()
                                }),
                        )
                        .when(
                            image_count > 0 && !item.turn.prompt.trim().is_empty(),
                            |row| {
                                row.child(div().text_size(px(12.)).text_color(rgb(muted())).child(
                                    format!(
                                        "{image_count} image{}",
                                        if image_count == 1 { "" } else { "s" }
                                    ),
                                ))
                            },
                        )
                        .child(div().text_size(px(12.)).text_color(rgb(muted())).child(""))
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
                            button(("preview-queued", index), "⋯")
                                .aria_label("Review queued message")
                                .min_h(px(28.))
                                .px(px(7.))
                                .on_click({
                                    let item = item.clone();
                                    cx.listener(move |view, _, window, cx| {
                                        view.preview_queued_turn(item.clone(), window, cx);
                                    })
                                }),
                        )
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
                ),
            );
        }
        let mut outer = div()
            .id("pending-messages-panel")
            .test_support()
            .mx(px(if self.chat_width.get() < 520. {
                3.
            } else {
                10.
            }))
            .flex_shrink_0()
            .mb(px(8.))
            .pt(px(9.))
            .px(px(12.))
            .pb(px(9.))
            .rounded(px(16.))
            .bg(rgb(well()))
            .border_1()
            .border_color(rgb(border_strong()))
            .flex()
            .flex_col()
            .gap(px(4.))
            .when(has_items, |panel| {
                panel.child(
                    div()
                        .id("next-messages-header")
                        .test_support()
                        .flex()
                        .items_center()
                        .gap(px(7.))
                        .child(
                            subtitle(format!("Next messages · {}", items.map_or(0, Vec::len)))
                                .text_size(px(12.))
                                .flex_1(),
                        )
                        .child(
                            button(
                                "pause-queue",
                                if items.into_iter().flatten().any(|item| item.paused) {
                                    "Resume"
                                } else {
                                    "Pause"
                                },
                            )
                            .min_h(px(28.))
                            .text_size(px(12.))
                            .disabled(self.turn_queue.pauses.contains_key(&self.session))
                            .on_click(cx.listener(|view, _, _, cx| {
                                let paused = view
                                    .turn_queue
                                    .items
                                    .get(&view.session)
                                    .is_some_and(|items| items.iter().any(|item| item.paused));
                                view.set_queue_paused(!paused, cx);
                            })),
                        )
                        .when(items.is_some_and(|x| x.len() > 1), |row| {
                            row.child(
                                button(
                                    "show-all-queued",
                                    if self.turn_queue.show_all.contains(&self.session) {
                                        "Show less".to_string()
                                    } else {
                                        "Show all".to_string()
                                    },
                                )
                                .min_h(px(28.))
                                .px(px(7.))
                                .text_size(px(12.))
                                .on_click(cx.listener(
                                    |view, _, _, cx| {
                                        if !view.turn_queue.show_all.remove(&view.session) {
                                            view.turn_queue.show_all.insert(view.session);
                                        }
                                        cx.notify();
                                    },
                                )),
                            )
                        }),
                )
            })
            .when(has_rows, |panel| panel.child(rows));
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
            outer = outer.child(
                div()
                    .id("queue-paused")
                    .test_support()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(subtitle(reason).text_size(px(13.))),
                    )
                    .when_some(first, |row, first| {
                        let pending = self
                            .turn_queue
                            .operations
                            .values()
                            .any(|entry| *entry == (self.session, first));
                        row.child(
                            button("resume-queue", "Resume next messages")
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
            outer = outer.child(
                subtitle(notice.clone())
                    .id("queue-notice")
                    .test_support()
                    .text_xs(),
            );
        }
        let mut recovery_rows = div()
            .id("queue-edit-recovery")
            .test_support()
            .max_h(px(100.))
            .overflow_y_scroll()
            .flex()
            .flex_col();
        for item in self
            .turn_queue
            .recovered
            .values()
            .filter(|item| item.turn.session_id == self.session)
        {
            let payload = drafts::MessagePayload {
                text: item.turn.prompt.clone(),
                attachments: item.turn.attachments.clone(),
            };
            recovery_rows = recovery_rows.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .single_line()
                            .text_size(px(12.))
                            .text_color(rgb(muted()))
                            .child(if item.turn.prompt.is_empty() {
                                "Image message".into()
                            } else {
                                item.turn.prompt.clone()
                            }),
                    )
                    .child(
                        button(
                            ElementId::Name(format!("recover-queue-edit-{}", item.id).into()),
                            "Recover draft",
                        )
                        .min_h(px(32.))
                        .flex_shrink_0()
                        .on_click(cx.listener(
                            move |view, _, window, cx| {
                                view.replace_composer_payload(payload.clone(), window, cx)
                            },
                        )),
                    ),
            );
        }
        if self
            .turn_queue
            .recovered
            .values()
            .any(|item| item.turn.session_id == self.session)
        {
            outer = outer.child(recovery_rows);
        }
        if let Some(&item) = self.turn_queue.undo.get(&self.session) {
            let pending = self.turn_queue.operation_pending(self.session, item);
            outer = outer.child(
                div()
                    .id("queue-removal-feedback")
                    .test_support()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(subtitle("Message removed"))
                    .child(
                        button("undo-remove-queued", "Undo")
                            .min_h(px(32.))
                            .disabled(pending)
                            .on_click(
                                cx.listener(move |view, _, _, cx| view.undo_queued_turn(item, cx)),
                            ),
                    )
                    .child(div().flex_1())
                    .child(
                        icons::icon_button(
                            "dismiss-queue-removal",
                            AppIcon::Close,
                            "Dismiss removed message",
                        )
                        .disabled(pending)
                        .on_click(cx.listener(move |view, _, _, cx| {
                            if view.turn_queue.undo.get(&view.session) == Some(&item)
                                && !view.turn_queue.operation_pending(view.session, item)
                            {
                                view.turn_queue.undo.remove(&view.session);
                                cx.notify();
                            }
                        })),
                    ),
            );
        }
        outer.into_any_element()
    }
}

#[cfg(all(test, feature = "validation"))]
impl Xylo {
    pub(super) fn validation_edit_queued_turn_while_pending(
        &mut self,
        item: QueuedTurn,
        cx: &mut Context<Self>,
    ) -> bool {
        let request = Uuid::new_v4();
        self.turn_queue
            .operations
            .insert(request, (item.turn.session_id, item.id));
        self.edit_queued_turn(item, cx);
        self.turn_queue.edits.contains_key(&request)
    }
    pub(super) fn validation_queue_restore_pending(&mut self, session: Uuid, item: Uuid) -> Uuid {
        let request = Uuid::new_v4();
        self.turn_queue.undo.insert(session, item);
        self.turn_queue.operations.insert(request, (session, item));
        self.turn_queue.restores.insert(request);
        request
    }
    pub(super) fn validation_queue_pause_pending(&mut self, session: Uuid) -> Uuid {
        let request = Uuid::new_v4();
        self.turn_queue.pauses.insert(session, request);
        request
    }
    pub(super) fn validation_recovered_queue_ids(&self) -> Vec<Uuid> {
        self.turn_queue.recovered.keys().copied().collect()
    }
    pub(super) fn validation_track_queued_guidance(&mut self, item: QueuedTurn) {
        self.turn_queue.guidance.insert(item.id, item);
    }
    pub(super) fn validation_queue_edit_ack(
        &mut self,
        item: QueuedTurn,
        result: Result<Vec<QueuedTurn>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = Uuid::new_v4();
        let session = item.turn.session_id;
        self.turn_queue
            .operations
            .insert(request, (session, item.id));
        self.turn_queue.removals.insert(request);
        self.turn_queue.edits.insert(request, item);
        self.turn_queue_result(request, session, result, window, cx);
    }
    pub(super) fn validation_queue_restore_ack(
        &mut self,
        session: Uuid,
        item: Uuid,
        result: Result<Vec<QueuedTurn>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = Uuid::new_v4();
        self.turn_queue.undo.insert(session, item);
        self.turn_queue.operations.insert(request, (session, item));
        self.turn_queue.restores.insert(request);
        self.turn_queue_result(request, session, result, window, cx);
    }
    pub(super) fn validation_recovered_queue_edit(
        &self,
        session: Uuid,
    ) -> Option<drafts::MessagePayload> {
        self.turn_queue
            .recovered
            .values()
            .find(|item| item.turn.session_id == session)
            .map(|item| drafts::MessagePayload {
                text: item.turn.prompt.clone(),
                attachments: item.turn.attachments.clone(),
            })
    }
}
