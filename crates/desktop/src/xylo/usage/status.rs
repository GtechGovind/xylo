//! Compact chat status and bounded asynchronous command refresh.
use super::*;

impl Xylo {
    pub(in crate::xylo) fn composer_status(&self, cx: &Context<Self>) -> AnyElement {
        use gpui_kit::component::WindowExt;
        let usage = button("usage-totals-button", "Usage")
            .h(px(26.))
            .min_h(px(26.))
            .px(px(5.))
            .text_size(px(11.))
            .font_weight(FontWeight::NORMAL)
            .text_color(rgb(muted()))
            .accessibility_label("Conversation and daily usage")
            .on_click(cx.listener(|view, _, window, cx| {
                let weak = cx.entity().downgrade();
                window.open_dialog(cx, move |dialog, window, cx| {
                    let content = weak
                        .update(cx, |view, _| {
                            super::super::context_panel::receipt_viewport(
                                "usage-details",
                                420.,
                                window,
                            )
                            .child(
                                div()
                                    .id("usage-content")
                                    .test_support()
                                    .flex()
                                    .flex_col()
                                    .gap(px(12.))
                                    .child(view.quota_rows())
                                    .child(view.usage_totals()),
                            )
                            .into_any_element()
                        })
                        .unwrap_or_else(|_| div().into_any_element());
                    dialog
                        .title("Usage")
                        .width(px(480.))
                        .rounded(px(16.))
                        .bg(rgb(panel()))
                        .border_color(rgb(border_strong()))
                        .child(content)
                });
                let _ = view;
            }));
        div()
            .id("composer-status-line")
            .test_support()
            .pt(px(3.))
            .px(px(8.))
            .flex()
            .flex_col()
            .gap_1()
            .min_h(px(30.))
            .text_size(px(chat::tokens::META_SIZE))
            .text_color(rgb(muted()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .when(
                        self.composer_focused && self.chat_width.get() >= 750.,
                        |row| {
                            let key = |label: &'static str| {
                                div()
                                    .px(px(4.))
                                    .py(px(1.))
                                    .rounded(px(3.))
                                    .border_1()
                                    .border_color(rgb(border_strong()))
                                    .text_size(px(11.))
                                    .child(label)
                            };
                            row.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .text_size(px(11.))
                                    .child(key("↵"))
                                    .child("Send")
                                    .child(div().w(px(3.)))
                                    .child(key("⇧ ↵"))
                                    .child("New line"),
                            )
                        },
                    )
                    .child(div().flex_1())
                    .child(self.composer_context(cx))
                    .child(
                        button("chat-saved-drafts", "Saved drafts")
                            .h(px(26.))
                            .min_h(px(26.))
                            .px(px(5.))
                            .text_size(px(11.))
                            .font_weight(FontWeight::NORMAL)
                            .text_color(rgb(muted()))
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.open_saved_drafts_dialog(window, cx)
                            })),
                    )
                    .child(usage),
            )
            .children(
                self.usage_ui
                    .status_text
                    .as_ref()
                    .map(|text| div().single_line().child(text.clone())),
            )
            .into_any_element()
    }

    pub(super) fn running_job_count(&self) -> usize {
        self.turns.get(&self.session).map_or(0, |turn| {
            turn.shell_jobs.iter().filter(|job| job.running).count()
        })
    }

    pub(super) fn refresh_status_command(&mut self, cx: &mut Context<Self>) {
        let command = self
            .settings
            .status_line_command
            .clone()
            .filter(|command| !command.trim().is_empty());
        let Some(command) = command else {
            self.usage_ui.status_text = None;
            self.usage_ui.status_tick = None;
            self.usage_ui.status_context = None;
            return;
        };
        let Some(workspace) = self.workspace.clone() else {
            self.usage_ui.status_text = None;
            self.usage_ui.status_context = None;
            return;
        };
        let context = (self.session, workspace.clone(), command.clone());
        if self.usage_ui.status_context.as_ref() != Some(&context) {
            self.usage_ui.status_text = None;
            self.usage_ui.status_last = None;
            self.usage_ui.status_context = Some(context);
        }
        if self.usage_ui.status_request.is_some()
            || self
                .usage_ui
                .status_last
                .is_some_and(|time| time.elapsed() < std::time::Duration::from_millis(300))
        {
            return;
        }
        let id = Uuid::new_v4();
        let session = self.session;
        let context = self
            .turns
            .get(&session)
            .and_then(|turn| turn.context.as_ref());
        let model = self
            .turns
            .get(&session)
            .and_then(|turn| turn.active_model.as_deref())
            .unwrap_or(&self.settings.provider.model);
        let payload = serde_json::json!({"session_id":session,"model":model,"mode":self.current_permission_mode(),"context_used":context.map(|context|context.used()),"context_capacity":context.map(|context|context.capacity),"estimated_cost":self.usage_ui.report.as_ref().filter(|(id,_)|*id==session).map(|(_,report)|report.conversation.estimated_usd),"branch":self.usage_ui.branch,"running_jobs":self.running_job_count()});
        self.usage_ui.status_request = Some((id, session, workspace.clone(), command));
        self.usage_ui.status_last = Some(std::time::Instant::now());
        self.host_call(cx, |host| host.status_line(id, workspace, payload));
        self.usage_ui.status_tick = Some(cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(300))
                .await;
            let _ = view.update(cx, |view, cx| {
                view.usage_ui.status_tick = None;
                cx.notify();
            });
        }));
    }

    pub(in crate::xylo) fn status_line_received(
        &mut self,
        id: Uuid,
        result: Result<Option<String>, String>,
        cx: &mut Context<Self>,
    ) {
        let Some((pending, session, workspace, command)) = self.usage_ui.status_request.take()
        else {
            return;
        };
        if pending != id {
            self.usage_ui.status_request = Some((pending, session, workspace, command));
            return;
        }
        if session == self.session
            && self.workspace.as_ref() == Some(&workspace)
            && self.settings.status_line_command.as_ref() == Some(&command)
        {
            self.usage_ui.status_text = match result {
                Ok(Some(text)) => Some(text),
                Ok(None) => self.usage_ui.status_text.clone(),
                Err(_) => Some("Status command failed · check General settings".into()),
            };
        }
        cx.notify();
    }
}
