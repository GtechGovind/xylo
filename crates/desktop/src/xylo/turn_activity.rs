//! Saved-turn activity is loaded only when opened, in metadata-only pages.
use super::*;
#[derive(Default)]
pub(super) struct ActivityUi {
    subagent_request: Option<(Uuid, Uuid)>,
    subagent_error: Option<(Uuid, String)>,
    pages: BTreeMap<(Uuid, Uuid), ActivityPage>,
    action_request: Option<(Uuid, Uuid, Uuid, Uuid)>,
}
#[derive(Default)]
struct ActivityPage {
    after: Option<i64>,
    open: bool,
    request: Option<Uuid>,
    page: Option<xylo_runtime::host::TurnActivityPage>,
    error: Option<String>,
}
fn activity_button_base(id: ElementId) -> gpui_kit::base::Button {
    focus_ring(gpui_kit::base::Button::new(id))
        .self_start()
        .h(px(28.))
        .px(px(5.))
        .ml(px(-5.))
        .rounded(px(6.))
        .flex()
        .items_center()
        .justify_start()
        .cursor_pointer()
        .text_size(px(13.))
        .text_color(rgb(muted()))
        .hover(|style| style.bg(rgb(hover())).text_color(rgb(text())))
}
fn activity_button(id: ElementId, label: impl Into<SharedString>) -> gpui_kit::base::Button {
    let label: SharedString = label.into();
    activity_button_base(id).child(label)
}
impl Xylo {
    pub(super) fn load_subagent_activity(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.host.as_ref() else {
            return;
        };
        let id = Uuid::new_v4();
        self.turn_activity.subagent_request = Some((id, self.session));
        self.turn_activity.subagent_error = None;
        if let Err(error) = host.subagent_activity(id, self.session) {
            self.turn_activity.subagent_request = None;
            self.turn_activity.subagent_error = Some((self.session, error.to_string()));
        }
        cx.notify();
    }
    pub(super) fn subagent_activity_loading(&self) -> bool {
        self.turn_activity
            .subagent_request
            .is_some_and(|(_, session)| session == self.session)
    }
    pub(super) fn subagent_activity_error(&self) -> Option<&str> {
        self.turn_activity
            .subagent_error
            .as_ref()
            .filter(|(session, _)| *session == self.session)
            .map(|(_, error)| error.as_str())
    }
    pub(super) fn subagent_activity_received(
        &mut self,
        id: Uuid,
        session: Uuid,
        result: Result<Vec<xylo_runtime::host::JournalAction>, String>,
        cx: &mut Context<Self>,
    ) {
        if self.turn_activity.subagent_request != Some((id, session)) {
            return;
        }
        self.turn_activity.subagent_request = None;
        if session != self.session {
            return;
        }
        match result {
            Ok(records) => {
                let turn = self.turns.entry(session).or_default();
                for record in records.into_iter().rev() {
                    if record.intent.session_id != session
                        || record.intent.name != crate::state::DELEGATE_TOOL
                        || turn
                            .subagents
                            .iter()
                            .chain(&turn.earlier_subagents)
                            .any(|agent| agent.id == record.intent.id)
                    {
                        continue;
                    }
                    let raw = record.result.as_ref().map(serde_json::Value::to_string);
                    let presentation = crate::state::tool_presentation::ToolPresentation::prepare(
                        &record.intent.name,
                        &record.intent.arguments,
                        raw.as_deref(),
                        match record.state.as_str() {
                            "succeeded" => Some(true),
                            "failed" => Some(false),
                            _ => None,
                        },
                    );
                    let summary = record
                        .error
                        .or(presentation.error)
                        .map(SharedString::from)
                        .or_else(|| match presentation.body {
                            crate::state::tool_presentation::ToolBody::Markdown(text) => {
                                Some(text.into())
                            }
                            _ => None,
                        });
                    turn.earlier_subagents.push(crate::state::Subagent {
                        id: record.intent.id,
                        task: record
                            .intent
                            .arguments
                            .get("task")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .into(),
                        tools: Vec::new(),
                        summary,
                        state: match record.state.as_str() {
                            "succeeded" => crate::state::SubagentState::Completed,
                            "failed" => crate::state::SubagentState::Failed,
                            _ => crate::state::SubagentState::NotCompleted,
                        },
                        // The journal has an update timestamp, not a start time. Do not invent duration.
                        started_at: 0,
                        finished_at: Some(record.updated_at),
                    });
                }
                turn.earlier_subagents
                    .sort_by_key(|agent| agent.finished_at.unwrap_or(agent.started_at));
                let excess = turn.earlier_subagents.len().saturating_sub(64);
                turn.earlier_subagents.drain(..excess);
            }
            Err(error) => self.turn_activity.subagent_error = Some((session, error)),
        }
        cx.notify();
    }
    pub(super) fn saved_activity_is_open(&self, message: &ChatMessage) -> bool {
        message
            .turn_id
            .and_then(|turn| self.turn_activity.pages.get(&(message.session_id, turn)))
            .is_some_and(|page| page.open)
    }
    fn remeasure_activity(
        &mut self,
        session: Uuid,
        turn: Option<Uuid>,
        message: Option<Uuid>,
        cx: &mut Context<Self>,
    ) {
        if session == self.session
            && let Some(history) = self.histories.get(&session)
        {
            for (index, item) in self.messages.iter().enumerate().filter(|(_, item)| {
                (turn.is_some() && item.turn_id == turn) || Some(item.id) == message
            }) {
                let _ = item;
                if index + 1 < history.list.item_count() {
                    history.list.splice(index + 1..index + 2, 1);
                }
            }
        }
        cx.notify();
    }
    fn load_activity(&mut self, turn: Uuid, after: Option<i64>, cx: &mut Context<Self>) {
        let session = self.session;
        let id = Uuid::new_v4();
        // Only metadata pages are cached. Keep the cache bounded across chats.
        if self.turn_activity.pages.len() >= 32
            && !self.turn_activity.pages.contains_key(&(session, turn))
            && let Some(key) = self
                .turn_activity
                .pages
                .keys()
                .find(|key| key.0 != session)
                .copied()
                .or_else(|| self.turn_activity.pages.keys().next().copied())
        {
            self.turn_activity.pages.remove(&key);
        }
        let page = self.turn_activity.pages.entry((session, turn)).or_default();
        page.open = true;
        page.error = None;
        page.request = Some(id);
        page.after = after;
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Task service unavailable"))
            .and_then(|host| host.turn_activity(id, session, turn, after));
        if let Err(error) = result {
            page.request = None;
            page.error = Some(error.to_string());
        }
        self.remeasure_activity(session, Some(turn), None, cx);
    }
    pub(super) fn activity_received(
        &mut self,
        id: Uuid,
        session: Uuid,
        turn: Uuid,
        result: Result<xylo_runtime::host::TurnActivityPage, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .turn_activity
            .pages
            .get_mut(&(session, turn))
            .filter(|entry| entry.request == Some(id))
        else {
            return;
        };
        entry.request = None;
        match result {
            Ok(page) if page.session_id == session && page.turn_id == turn => {
                entry.page = Some(page);
            }
            _ => entry.error = Some("Activity couldn’t be loaded. Try again.".into()),
        }
        self.remeasure_activity(session, Some(turn), None, cx);
    }
    fn open_activity_action(&mut self, turn: Uuid, action: Uuid, cx: &mut Context<Self>) {
        let session = self.session;
        let id = Uuid::new_v4();
        self.turn_activity.action_request = Some((id, session, turn, action));
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Task service unavailable"))
            .and_then(|host| host.turn_action(id, session, turn, action));
        if let Err(error) = result {
            self.turn_activity.action_request = None;
            self.error = Some(error.to_string());
        }
        cx.notify();
    }
    pub(super) fn activity_action_received(
        &mut self,
        id: Uuid,
        session: Uuid,
        turn: Uuid,
        action: Uuid,
        result: Result<Option<xylo_runtime::host::JournalAction>, String>,
        cx: &mut Context<Self>,
    ) {
        if self.turn_activity.action_request != Some((id, session, turn, action)) {
            return;
        }
        self.turn_activity.action_request = None;
        if session != self.session {
            return;
        }
        match result {
            Ok(Some(record))
                if record.intent.session_id == session
                    && record.intent.turn_id == turn
                    && record.intent.id == action =>
            {
                if matches!(
                    record.intent.name.as_str(),
                    "shell_command" | "run_command" | "shell_output"
                ) && let Some(job) = record
                    .result
                    .as_ref()
                    .and_then(|result| result.get("summary").unwrap_or(result).get("job_id"))
                    .and_then(serde_json::Value::as_str)
                    .and_then(|id| Uuid::parse_str(id).ok())
                {
                    let title = record
                        .intent
                        .arguments
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("Saved command")
                        .to_owned();
                    let origin = record
                        .result
                        .as_ref()
                        .and_then(|result| {
                            result.get("summary").unwrap_or(result).get("job_turn_id")
                        })
                        .and_then(serde_json::Value::as_str)
                        .and_then(|id| Uuid::parse_str(id).ok())
                        .unwrap_or(turn);
                    self.open_retained_command_output(session, origin, job, title, cx);
                    return;
                }
                let text = format!(
                    "{} · {}\n\nInput\n{}\n\nOutput\n{}{}",
                    record.intent.name,
                    record.state,
                    serde_json::to_string_pretty(&record.intent.arguments).unwrap_or_default(),
                    record
                        .result
                        .map(|result| serde_json::to_string_pretty(&result).unwrap_or_default())
                        .unwrap_or_else(|| "No output was saved.".into()),
                    record
                        .error
                        .map(|error| format!("\n\n{error}"))
                        .unwrap_or_default()
                );
                self.show_action_output(session, action, "Saved activity".into(), text, cx);
            }
            _ => {
                self.error = Some("This activity’s details are unavailable.".into());
                cx.notify();
            }
        }
    }
    pub(super) fn saved_activity(&self, message: &ChatMessage, cx: &Context<Self>) -> AnyElement {
        let session = message.session_id;
        // A turn identity alone is not evidence of work: greetings and legacy
        // replies without retained journal metadata have no disclosure.
        let Some(turn) = message.turn_id.filter(|_| message.activity_count > 0) else {
            return div().into_any_element();
        };
        let summary = message.activity_summary.clone().unwrap_or_else(|| {
            format!(
                "{} {}",
                message.activity_count,
                if message.activity_count == 1 {
                    "action"
                } else {
                    "actions"
                }
            )
        });
        let entry = self.turn_activity.pages.get(&(session, turn));
        let open = entry.is_some_and(|entry| entry.open);
        div()
            .id(SharedString::from(format!("saved-activity-{turn}")))
            .test_support()
            .flex()
            .flex_col()
            .items_start()
            .mb(px(18.))
            .text_size(px(13.))
            .text_color(rgb(muted()))
            .child(
                activity_button_base(ElementId::Name(
                    format!("saved-activity-toggle-{turn}").into(),
                ))
                .h(px(32.))
                .gap(px(8.))
                .accessibility_label(format!(
                    "{summary}. Activity {}",
                    if open { "expanded" } else { "collapsed" }
                ))
                .child(
                    icons::small(if message.status == MessageStatus::Complete {
                        AppIcon::Done
                    } else {
                        AppIcon::Info
                    })
                    .size(px(16.)),
                )
                .child(summary)
                .child(
                    icons::small(if open {
                        AppIcon::ChevronDown
                    } else {
                        AppIcon::ChevronRight
                    })
                    .size(px(13.)),
                )
                .on_click(cx.listener(move |view, _, _, cx| {
                    if view.session != session {
                        return;
                    }
                    if let Some(entry) = view.turn_activity.pages.get_mut(&(session, turn))
                        && (entry.page.is_some() || entry.request.is_some())
                    {
                        entry.open = !entry.open;
                        view.remeasure_activity(session, Some(turn), None, cx);
                        return;
                    }
                    view.load_activity(turn, None, cx);
                })),
            )
            .when(open, |body| {
                let mut details = div()
                    .ml(px(7.))
                    .mt(px(8.))
                    .mb(px(8.))
                    .pl(px(16.))
                    .pt(px(6.))
                    .pb(px(4.))
                    .border_l_1()
                    .border_color(rgb(border()))
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap(px(6.));
                if let Some(entry) = entry {
                    if entry.request.is_some() {
                        details = details.child(subtitle("Loading activity…"));
                    }
                    if let Some(error) = &entry.error {
                        details = details.child(subtitle(error.clone()));
                    }
                    if entry.after.is_some() {
                        details =
                            details.child(
                                activity_button(
                                    ElementId::Name(format!("saved-activity-first-{turn}").into()),
                                    "First activity page",
                                )
                                .disabled(entry.request.is_some())
                                .on_click(cx.listener(
                                    move |view, _, _, cx| view.load_activity(turn, None, cx),
                                )),
                            );
                    }
                    if let Some(page) = &entry.page {
                        if page.items.is_empty() {
                            details = details
                                .child(subtitle("No activity details were saved for this task."));
                        }
                        for item in &page.items {
                            let action = item.id;
                            let label = match item.state.as_str() {
                                "succeeded" => "Finished",
                                "failed" => "Failed",
                                "outcome_unknown" => "Outcome unavailable",
                                "pending" | "started" | "running" | "prepared" => "Incomplete",
                                "denied" => "Not allowed",
                                _ => "Recorded",
                            };
                            let verb = crate::state::tool_presentation::ToolPresentation::prepare(
                                &item.name,
                                &serde_json::Value::Null,
                                None,
                                None,
                            )
                            .verb;
                            details = details.child(
                                activity_button(
                                    ElementId::Name(format!("saved-action-{action}").into()),
                                    format!("{verb} · {label}"),
                                )
                                .on_click(cx.listener(
                                    move |view, _, _, cx| {
                                        view.open_activity_action(turn, action, cx)
                                    },
                                )),
                            );
                            if let Some(error) = &item.error {
                                details = details
                                    .child(subtitle(error.clone()).text_color(rgb(danger())));
                            }
                        }
                        if let Some(after) = page.next {
                            details = details.child(
                                activity_button(
                                    ElementId::Name(format!("saved-activity-next-{turn}").into()),
                                    "Next activity page",
                                )
                                .disabled(entry.request.is_some())
                                .on_click(cx.listener(
                                    move |view, _, _, cx| view.load_activity(turn, Some(after), cx),
                                )),
                            );
                        }
                    }
                }
                body.child(details)
            })
            .into_any_element()
    }
}

