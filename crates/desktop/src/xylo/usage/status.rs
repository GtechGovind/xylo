//! Compact chat status and bounded asynchronous command refresh.
use super::*;

impl Xylo {
    pub(in crate::xylo) fn composer_status(&self, cx: &Context<Self>) -> AnyElement {
        let weak = cx.entity().downgrade();
        let usage = gpui_kit::component::popover::Popover::new("usage-popover")
            .anchor(Anchor::BottomRight)
            .trigger(
                MenuButton::new("usage-totals-button")
                    .ghost()
                    .h(px(22.))
                    .child("Usage")
                    .accessibility_label("Conversation and daily usage"),
            )
            .content(move |_, window, cx| {
                weak.update(cx, |view, _| {
                    context_panel::receipt_viewport("usage-totals-panel", 320., window)
                        .child(
                            div()
                                .id("usage-totals-content")
                                .test_support()
                                .w_full()
                                .p_3()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(view.usage_totals())
                                .child(view.quota_rows()),
                        )
                        .into_any_element()
                })
                .unwrap_or_else(|_| div().into_any_element())
            });
        let turn = self.turns.get(&self.session);
        let percent = turn
            .and_then(|turn| turn.context.as_ref())
            .filter(|context| context.capacity > 0)
            .map(|context| {
                format!(
                    "{}% context",
                    (context.used().saturating_mul(100) / context.capacity).min(100)
                )
            })
            .unwrap_or_else(|| "Context —".into());
        let cost = self
            .usage_ui
            .report
            .as_ref()
            .filter(|(session, _)| *session == self.session)
            .map(|(_, report)| CostEstimate {
                usd: report.conversation.estimated_usd,
                unpriced_calls: report.conversation.unpriced_calls,
            });
        let branch = self
            .usage_ui
            .report
            .as_ref()
            .filter(|(session, _)| *session == self.session)
            .and(self.usage_ui.branch.as_deref())
            .unwrap_or("No branch");
        let model = turn
            .and_then(|turn| turn.active_model.as_deref())
            .unwrap_or_else(|| {
                if self.settings.provider.model.is_empty() {
                    "No model"
                } else {
                    &self.settings.provider.model
                }
            });
        let text = format!(
            "{} · {} · {} · {} · {} · {} jobs",
            model,
            self.current_permission_mode().label(),
            percent,
            cost_label(cost),
            branch,
            self.running_job_count()
        );
        div()
            .id("composer-status-line")
            .test_support()
            .pt_1()
            .px_2()
            .flex()
            .flex_col()
            .gap_1()
            .text_size(px(11.))
            .text_color(rgb(muted()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w_0().single_line().child(text))
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
