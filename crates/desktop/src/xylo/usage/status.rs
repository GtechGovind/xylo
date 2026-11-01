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
            .content(move |_, _, cx| {
                weak.update(cx, |view, _| {
                    div()
                        .w(px(320.))
                        .p_3()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(view.usage_totals())
                        .child(view.quota_rows())
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
            self.settings.permissions.label(),
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
        0
    } // A supplies the per-chat job registry at integration.

    pub(super) fn refresh_status_command(&mut self, cx: &mut Context<Self>) {
        if self
            .settings
            .status_line_command
            .as_ref()
            .is_none_or(|command| command.trim().is_empty())
        {
            self.usage_ui.status_text = None;
            self.usage_ui.status_tick = None;
            return;
        }
        if self.usage_ui.status_request.is_some()
            || self
                .usage_ui
                .status_last
                .is_some_and(|time| time.elapsed() < std::time::Duration::from_millis(300))
        {
            return;
        }
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        let id = Uuid::new_v4();
        let session = self.session;
        let context = self
            .turns
            .get(&session)
            .and_then(|turn| turn.context.as_ref());
        let payload = serde_json::json!({"session_id":session,"model":self.settings.provider.model,"mode":self.settings.permissions,"context_used":context.map(|context|context.used()),"context_capacity":context.map(|context|context.capacity),"estimated_cost":self.usage_ui.report.as_ref().filter(|(id,_)|*id==session).map(|(_,report)|report.conversation.estimated_usd),"branch":self.usage_ui.branch,"running_jobs":self.running_job_count()});
        self.usage_ui.status_request = Some((id, session));
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
        let Some((pending, session)) = self.usage_ui.status_request else {
            return;
        };
        if pending != id {
            return;
        }
        self.usage_ui.status_request = None;
        if session == self.session && self.settings.status_line_command.is_some() {
            self.usage_ui.status_text = match result {
                Ok(text) => text,
                Err(_) => Some("Status command failed · check General settings".into()),
            };
        }
        cx.notify();
    }
}