#[cfg(all(test, not(feature = "validation")))]
mod subagent_restore_tests {
    use super::Xylo;
    use gpui_kit::{AppContext, TestAppContext, WindowOptions};
    use uuid::Uuid;
    #[gpui_kit::test]
    fn saved_results_are_owned_deduplicated_and_do_not_replace_live_work(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (window, view) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Isolated fixture")), window, cx))
            })
            .unwrap()
        });
        cx.update_window(window, |_, _, cx| view.update(cx, |view, cx| {
            let session = view.session;
            let id = Uuid::new_v4();
            let request = Uuid::new_v4();
            let record: xylo_runtime::host::JournalAction = serde_json::from_value(serde_json::json!({
                "intent": { "id": id, "session_id": session, "turn_id": Uuid::new_v4(), "name": "agent_delegate",
                    "arguments": {"task":"Review layout"}, "writes":false, "idempotency_key":"review" },
                "state":"succeeded", "result":{"response":"A retained result"}, "error":null, "updated_at":1234
            })).unwrap();
            view.turn_activity.subagent_request = Some((request, session));
            view.subagent_activity_received(Uuid::new_v4(), session, Ok(vec![record.clone()]), cx);
            assert!(!view.turns.contains_key(&session));
            let mut foreign = record.clone(); foreign.intent.session_id = Uuid::new_v4();
            view.subagent_activity_received(request, session, Ok(vec![foreign, record.clone(), record.clone()]), cx);
            let agents = &view.turns[&session].earlier_subagents;
            assert_eq!(agents.len(), 1);
            assert_eq!(agents[0].summary.as_deref(), Some("A retained result"));
            assert_eq!(agents[0].started_at, 0, "A saved update time is not a measured duration");
            let mut current = agents[0].clone();
            current.state = crate::state::SubagentState::Running;
            let turn = view.turns.get_mut(&session).unwrap(); turn.earlier_subagents.clear(); turn.subagents.push(current);
            view.turn_activity.subagent_request = Some((request, session));
            view.subagent_activity_received(request, session, Ok(vec![record.clone()]), cx);
            assert!(view.turns[&session].earlier_subagents.is_empty());
            assert_eq!(view.turns[&session].subagents[0].state, crate::state::SubagentState::Running);
            view.turn_activity.subagent_request = Some((request, session));
            view.session = Uuid::new_v4();
            view.subagent_activity_received(request, session, Ok(vec![record]), cx);
            assert!(!view.turns.contains_key(&view.session), "Switching chats must reject the prior response");
        })).unwrap();
    }
}
