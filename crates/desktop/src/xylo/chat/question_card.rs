//! Native question cards with action-scoped text inputs.
use super::*;
use crate::state::ToolCard;
use xylo_core::{InteractionAnswer, UserQuestion};

impl Xylo {
    /// Every field owns its editor and subscription for one chat/action/slot.
    /// Keeping these until an acknowledgement also preserves rejected replies.
    pub(in crate::xylo) fn interaction_input(
        &self,
        session: Uuid,
        id: Uuid,
        slot: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TextareaState> {
        let key = (session, id, slot);
        if let Some((input, _)) = self.interaction_inputs.borrow().get(&key) {
            return input.clone();
        }
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(if slot == usize::MAX {
                    "Suggest changes to the plan…"
                } else {
                    "Tell Xylo what you prefer…"
                })
                .auto_grow(2, 6)
                .submit_on_enter(true)
        });
        let subscription = cx.subscribe_in(&input, window, move |this, input, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                if slot != usize::MAX {
                    let text = input.read(cx).value().to_string();
                    if let Some(answer) = this
                        .turns
                        .get_mut(&session)
                        .and_then(|turn| turn.tools.iter_mut().find(|tool| tool.id == id))
                        .and_then(|tool| tool.question_answers.get_mut(slot))
                    {
                        answer.other = Some(text);
                    }
                }
                this.invalidate_live_history();
                cx.notify();
            }
            if matches!(event, InputEvent::PressEnter { shift: false, .. })
                && this.session == session
            {
                this.submit_local_interaction(session, id, slot, cx);
            }
        });
        self.interaction_inputs
            .borrow_mut()
            .insert(key, (input.clone(), subscription));
        input
    }

    pub(super) fn submit_local_interaction(
        &mut self,
        session: Uuid,
        id: Uuid,
        slot: usize,
        cx: &mut Context<Self>,
    ) {
        if self.session != session {
            return;
        }
        let answer = if slot == usize::MAX {
            let feedback = self
                .interaction_inputs
                .borrow()
                .get(&(session, id, slot))
                .map(|(input, _)| input.read(cx).value().trim().to_owned())
                .unwrap_or_default();
            InteractionAnswer::KeepPlanning { feedback }
        } else {
            let Some(tool) = self
                .turns
                .get(&session)
                .and_then(|turn| turn.tools.iter().find(|tool| tool.id == id))
            else {
                return;
            };
            let mut answers = tool.question_answers.clone();
            for (index, answer) in answers.iter_mut().enumerate() {
                if answer.other.is_some()
                    && let Some((input, _)) =
                        self.interaction_inputs.borrow().get(&(session, id, index))
                {
                    answer.other = Some(input.read(cx).value().trim().to_owned());
                }
            }
            if answers.is_empty()
                || answers.iter().any(|answer| {
                    answer.selected.is_empty()
                        && answer
                            .other
                            .as_ref()
                            .is_none_or(|text| text.trim().is_empty())
                })
            {
                self.error = Some("Write an answer before continuing.".into());
                cx.notify();
                return;
            }
            InteractionAnswer::Questions { answers }
        };
        self.answer_interaction(id, answer, cx);
    }

    pub(super) fn answer_interaction(
        &mut self,
        id: Uuid,
        answer: InteractionAnswer,
        cx: &mut Context<Self>,
    ) {
        if answer.validate_text().is_err() {
            self.error = Some(
                if match &answer {
                    InteractionAnswer::KeepPlanning { feedback } => feedback.len() > 8192,
                    InteractionAnswer::Questions { answers } => answers
                        .iter()
                        .any(|answer| answer.other.as_ref().is_some_and(|text| text.len() > 8192)),
                    _ => false,
                } {
                    "This answer is too long. Shorten it and try again.".into()
                } else {
                    "Write an answer before continuing.".into()
                },
            );
            cx.notify();
            return;
        }
        if self.interaction_pending.contains(&(self.session, id))
            || !self
                .turns
                .get(&self.session)
                .is_some_and(|turn| turn.tools.iter().any(|card| card.id == id && card.approval))
        {
            return;
        }
        self.interaction_pending.insert((self.session, id));
        if let Err(error) = self.try_engine_command(
            WorkerCommand::Answer {
                action_id: id,
                answer,
            },
            cx,
        ) {
            self.interaction_pending.remove(&(self.session, id));
            self.error = Some(error.to_string());
        }
        self.invalidate_live_history();
        self.sync_history_list(cx);
        cx.notify();
    }

    pub(super) fn question_card(
        &self,
        i: usize,
        tool: &ToolCard,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = tool.id;
        let session = self.session;
        let pending = self.interaction_pending.contains(&(session, id));
        let questions: Vec<UserQuestion> =
            serde_json::from_value(tool.input["questions"].clone()).unwrap_or_default();
        let mut card = div()
            .p(px(20.))
            .my(px(20.))
            .border_1()
            .border_color(rgb(border_strong()))
            .rounded(px(14.))
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .text_size(px(12.))
                    .text_color(rgb(muted()))
                    .child(icons::small(AppIcon::Chat).size(px(14.)))
                    .child(if questions.len() == 1 {
                        "One quick question"
                    } else {
                        "A few quick questions"
                    }),
            );
        for (index, question) in questions.into_iter().enumerate() {
            let multi = question.multi_select;
            let mut section = div()
                .flex()
                .flex_col()
                .gap(px(7.))
                .child(
                    div()
                        .text_size(px(17.))
                        .font_weight(FontWeight::MEDIUM)
                        .child(question.question.clone()),
                )
                .child(
                    subtitle("Choose an answer or write your own.")
                        .text_size(px(13.))
                        .mb(px(9.)),
                );
            for (choice, option) in question.options.into_iter().enumerate() {
                let selected = tool
                    .question_answers
                    .get(index)
                    .is_some_and(|answer| answer.selected.contains(&choice));
                section = section.child(
                    secondary(
                        ElementId::Name(format!("question-{i}-{index}-{choice}").into()),
                        "",
                    )
                    .justify_start()
                    .w_full()
                    .px(px(12.))
                    .py(px(11.))
                    .rounded(px(9.))
                    .border_1()
                    .border_color(rgb(if selected { muted() } else { border() }))
                    .bg(rgb(if selected { panel() } else { crate::ui::bg() }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(11.))
                            .child(if selected { "●" } else { "○" })
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.))
                                    .child(option.label)
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(rgb(muted()))
                                            .child(option.description),
                                    ),
                            ),
                    )
                    .selected(selected)
                    .aria_toggled(if selected {
                        gpui::accesskit::Toggled::True
                    } else {
                        gpui::accesskit::Toggled::False
                    })
                    .disabled(pending)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(answer) = this
                            .turns
                            .get_mut(&session)
                            .and_then(|turn| turn.tools.iter_mut().find(|tool| tool.id == id))
                            .and_then(|tool| tool.question_answers.get_mut(index))
                        {
                            if answer.selected.contains(&choice) {
                                answer.selected.retain(|selected| *selected != choice);
                            } else {
                                if !multi {
                                    answer.selected.clear();
                                    answer.other = None;
                                }
                                answer.selected.push(choice);
                            }
                        }
                        this.invalidate_live_history();
                        cx.notify();
                    })),
                );
            }
            let expanded = tool
                .question_answers
                .get(index)
                .is_some_and(|answer| answer.other.is_some());
            section = section.child(
                secondary(
                    ElementId::Name(format!("question-other-{i}-{index}").into()),
                    if expanded {
                        "● Write another answer"
                    } else {
                        "○ Write another answer"
                    },
                )
                .self_start()
                .justify_start()
                .aria_expanded(expanded)
                .disabled(pending)
                .on_click(cx.listener(move |this, _, window, cx| {
                    let input = this.interaction_input(session, id, index, window, cx);
                    let text = input.read(cx).value().to_string();
                    if let Some(answer) = this
                        .turns
                        .get_mut(&session)
                        .and_then(|turn| turn.tools.iter_mut().find(|tool| tool.id == id))
                        .and_then(|tool| tool.question_answers.get_mut(index))
                    {
                        if !multi {
                            answer.selected.clear();
                        }
                        answer.other = Some(text);
                    }
                    input.update(cx, |input, cx| input.focus(window, cx));
                    this.invalidate_live_history();
                    cx.notify();
                })),
            );
            if expanded {
                let input = self.interaction_input(session, id, index, window, cx);
                section = section.child(
                    Textarea::new(&input)
                        .text_size(px(13.))
                        .aria_label(format!("Answer {}", index + 1))
                        .disabled(pending),
                );
            }
            card = card.child(section);
        }
        card.child(
            div()
                .flex()
                .gap_2()
                .child(
                    primary(
                        ("answer-questions", i),
                        if pending {
                            "Sending answers…"
                        } else {
                            if tool.question_answers.len() == 1 {
                                "Send answer"
                            } else {
                                "Send answers"
                            }
                        },
                    )
                    .disabled(pending)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.submit_local_interaction(session, id, 0, cx)
                    })),
                )
                .child(
                    secondary(("cancel-questions", i), "Decline to answer")
                        .disabled(pending)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.answer_interaction(id, InteractionAnswer::Reject, cx)
                        })),
                ),
        )
        .into_any_element()
    }
}
