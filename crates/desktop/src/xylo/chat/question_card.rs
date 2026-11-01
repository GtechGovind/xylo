//! Native question cards with selectable options and free-text Other answers.
use super::*;
use crate::state::ToolCard;
use xylo_core::{InteractionAnswer, UserQuestion};

impl Xylo {
    pub(super) fn answer_interaction(
        &mut self,
        id: Uuid,
        answer: InteractionAnswer,
        cx: &mut Context<Self>,
    ) {
        if let Some(card) = self
            .turns
            .get_mut(&self.session)
            .and_then(|turn| turn.tools.iter_mut().find(|card| card.id == id))
        {
            if !card.approval {
                return;
            }
            card.approval = false;
        }
        self.engine_command(
            WorkerCommand::Answer {
                action_id: id,
                answer,
            },
            cx,
        );
        self.invalidate_live_history();
        self.sync_history_list(cx);
        cx.notify();
    }

    pub(super) fn question_card(
        &self,
        i: usize,
        tool: &ToolCard,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = tool.id;
        let questions: Vec<UserQuestion> =
            serde_json::from_value(tool.input["questions"].clone()).unwrap_or_default();
        let mut card = div().p_3().flex().flex_col().gap_3().child(subtitle(
            "Choose your answers. For Other, type in the composer and click Use composer text.",
        ));
        for (index, question) in questions.into_iter().enumerate() {
            let multi = question.multi_select;
            let mut section = div().flex().flex_col().gap_2().child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .child(format!("{} · {}", question.header, question.question)),
            );
            for (choice, option) in question.options.into_iter().enumerate() {
                let selected = tool
                    .question_answers
                    .get(index)
                    .is_some_and(|answer| answer.selected.contains(&choice));
                section = section.child(
                    secondary(
                        ElementId::Name(format!("question-{i}-{index}-{choice}").into()),
                        format!(
                            "{}{} — {}",
                            if selected { "✓ " } else { "" },
                            option.label,
                            option.description
                        ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(answer) = this
                            .turns
                            .get_mut(&this.session)
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
            let other = tool
                .question_answers
                .get(index)
                .and_then(|answer| answer.other.clone());
            section = section
                .when_some(other, |section, text| {
                    section.child(subtitle(format!("Other: {text}")))
                })
                .child(
                    secondary(
                        ElementId::Name(format!("question-other-{i}-{index}").into()),
                        "Other · Use composer text",
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let text = this.composer.read(cx).value().trim().to_owned();
                        if text.is_empty() {
                            return;
                        }
                        if let Some(answer) = this
                            .turns
                            .get_mut(&this.session)
                            .and_then(|turn| turn.tools.iter_mut().find(|tool| tool.id == id))
                            .and_then(|tool| tool.question_answers.get_mut(index))
                        {
                            if !multi {
                                answer.selected.clear();
                            }
                            answer.other = Some(text);
                        }
                        this.invalidate_live_history();
                        cx.notify();
                    })),
                );
            card = card.child(section);
        }
        let answers = tool.question_answers.clone();
        let ready = !answers.is_empty()
            && answers.iter().all(|answer| {
                !answer.selected.is_empty()
                    || answer
                        .other
                        .as_ref()
                        .is_some_and(|text| !text.trim().is_empty())
            });
        card.child(
            div()
                .flex()
                .gap_2()
                .child(
                    primary(("answer-questions", i), "Submit answers")
                        .disabled(!ready)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.answer_interaction(
                                id,
                                InteractionAnswer::Questions {
                                    answers: answers.clone(),
                                },
                                cx,
                            )
                        })),
                )
                .child(
                    secondary(("cancel-questions", i), "Decline").on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.answer_interaction(id, InteractionAnswer::Reject, cx)
                        },
                    )),
                ),
        )
        .into_any_element()
    }
}
