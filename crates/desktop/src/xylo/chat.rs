use super::*;
use gpui_kit::base::{Disableable, Selectable};
#[path = "tool_result_view.rs"]
mod tool_result_view;
use gpui_kit::base::TestSupportExt;
use gpui_kit::component::{
    IconName,
    button::{Button as MenuButton, ButtonVariants},
    menu::{DropdownMenu, PopupMenuItem},
};

/// Assistant turns are prefixed by the product mark in the left gutter, the
/// way a reply reads in a modern assistant: no name label, no bubble.
fn assistant_row(body: AnyElement) -> AnyElement {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .items_start()
        .gap(px(12.))
        .child(
            div()
                .flex_shrink_0()
                .w(px(20.))
                .pt(px(6.))
                .child(brand_glyph(16.)),
        )
        .child(div().flex_1().min_w_0().child(body))
        .into_any_element()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActivityPosition {
    Standalone,
    Start,
    Continuation,
}

fn activity_eligible(tool: &crate::state::ToolCard) -> bool {
    !tool.approval
        && tool.success != Some(false)
        && tool.presentation.error.is_none()
        && !matches!(tool.name.as_str(), "todo_write" | "image_generate")
}

/// O(1), immediate neighbors only; never projects, filters, or mutates list rows.
fn adjacent_activity(tools: &[crate::state::ToolCard], i: usize) -> ActivityPosition {
    let Some(tool) = tools.get(i).filter(|tool| activity_eligible(tool)) else {
        return ActivityPosition::Standalone;
    };
    let _ = tool;
    if i.checked_sub(1)
        .and_then(|i| tools.get(i))
        .is_some_and(activity_eligible)
    {
        ActivityPosition::Continuation
    } else if i
        .checked_add(1)
        .and_then(|i| tools.get(i))
        .is_some_and(activity_eligible)
    {
        ActivityPosition::Start
    } else {
        ActivityPosition::Standalone
    }
}

#[cfg(test)]
mod activity_tests {
    use super::{ActivityPosition, adjacent_activity};
    use uuid::Uuid;

    fn tool(name: &str) -> crate::state::ToolCard {
        crate::state::ToolCard {
            id: Uuid::nil(),
            name: name.into(),
            input: serde_json::json!({}),
            result: None,
            presentation: Default::default(),
            approval: false,
            rule: None,
            success: None,
            started_at: 0,
            finished_at: None,
        }
    }

    #[test]
    fn adjacent_activity_boundaries_and_runs() {
        assert_eq!(adjacent_activity(&[], 0), ActivityPosition::Standalone);
        assert_eq!(
            adjacent_activity(&[tool("file_read")], 0),
            ActivityPosition::Standalone
        );
        let tools = [
            tool("file_read"),
            tool("search_project"),
            tool("run_command"),
        ];
        assert_eq!(adjacent_activity(&tools, 0), ActivityPosition::Start);
        assert_eq!(adjacent_activity(&tools, 1), ActivityPosition::Continuation);
        assert_eq!(adjacent_activity(&tools, 2), ActivityPosition::Continuation);
        assert_eq!(adjacent_activity(&tools, 3), ActivityPosition::Standalone);
        assert_eq!(
            adjacent_activity(&tools, usize::MAX),
            ActivityPosition::Standalone
        );
    }

    #[test]
    fn adjacent_activity_exclusions_break_runs() {
        let mut approval = tool("file_edit");
        approval.approval = true;
        let mut failure = tool("run_command");
        failure.success = Some(false);
        let mut error = tool("file_read");
        error.presentation.error = Some("failed".into());
        for excluded in [
            approval,
            failure,
            error,
            tool("todo_write"),
            tool("image_generate"),
        ] {
            let tools = [
                tool("file_read"),
                excluded,
                tool("search_project"),
                tool("git_status"),
            ];
            assert_eq!(adjacent_activity(&tools, 0), ActivityPosition::Standalone);
            assert_eq!(adjacent_activity(&tools, 1), ActivityPosition::Standalone);
            assert_eq!(adjacent_activity(&tools, 2), ActivityPosition::Start);
            assert_eq!(adjacent_activity(&tools, 3), ActivityPosition::Continuation);
        }
    }
}

#[derive(Default)]
struct ComposerModelPicker {
    choosing: bool,
    models: std::sync::Arc<Vec<ProviderConfig>>,
}

impl Xylo {
    pub(super) fn chat(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.recovery.open {
            return self.recovery_panel(cx);
        }
        if self.messages.is_empty() && !self.running() && !self.history_loading() {
            let home = self.chat_home(cx);
            if self.recovery_blocks_submit() {
                return div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(self.recovery_banner(cx))
                    .child(div().flex_1().min_h_0().child(home))
                    .into_any_element();
            }
            return home;
        }
        self.sync_history_list(cx);
        let state = self.histories.get(&self.session).unwrap().list.clone();
        let weak = cx.entity().downgrade();
        let history = list(state, move |index, _, cx| {
            weak.update(cx, |view, cx| view.chat_row(index, cx))
                .unwrap_or_else(|_| div().into_any_element())
        })
        .flex_1()
        .min_h_0()
        .w_full();
        div()
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .child(self.recovery_banner(cx))
            .child(history)
            .child(self.history_later_footer(cx))
            .child(
                div()
                    .w_full()
                    .flex()
                    .justify_center()
                    .px(px(28.))
                    .pt(px(6.))
                    .pb(px(20.))
                    .flex_shrink_0()
                    .child(self.prompt_box(cx)),
            )
            .into_any_element()
    }
    fn chat_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let content = if index == 0 {
            let history = self.histories.get(&self.session).unwrap();
            div()
                .flex()
                .justify_center()
                .py_2()
                .child(if history.loading {
                    subtitle("Loading conversation…").into_any_element()
                } else if history.has_more {
                    secondary("older-messages", "Load earlier messages")
                        .h(px(30.))
                        .rounded_full()
                        .text_size(px(12.5))
                        .on_click(cx.listener(|view, _, _, cx| view.load_older(cx)))
                        .into_any_element()
                } else {
                    div().into_any_element()
                })
                .into_any_element()
        } else if let Some(message) = self.messages.get(index - 1) {
            let user = message.role == "user";
            let content = message.content.clone();
            let body = div()
                .id(("message-body", index))
                .test_support()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_2()
                .text_size(px(15.))
                .line_height(relative(if user { 1.5 } else { 1.7 }))
                .text_color(rgb(text()))
                .when(user, |row| {
                    row.max_w(px(640.))
                        .px(px(16.))
                        .py(px(10.))
                        .rounded(px(18.))
                        .bg(rgb(brand_soft()))
                })
                .when(!user, |row| row.w_full())
                .when(self.history_message_highlight(message.id), |row| {
                    row.border_1().border_color(rgb(brand()))
                })
                .when(!message.content.is_empty(), |row| {
                    row.child(
                        TextView::markdown(
                            SharedString::from(message.id.to_string()),
                            message.content.clone(),
                        )
                        .selectable(true),
                    )
                })
                .when(!message.attachments.is_empty(), |row| {
                    row.child(self.message_images(message, cx))
                });
            let copy = focus_ring(gpui_kit::base::Button::new(("copy-message", index)))
                .accessibility_label("Copy message")
                .tooltip(|_, cx| cx.new(|_| XyloTooltip("Copy".into())).into())
                .opacity(0.)
                .group_hover("chat-message", |style| style.opacity(1.))
                .focus(|style| style.opacity(1.))
                .size(px(26.))
                .rounded(px(7.))
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(muted()))
                .hover(|style| style.bg(rgb(hover())).text_color(rgb(text())))
                .child(icon(IconName::Copy).size(px(13.)))
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(content.clone()))
                });
            let body = if user {
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(px(4.))
                    .child(body)
                    .child(div().pr(px(4.)).child(copy))
                    .into_any_element()
            } else {
                assistant_row(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(body)
                        .child(div().flex().child(copy))
                        .into_any_element(),
                )
            };
            div()
                .id(("chat-message", index))
                .test_support()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .group("chat-message")
                .child(body)
                .into_any_element()
        } else if let Some(turn) = self.turns.get(&self.session) {
            let offset = index - self.messages.len() - 1;
            let latest_plan = turn.tools.iter().rposition(|t| t.name == "todo_write");
            let streamed = !turn.text.is_empty() && (turn.running || turn.status == "Cancelled");
            if let Some(tool) = turn.tools.get(offset) {
                // Presentation only: keep the direct list-index mapping and every tool row.
                let activity = adjacent_activity(&turn.tools, offset);
                // Runs of quiet activity are grouped under one caption and a
                // hairline; standalone or attention-worthy cards stand alone.
                let grouped = activity != ActivityPosition::Standalone;
                div()
                    .w_full()
                    .pl(px(32.))
                    .when(grouped, |section| {
                        section.child(
                            div()
                                .w_full()
                                .when(activity == ActivityPosition::Start, |run| {
                                    run.child(
                                        div()
                                            .pb(px(6.))
                                            .flex()
                                            .items_center()
                                            .gap(px(6.))
                                            .child(
                                                icon(Glyph::ListChecks)
                                                    .size(px(13.))
                                                    .text_color(rgb(faint())),
                                            )
                                            .child(eyebrow("Working through the task")),
                                    )
                                })
                                .child(
                                    div()
                                        .border_l_2()
                                        .border_color(rgb(border()))
                                        .pl(px(10.))
                                        .child(self.tool_row(
                                            offset,
                                            tool,
                                            latest_plan == Some(offset),
                                            cx,
                                        )),
                                ),
                        )
                    })
                    .when(!grouped, |section| {
                        section.py(px(4.)).child(self.tool_row(
                            offset,
                            tool,
                            latest_plan == Some(offset),
                            cx,
                        ))
                    })
                    .into_any_element()
            } else if offset == turn.tools.len() && streamed {
                assistant_row(
                    div()
                        .w_full()
                        .pt(px(4.))
                        .text_size(px(15.))
                        .line_height(relative(1.7))
                        .text_color(rgb(text()))
                        .child(
                            TextView::markdown("streamed-answer", turn.text.clone())
                                .selectable(true),
                        )
                        .into_any_element(),
                )
            } else if turn.running {
                turn_status(turn)
            } else if let Some(error) = &turn.error {
                self.turn_error(error, cx)
            } else {
                div().into_any_element()
            }
        } else {
            div().into_any_element()
        };
        div()
            .w_full()
            .flex()
            .justify_center()
            .px(px(28.))
            .py(px(2.))
            .when(index > 0 && index <= self.messages.len(), |row| {
                row.py(px(14.))
            })
            .child(div().w_full().max_w(px(760.)).child(content))
            .into_any_element()
    }
    /// Records the decision immediately so the card responds before the worker
    /// confirms; the engine still enforces the actual outcome.
    pub(super) fn decide_action(
        &mut self,
        id: Uuid,
        allow: bool,
        remember: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(card) = self
            .turns
            .get_mut(&self.session)
            .and_then(|turn| turn.tools.iter_mut().find(|t| t.id == id))
        {
            // A restored legacy preview may have already lost its tail upstream.
            // Neither pointer nor keyboard approval can bypass full inspection.
            if allow && card.input.get("diff_truncated").and_then(Value::as_bool) == Some(true) {
                return;
            }
            card.approval = false;
            if !allow {
                card.success = Some(false);
                card.result = Some("You denied this action.".into());
                card.refresh_presentation();
            }
        }
        self.engine_command(
            WorkerCommand::Approve {
                action_id: id,
                allow,
                remember,
            },
            cx,
        );
        self.invalidate_live_history();
        self.sync_history_list(cx);
        cx.notify();
    }
    /// Keyboard: allow the oldest pending action in this chat.
    pub(super) fn allow_pending(&mut self, cx: &mut Context<Self>) {
        let pending = self
            .turns
            .get(&self.session)
            .and_then(|turn| turn.pending_approvals().next().map(|t| t.id));
        if let Some(id) = pending {
            self.decide_action(id, true, false, cx);
        }
    }
    /// Keyboard: deny the oldest pending action, otherwise stop the task.
    pub(super) fn deny_or_stop(&mut self, cx: &mut Context<Self>) {
        let pending = self
            .turns
            .get(&self.session)
            .and_then(|turn| turn.pending_approvals().next().map(|t| t.id));
        match pending {
            Some(id) => self.decide_action(id, false, false, cx),
            None if self.running() => self.engine_command(WorkerCommand::Cancel, cx),
            None => {}
        }
    }
    fn tool_row(
        &self,
        i: usize,
        tool: &crate::state::ToolCard,
        latest_plan: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if tool.name == "todo_write" && tool.success != Some(false) {
            return plan_card(i, tool, latest_plan);
        }
        let id = tool.id;
        let expanded = self.expanded_tools.contains(&id);
        let cached = &tool.presentation;
        let summary = ActionSummary {
            verb: cached.verb.clone(),
            target: cached.target.clone(),
            meta: cached.meta.clone().map(|text| (text, Tone::Muted)),
            question: cached.question.clone(),
        };
        let (state, state_word): (AnyElement, &str) = if tool.approval {
            (
                icon(IconName::TriangleAlert)
                    .size(px(13.))
                    .text_color(rgb(warning()))
                    .into_any_element(),
                "needs approval",
            )
        } else {
            match tool.success {
                None => (
                    Spinner::new()
                        .small()
                        .color(rgb(muted()).into())
                        .into_any_element(),
                    "in progress",
                ),
                Some(true) => (
                    icon(Glyph::Check)
                        .size(px(13.))
                        .text_color(rgb(success()))
                        .into_any_element(),
                    "done",
                ),
                Some(false) => (
                    icon(IconName::CircleX)
                        .size(px(13.))
                        .text_color(rgb(danger()))
                        .into_any_element(),
                    "failed",
                ),
            }
        };
        let glyph = tool_glyph(&tool.name);
        let header = focus_ring(gpui_kit::base::Button::new(("tool-detail", i)))
            .accessibility_label(format!(
                "{} {}, {state_word}. {}",
                summary.verb,
                summary.target,
                if expanded {
                    "Hide details"
                } else {
                    "Show details"
                }
            ))
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .justify_start()
            .gap(px(8.))
            .px(px(8.))
            .h(px(30.))
            .rounded(px(8.))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(hover())))
            .child(
                icon(glyph)
                    .size(px(14.))
                    .flex_shrink_0()
                    .text_color(rgb(if tool.approval { warning() } else { muted() })),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(text_secondary()))
                    .child(summary.verb.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(12.5))
                    .font_family(mono(cx))
                    .text_color(rgb(muted()))
                    .child(summary.target.clone()),
            )
            .when_some(summary.meta.clone(), |row, (meta, tone)| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(11.5))
                        .text_color(rgb(tone.color()))
                        .child(meta),
                )
            })
            .child(div().flex_shrink_0().child(state))
            .child(
                icon(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(px(12.))
                .text_color(rgb(faint())),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.expanded_tools.remove(&id) {
                    this.expanded_tools.insert(id);
                }
                this.invalidate_live_history();
                this.sync_history_list(cx);
                cx.notify();
            }));
        let framed = tool.approval
            || expanded
            || cached.error.is_some()
            || (tool.name == "image_generate" && tool.success == Some(true));
        let mut card = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_0()
            .rounded(px(10.))
            .when(framed, |card| {
                card.border_1()
                    .border_color(rgb(border()))
                    .bg(rgb(panel()))
                    .p(px(4.))
            })
            .when(tool.approval, |card| {
                card.p(px(14.))
                    .gap_3()
                    .rounded(px(14.))
                    .bg(rgb(panel()))
                    .border_1()
                    .border_color(rgb(border_strong()))
                    .shadow_sm()
            })
            .child(header);
        if tool.name == "image_generate" && tool.success == Some(true) {
            card = card.child(self.generated_tool_image_card(tool, cx));
        }
        if tool.approval {
            card = card.child(self.approval_panel(i, tool, &summary, cx));
        } else if expanded {
            card = card.child(self.structured_tool_details(i, tool, cx));
        }
        if let Some(error) = &cached.error {
            card = card.child(
                div()
                    .px_3()
                    .pb_2()
                    .text_size(px(11.))
                    .text_color(rgb(danger()))
                    .child(error.clone()),
            );
        }
        card.into_any_element()
    }
    fn full_action_inspector(
        &self,
        i: usize,
        tool: &crate::state::ToolCard,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = tool.id;
        let expanded = self.raw_tool_details.contains(&id);
        let full = serde_json::to_string_pretty(&tool.input).unwrap_or_default();
        let diff = tool
            .input
            .get("diff")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let copy = full.clone();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        button(
                            ("approval-inspect", i),
                            if expanded {
                                "Hide full action"
                            } else {
                                "Inspect full action"
                            },
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            if !view.raw_tool_details.remove(&id) {
                                view.raw_tool_details.insert(id);
                            }
                            view.invalidate_live_history();
                            view.sync_history_list(cx);
                            cx.notify();
                        })),
                    )
                    .child(button(("approval-copy", i), "Copy full action").on_click(
                        move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                        },
                    )),
            )
            .when(expanded, |body| {
                body.when_some(diff, |body, diff| {
                    let copy = diff.clone();
                    body.child(
                        button(("approval-copy-diff", i), "Copy full diff").on_click(
                            move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                            },
                        ),
                    )
                    .child(full_code_block(("approval-full-diff", i), &diff))
                })
                .child(full_code_block(("approval-full", i), &full))
            })
            .into_any_element()
    }
    fn approval_panel(
        &self,
        i: usize,
        tool: &crate::state::ToolCard,
        summary: &ActionSummary,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = tool.id;
        let incomplete = tool.input.get("diff_truncated").and_then(Value::as_bool) == Some(true);
        let shortcut = if cfg!(target_os = "macos") {
            "⌘⇧↩ allow · Esc deny"
        } else {
            "Ctrl+Shift+Enter allow · Esc deny"
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .child(icon(Glyph::ShieldCheck).size(px(16.)).mt(px(1.)).text_color(rgb(accent_text())))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(14.))
                            .line_height(relative(1.45))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(text()))
                            .child(summary.question.clone()),
                    ),
            )
            .child(approval_preview(i, tool))
            .child(self.full_action_inspector(i, tool, cx))
            .when(incomplete, |panel| panel.child(subtitle("This saved change was shortened before inspection. Deny it and ask for a fresh action to review the complete change.")))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(primary(("allow", i), "Allow once").disabled(incomplete).on_click(
                        cx.listener(move |this, _, _, cx| this.decide_action(id, true, false, cx)),
                    ))
                    .child(secondary(("deny", i), "Deny").on_click(
                        cx.listener(move |this, _, _, cx| this.decide_action(id, false, false, cx)),
                    ))
                    .child(div().flex_1())
                    .child(subtitle(shortcut)),
            )
            .child(subtitle("Allow once approves only this action."))
            .when_some(tool.rule.clone(), |panel, rule| {
                panel.child(
                    div().flex().flex_col().gap_2().pt_3()
                        .border_t_1().border_color(rgb(border()))
                        .child(subtitle(format!("Persistent permission for this chat session: {rule}. Allows this action and future matching actions without asking again. Resets when the worker session ends; does not apply to other chats or override read-only mode.")))
                        .child(secondary(("allow-always", i), format!("Always allow {rule}"))
                            .self_start()
                            .disabled(incomplete)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.decide_action(id, true, true, cx)
                            }))),
                )
            })
            .into_any_element()
    }
    fn turn_error(&self, error: &str, _cx: &mut Context<Self>) -> AnyElement {
        let details = error.to_owned();
        div()
            .w_full()
            .pl(px(32.))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_start()
                    .gap_3()
                    .p(px(14.))
                    .rounded(px(12.))
                    .bg(rgb(danger_bg()))
                    .child(icon(IconName::CircleAlert).size(px(16.)).mt(px(2.)).text_color(rgb(danger())))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(rgb(text()))
                                    .child("The task stopped with an error"),
                            )
                            .child(body_text(product_error(error)))
                            .child(subtitle(
                                "Progress so far is saved. Send a message to continue or adjust the request.",
                            )),
                    )
                    .child(
                        button("copy-turn-error", "Copy details")
                            .text_size(px(12.))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(details.clone()))
                            }),
                    ),
            )
            .into_any_element()
    }
    fn can_populate_chat_starter(&self, cx: &Context<Self>) -> bool {
        self.messages.is_empty()
            && !self.running()
            && !self.history_loading()
            && !self.recovery.open
            && !self.recovery_blocks_submit()
            && !self.draft_flush_pending()
            && !self.draft_needs_attention()
            && self.composer.read(cx).value().is_empty()
            && self.workspace.is_some()
            && self.settings.provider.validate().is_ok()
    }

    pub(super) fn populate_chat_starter(
        &mut self,
        prompt: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Recheck at click time: a rendered suggestion must never overwrite a draft.
        if !self.can_populate_chat_starter(cx) {
            return;
        }
        self.composer.update(cx, |input, cx| {
            input.set_value(prompt, window, cx);
            input.focus(window, cx);
        });
        self.draft_changed(cx);
        cx.notify();
    }

    fn chat_home(&self, cx: &mut Context<Self>) -> AnyElement {
        let configured = self.settings.provider.validate().is_ok();
        let project_name = self.workspace.as_ref().map(|path| {
            path.file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned()
        });
        // Context line under the composer: where we are and which model answers.
        let mut context = div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .text_size(px(12.5))
            .text_color(rgb(muted()));
        match (&project_name, self.workspace_ready) {
            (None, _) => {
                context = context.child("No project open");
            }
            (Some(name), false) => {
                context = context
                    .child(
                        icon(IconName::Folder)
                            .size(px(13.))
                            .text_color(rgb(faint())),
                    )
                    .child(name.clone())
                    .child("· loading project…");
            }
            (Some(name), true) => {
                context = context
                    .child(
                        icon(IconName::Folder)
                            .size(px(13.))
                            .text_color(rgb(faint())),
                    )
                    .child(name.clone());
            }
        }
        context = context.child("·").child(if configured {
            self.settings.provider.model.clone()
        } else {
            "No model set up".to_owned()
        });
        if self.workspace.is_none() || !configured {
            context = context.child(
                button(
                    "chat-home-setup",
                    if self.workspace.is_none() {
                        "Choose project"
                    } else {
                        "Set up model"
                    },
                )
                .h(px(26.))
                .px(px(8.))
                .text_size(px(12.5))
                .text_color(rgb(accent_text()))
                .on_click(cx.listener(|view, _, window, cx| {
                    if view.workspace.is_none() {
                        view.pick_workspace(window, cx);
                    } else if view.settings.provider.validate().is_err() {
                        view.navigate(Page::Providers, cx);
                    }
                })),
            );
        }
        div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .px(px(28.))
            .pb(px(96.))
            .child(
                div()
                    .w_full()
                    .max_w(px(760.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(22.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(14.))
                            .child(brand_glyph(30.))
                            .child(display(greeting())),
                    )
                    .child(self.prompt_box(cx))
                    .child(context)
                    .when(self.can_populate_chat_starter(cx), |body| {
                        body.child(
                            div()
                                .flex()
                                .flex_wrap()
                                .justify_center()
                                .gap_2()
                                .pt(px(6.))
                                .children([
                                    (Glyph::Lightbulb, "Explain this project", "Explain this project using read-only inspection. Summarize its architecture, key entry points, and how to run its tests. Do not edit files."),
                                    (Glyph::Eye, "Review my changes", "Review the current project changes using read-only inspection. Look for bugs, regressions, and missing tests. Do not edit files."),
                                    (Glyph::ListChecks, "Plan before editing", "Inspect this project and help me plan a change before making any edits. Ask what I want to change, then propose a plan and wait for my approval before editing files."),
                                ].into_iter().enumerate().map(|(index, (glyph, label, prompt))| {
                                    chip_with(("chat-home-starter", index), Some(icon(glyph)), label)
                                        .on_click(cx.listener(move |view, _, window, cx| {
                                            view.populate_chat_starter(prompt, window, cx);
                                        }))
                                })),
                        )
                    }),
            )
            .into_any_element()
    }
    fn model_switcher(
        &self,
        label: String,
        picking: Entity<ComposerModelPicker>,
        cx: &Context<Self>,
    ) -> AnyElement {
        MenuButton::new("composer-model-options")
            .ghost()
            .label(label)
            .max_w(px(240.))
            .child(icon(IconName::ChevronRight).size(px(12.)))
            .on_click(cx.listener(move |view, _, _, cx| {
                picking.update(cx, |state, cx| {
                    state.choosing = true;
                    state.models = std::sync::Arc::new(
                        view.settings
                            .providers
                            .iter()
                            .filter(|provider| !provider.model.is_empty())
                            .cloned()
                            .collect(),
                    );
                    cx.notify();
                });
                cx.notify();
            }))
            .into_any_element()
    }

    fn composer_model_choices(
        &self,
        picking: Entity<ComposerModelPicker>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let back = picking.clone();
        let mut list = div()
            .id("composer-model-list")
            .w(px(260.))
            .max_h(px(300.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                MenuButton::new("composer-model-back")
                    .ghost()
                    .label("Models")
                    .icon(IconName::ArrowLeft)
                    .justify_start()
                    .on_click(cx.listener(move |_, _, _, cx| {
                        back.update(cx, |value, cx| {
                            value.choosing = false;
                            cx.notify();
                        });
                        cx.notify();
                    })),
            );
        // This view exists only while the picker is open. Large registries are
        // never copied on the composer frame path.
        let models = picking.read(cx).models.clone();
        for (index, provider) in models.iter().enumerate() {
            let models = models.clone();
            let picking = picking.clone();
            let checked = providers::same_connection(&provider, &self.settings.provider)
                && provider.model == self.settings.provider.model;
            list = list.child(
                MenuButton::new(("composer-model-choice", index))
                    .ghost()
                    .w_full()
                    .justify_start()
                    .label(format!("{} · {}", provider.model, provider.name))
                    .selected(checked)
                    .bg(rgb(if checked { selected() } else { panel() }))
                    .tooltip(format!("{} · {}", provider.model, provider.name))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        let provider = &models[index];
                        let Some(provider) = view
                            .settings
                            .providers
                            .iter()
                            .find(|current| {
                                current.model == provider.model
                                    && providers::same_connection(current, &provider)
                            })
                            .cloned()
                        else {
                            view.error =
                                Some("The model catalog changed. Reopen the model picker.".into());
                            cx.notify();
                            return;
                        };
                        view.settings.provider = provider.clone();
                        view.provider_form(&provider, window, cx);
                        let settings = view.settings.clone();
                        view.host_call(cx, |host| host.save_settings(settings));
                        picking.update(cx, |value, cx| {
                            value.choosing = false;
                            cx.notify();
                        });
                        cx.notify();
                    })),
            );
        }
        list.child(
            MenuButton::new("composer-manage-models")
                .ghost()
                .label("Manage models…")
                .justify_start()
                .on_click(cx.listener(|view, _, _, cx| {
                    view.settings_open = true;
                    view.navigate(Page::Providers, cx);
                })),
        )
        .into_any_element()
    }

    fn composer_context_actions(&self, cx: &Context<Self>) -> AnyElement {
        let weak = cx.entity().downgrade();
        MenuButton::new("composer-context-actions")
            .accessibility_label("Add context")
            .ghost()
            .icon(IconName::Plus)
            .tooltip("Add context")
            .disabled(self.attachments.importing_for(self.session))
            .dropdown_menu(move |menu, _, _| {
                let attach = weak.clone();
                let paste = weak.clone();
                let drafts = weak.clone();
                menu.item(
                    PopupMenuItem::new("Attach images…").on_click(move |_, window, cx| {
                        let _ = attach.update(cx, |view, cx| view.attachment_picker(window, cx));
                    }),
                )
                .item(PopupMenuItem::new("Paste image").on_click(move |_, _, cx| {
                    let _ = paste.update(cx, |view, cx| view.attachment_paste_files(cx));
                }))
                .separator()
                .item(
                    PopupMenuItem::new("Saved drafts").on_click(move |_, _, cx| {
                        let _ = drafts.update(cx, |view, cx| view.open_saved_drafts(cx));
                    }),
                )
            })
            .into_any_element()
    }

    fn unified_model_control(&self, label: String, cx: &Context<Self>) -> AnyElement {
        use gpui_kit::component::popover::Popover;
        let provider = &self.settings.provider;
        let effort = provider.effective_reasoning_effort().or(provider
            .default_reasoning_effort
            .as_deref()
            .filter(|value| provider.supported_reasoning_efforts().contains(value)));
        let effort_label = effort.map(Self::effort_caption).unwrap_or_else(|| {
            if provider.supported_reasoning_efforts().is_empty() {
                String::new()
            } else {
                "Default".into()
            }
        });
        let weak = cx.entity().downgrade();
        let mut trigger_layout = div().min_w_0().max_w(px(250.)).flex_shrink(1.);
        Popover::new("composer-model-popover")
            .bg(rgb(panel()))
            .border_1()
            .border_color(rgb(border()))
            .rounded(px(16.))
            .trigger_style(trigger_layout.style().clone())
            .anchor(Anchor::BottomRight)
            .offset(px(8.))
            .trigger(
                MenuButton::new("composer-model")
                    .ghost()
                    .label(label)
                    .h(px(30.))
                    .rounded_full()
                    .text_size(px(12.5))
                    .text_color(rgb(text_secondary()))
                    .max_w(px(250.))
                    .w_full()
                    .min_w_0()
                    .flex_shrink(1.)
                    .gap_1()
                    .child(
                        div()
                            .flex_shrink_0()
                            .whitespace_nowrap()
                            .text_color(rgb(accent_text()))
                            .child(effort_label),
                    )
                    .child(icon(IconName::ChevronDown).size(px(12.)))
                    .tooltip("Model and reasoning"),
            )
            .content(move |_, window, cx| {
                let picking = window.use_keyed_state("composer-model-choosing", cx, |_, _| {
                    ComposerModelPicker::default()
                });
                weak.update(cx, |view, cx| view.composer_model_panel(picking, cx))
                    .unwrap_or_else(|_| div().into_any_element())
            })
            .into_any_element()
    }

    fn effort_caption(value: &str) -> String {
        match value {
            "none" => "None".into(),
            "minimal" => "Minimal".into(),
            "low" => "Low".into(),
            "medium" => "Medium".into(),
            "high" => "High".into(),
            "xhigh" => "Extra high".into(),
            other => {
                let mut chars = other.chars();
                chars
                    .next()
                    .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                    .unwrap_or_default()
            }
        }
    }

    fn composer_model_panel(
        &self,
        picking: Entity<ComposerModelPicker>,
        cx: &Context<Self>,
    ) -> AnyElement {
        if picking.read(cx).choosing {
            return self.composer_model_choices(picking, cx);
        }
        let provider = &self.settings.provider;
        let efforts = provider.supported_reasoning_efforts();
        let active = provider.effective_reasoning_effort().or(provider
            .default_reasoning_effort
            .as_deref()
            .filter(|value| efforts.contains(value)));
        let title = active.map(Self::effort_caption).unwrap_or_else(|| {
            if efforts.is_empty() {
                "Model".into()
            } else {
                "Provider default".into()
            }
        });
        let mut surface = div()
            .id("composer-model-panel")
            .w(px(260.))
            .flex()
            .flex_col()
            .gap_3()
            .p_2()
            .child(
                div()
                    .relative()
                    .h(px(32.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_center()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(accent_text()))
                            .child(title),
                    )
                    .when(!efforts.is_empty(), |row| {
                        row.child(
                            MenuButton::new("composer-effort-default")
                                .absolute()
                                .right_0()
                                .top_0()
                                .ghost()
                                .icon(IconName::RefreshCw)
                                .accessibility_label("Use provider default reasoning")
                                .tooltip("Use provider default")
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.settings.provider.reasoning_effort = None;
                                    view.settings.remember_model(view.settings.provider.clone());
                                    let settings = view.settings.clone();
                                    view.host_call(cx, |host| host.save_settings(settings));
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(div().flex().justify_center().child(self.model_switcher(
                if provider.model.is_empty() {
                    "Select model".into()
                } else {
                    provider.model.clone()
                },
                picking,
                cx,
            )));
        if !efforts.is_empty() {
            let source = provider.clone();
            let mut levels = div()
                .id("composer-effort-levels")
                .w_full()
                .flex()
                .items_center()
                .gap_1()
                .rounded_full()
                .bg(rgb(hover()))
                .p_1();
            for (index, effort) in efforts.iter().enumerate() {
                let effort = (*effort).to_owned();
                let selected = active == Some(effort.as_str());
                let caption = Self::effort_caption(&effort);
                let source = source.clone();
                levels = levels.child(
                    MenuButton::new(("composer-effort", index))
                        .ghost()
                        .flex_1()
                        .min_w_0()
                        .h(px(30.))
                        .px_1()
                        .rounded_full()
                        .accessibility_label(format!("Reasoning: {caption}"))
                        .tooltip(caption.clone())
                        .selected(selected)
                        .bg(rgb(hover()))
                        .child(
                            div()
                                .flex_shrink_0()
                                .size(px(if selected { 22. } else { 4. }))
                                .rounded_full()
                                .bg(rgb(if selected { text() } else { muted() })),
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            if view.settings.provider.model != source.model
                                || !providers::same_connection(&view.settings.provider, &source)
                                || !view
                                    .settings
                                    .provider
                                    .supported_reasoning_efforts()
                                    .contains(&effort.as_str())
                            {
                                return;
                            }
                            view.settings.provider.reasoning_effort = Some(effort.clone());
                            view.settings.remember_model(view.settings.provider.clone());
                            let settings = view.settings.clone();
                            view.host_call(cx, |host| host.save_settings(settings));
                            cx.notify();
                        })),
                );
            }
            surface = surface.child(levels);
        }
        surface.into_any_element()
    }

    fn composer_permissions(&self, cx: &Context<Self>) -> AnyElement {
        let weak = cx.entity().downgrade();
        let caption = match self.settings.permissions {
            PermissionMode::AskForWrites => "Ask first",
            PermissionMode::AcceptEdits => "Accept edits",
            PermissionMode::Auto => "Edits & commands",
            PermissionMode::ReadOnly => "Read only",
        };
        MenuButton::new("composer-permissions")
            .ghost()
            .label(caption)
            .icon(Glyph::SlidersHorizontal)
            .max_w(px(170.))
            .h(px(30.))
            .rounded_full()
            .text_size(px(12.5))
            .text_color(rgb(text_secondary()))
            .tooltip(format!(
                "{} · Applies to new tasks",
                self.settings.permissions.description()
            ))
            .dropdown_menu(move |mut menu, _, cx| {
                let Ok(active) = weak.update(cx, |view, _| view.settings.permissions) else {
                    return menu;
                };
                menu = menu.label("Applies to new tasks");
                for mode in PermissionMode::ALL {
                    let view = weak.clone();
                    menu = menu.item(
                        PopupMenuItem::new(mode.label())
                            .checked(active == mode)
                            .on_click(move |_, _, cx| {
                                let _ = view.update(cx, |view, cx| {
                                    view.settings.permissions = mode;
                                    let settings = view.settings.clone();
                                    view.host_call(cx, |host| host.save_settings(settings));
                                    cx.notify();
                                });
                            }),
                    );
                }
                menu.separator()
                    .label("Running tasks keep their current permissions")
            })
            .into_any_element()
    }

    fn composer_context(&self, cx: &Context<Self>) -> AnyElement {
        // Only read the last measured count on the render path. Estimating long
        // histories is deferred until the popover opens, rather than per token.
        let reported = self
            .turns
            .get(&self.session)
            .map(|turn| turn.usage.input_tokens)
            .filter(|count| *count > 0);
        let capacity = self.settings.provider.context_tokens;
        let label = reported
            .filter(|_| capacity > 0)
            .map(|used| {
                let percent = used.saturating_mul(100) / u64::from(capacity);
                if percent == 0 {
                    "<1%".to_owned()
                } else {
                    format!("{percent}%")
                }
            })
            .unwrap_or_else(|| "Context".into());
        let weak = cx.entity().downgrade();
        MenuButton::new("composer-context")
            .accessibility_label("Context window")
            .ghost()
            .h(px(30.))
            .w(px(28.))
            .px_0()
            .child(if let Some(used) = reported.filter(|_| capacity > 0) {
                gpui_kit::component::progress::ProgressCircle::new("composer-context-usage")
                    .value((used as f32 / capacity as f32 * 100.).clamp(0., 100.))
                    .with_size(px(14.))
                    .color(rgb(muted()))
                    .accessibility_label(format!(
                        "Last provider input: {used} of {capacity} configured tokens"
                    ))
                    .into_any_element()
            } else {
                div()
                    .size(px(13.))
                    .rounded_full()
                    .border_2()
                    .border_color(rgb(muted()))
                    .into_any_element()
            })
            .tooltip(format!("Context window · {label}"))
            .dropdown_menu(move |mut menu, _, cx| {
                let details = weak
                    .update(cx, |view, cx| {
                        let visible_characters: usize = view
                            .messages
                            .iter()
                            .map(|message| message.content.chars().count())
                            .sum();
                        let draft_characters = view.composer.read(cx).value().chars().count();
                        let streamed_characters = view
                            .turns
                            .get(&view.session)
                            .map_or(0, |turn| turn.text.chars().count());
                        let estimate =
                            (visible_characters + draft_characters + streamed_characters)
                                .div_ceil(4);
                        let reported = view
                            .turns
                            .get(&view.session)
                            .map(|turn| turn.usage.input_tokens)
                            .filter(|count| *count > 0);
                        (estimate, reported, view.settings.provider.context_tokens)
                    })
                    .ok();
                menu = menu.label("Context window");
                if let Some((estimate, reported, capacity)) = details {
                    if let Some(tokens) = reported {
                        menu = menu.label(format!("Last provider input: {tokens} tokens"));
                    }
                    menu = menu.label(format!("Visible conversation + draft: ~{estimate} tokens"));
                    menu = menu.label(if capacity > 0 {
                        format!("Configured limit: {capacity} tokens")
                    } else {
                        "Context limit unknown".into()
                    });
                    menu = menu
                        .separator()
                        .label("Estimate excludes hidden system, tool and attachment context.");
                }
                menu
            })
            .into_any_element()
    }
    fn prompt_box(&self, cx: &mut Context<Self>) -> AnyElement {
        let paste = cx.entity().downgrade();
        let model = if self.settings.provider.model.is_empty() {
            "Select model".to_string()
        } else {
            self.settings.provider.model.clone()
        };
        let draft_empty = self.composer.read(cx).value().trim().is_empty();
        let has_attachments = !self.draft_attachments().is_empty();
        let running = self.running();
        div()
            .w_full()
            .max_w(px(760.))
            .child(self.pending_messages(cx))
            .when(!self.speech.transcript.is_empty(), |panel| {
                panel.child(
                    button("review-voice-transcript", "Review voice transcript")
                        .text_size(px(12.))
                        .on_click(cx.listener(|view, _, _, cx| view.navigate(Page::Voice, cx))),
                )
            })
            .child(
                div()
                    .id("image-drop-target")
                    .test_support()
                    .w_full()
                    .rounded(px(20.))
                    .bg(rgb(panel()))
                    .border_1()
                    .border_color(rgb(border_strong()))
                    .shadow_sm()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .px(px(14.))
                    .pt(px(12.))
                    .pb(px(10.))
                    .on_drop(cx.listener(|view, paths: &ExternalPaths, _, cx| {
                        view.attach_image_paths(paths.paths().to_vec(), AttachmentSource::Drop, cx)
                    }))
                    .child(
                        div().min_h(px(44.)).text_size(px(15.)).child(
                            Textarea::new(&self.composer)
                                .on_paste(move |item, _, cx| {
                                    paste.update(cx, |view, cx| view.attachment_paste_item(item, cx)).unwrap_or(false)
                                })
                                .aria_label("Message")
                                .appearance(false)
                                .bordered(false)
                                .disabled(self.draft_flush_pending()),
                        ),
                    )
                    .when(
                        has_attachments || self.attachments.importing_for(self.session),
                        |surface| surface.child(self.attachment_strip(cx)),
                    )
                    .when(!self.attachments.notice.is_empty(), |surface| {
                        surface.child(subtitle(self.attachments.notice.clone()))
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .when(running && (!draft_empty || has_attachments), |row| row.flex_wrap())
                            .min_h(px(32.))
                            .child(self.composer_context_actions(cx))
                            .child(self.composer_permissions(cx))
                            .child(div().flex_1().min_w_0())
                            .child(self.composer_context(cx))
                            .child(self.unified_model_control(model, cx))
                            .child(self.speech_capture_button(cx))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .flex_shrink_0()
                                    .ml(px(4.))
                                    .when(running && !draft_empty, |row| {
                                        row.child(pill("steer-prompt", "Steer")
                                            .accessibility_label("Steer current task")
                                            .border_color(rgb(border_strong()))
                                            .disabled(self.pending_guidance_text().is_some() || self.draft_send_pending() || has_attachments || self.attachments.importing_for(self.session) || self.draft_flush_pending())
                                            .tooltip(|_,cx| cx.new(|_| XyloTooltip("Update the active task at its next safe step. Images can be queued for the next turn.".into())).into())
                                            .on_click(cx.listener(|view,_,window,cx| view.steer_composer(window,cx))))
                                    })
                                    .when(running, |row| {
                                        row.child(
                                            stop_button("cancel-task", "Stop the task")
                                                .tooltip(|_, cx| cx.new(|_| XyloTooltip("Stop".into())).into())
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.engine_command(WorkerCommand::Cancel, cx)
                                                })),
                                        )
                                    })
                                    .when(!running || !draft_empty || has_attachments, |row| {
                                        row.child({
                                            let pending = self.draft_send_pending();
                                            let disabled = pending
                                                || self.attachments.importing_for(self.session)
                                                || self.draft_flush_pending()
                                                || self.pending_guidance_text().is_some_and(|text| text == self.composer.read(cx).value().as_str())
                                                || (!running && draft_empty && !has_attachments);
                                            send_button(
                                                "send-prompt",
                                                if pending {
                                                    "Sending message"
                                                } else if running {
                                                    "Queue message"
                                                } else {
                                                    "Send message"
                                                },
                                                disabled,
                                            )
                                            .when(running, |button| button.w(px(66.)).rounded_full())
                                            .tooltip({ let queued = running; move |_, cx| cx.new(|_| XyloTooltip(if queued { "Queue for after this task finishes" } else { "Send message" }.into())).into() })
                                            .child(if pending {
                                                Spinner::new()
                                                    .small()
                                                    .color(rgb(brand_foreground()).into())
                                                    .into_any_element()
                                            } else if running {
                                                div().text_size(px(12.5)).font_weight(FontWeight::MEDIUM).child("Queue").into_any_element()
                                            } else {
                                                icon(IconName::ArrowUp)
                                                    .size(px(18.))
                                                    .into_any_element()
                                            })
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.submit(window, cx)
                                            }))
                                        })
                                    }),
                            ),
                    ),
            )
            .when(self.draft_needs_attention(), |surface| {
                surface.child(div().pt_1().child(self.draft_status_footer(cx)))
            })
            .into_any_element()
    }
}
use crate::state::{ToolCard, TurnState};
use gpui_kit::component::{Icon, Sizable, spinner::Spinner};
use serde_json::Value;

/// Time-of-day greeting addressed to the person, as the home screen title.
fn greeting() -> String {
    use chrono::Timelike;
    let hour = chrono::Local::now().hour();
    let salutation = match hour {
        5..=11 => "Good morning",
        12..=16 => "Good afternoon",
        17..=21 => "Good evening",
        _ => "Hello",
    };
    let name = super::sidebar::person_name();
    if name == "You" {
        salutation.to_owned()
    } else {
        format!("{salutation}, {name}")
    }
}

/// A glyph for each kind of action so a run of tool calls scans at a glance.
fn tool_glyph(name: &str) -> Glyph {
    match name {
        "file_read" | "read_file" => Glyph::FileSearch,
        "file_write" | "file_edit" | "write_file" | "edit_file" => Glyph::FilePen,
        "run_command" | "shell_command" | "terminal" => Glyph::Terminal,
        "search_project" | "grep" | "glob" | "find" => Glyph::Search,
        "git_status" | "git_diff" | "git_log" | "git_commit" => Glyph::GitBranch,
        "web_fetch" | "web_search" | "browser" => Glyph::Globe,
        "todo_write" => Glyph::ListChecks,
        "image_generate" => Glyph::Image,
        "memory" | "memory_write" | "memory_read" => Glyph::Brain,
        _ => Glyph::Wrench,
    }
}

#[derive(Clone, Copy)]
enum Tone {
    Muted,
}
impl Tone {
    fn color(self) -> u32 {
        match self {
            Tone::Muted => muted(),
        }
    }
}
/// One-line, human description of an action for its card and approval.
struct ActionSummary {
    verb: String,
    target: String,
    meta: Option<(String, Tone)>,
    question: String,
}
fn text_arg<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}
/// The command line an action runs, for display only.
fn command_line(tool: &ToolCard) -> String {
    match tool.name.as_str() {
        "shell_command" => text_arg(&tool.input, "command").to_owned(),
        _ => {
            let mut line = text_arg(&tool.input, "program").to_owned();
            for arg in tool
                .input
                .get("args")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                line.push(' ');
                if arg.is_empty() || arg.contains([' ', '"', '\'']) {
                    line.push_str(&format!("\"{}\"", arg.replace('"', "\\\"")));
                } else {
                    line.push_str(arg);
                }
            }
            line
        }
    }
}
fn elapsed_label(turn: &TurnState) -> String {
    let seconds = turn
        .started_at
        .map(|start| ((now_ms() - start).max(0) / 1000) as u64)
        .unwrap_or(0);
    if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}
/// Live progress for a running turn: what it is doing, for how long, and how to stop it.
fn turn_status(turn: &TurnState) -> AnyElement {
    let waiting = turn.pending_approvals().count();
    let (indicator, label): (AnyElement, String) = if waiting > 0 {
        (
            icon(IconName::TriangleAlert)
                .size(px(15.))
                .text_color(rgb(warning()))
                .into_any_element(),
            if waiting == 1 {
                "Waiting for your approval".into()
            } else {
                format!("Waiting for your approval on {waiting} actions")
            },
        )
    } else {
        (
            brand_glyph(16.).into_any_element(),
            if turn.status.is_empty() || turn.status == "Working" {
                "Working…".into()
            } else {
                turn.status.clone()
            },
        )
    };
    div()
        .w_full()
        .flex()
        .items_start()
        .gap(px(12.))
        .py(px(6.))
        .child(
            div()
                .flex_shrink_0()
                .w(px(20.))
                .pt(px(2.))
                .flex()
                .justify_center()
                .child(indicator),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .text_size(px(14.))
                        .text_color(rgb(text_secondary()))
                        .child(label),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_size(px(12.))
                        .text_color(rgb(faint()))
                        .child(elapsed_label(turn))
                        .child("·")
                        .child(kbd("Esc"))
                        .child(if waiting > 0 { "to deny" } else { "to stop" }),
                ),
        )
        .into_any_element()
}
/// The agent's checklist; only the newest version is shown in full.
fn plan_card(i: usize, tool: &ToolCard, latest: bool) -> AnyElement {
    let items: Vec<(String, String)> = tool
        .input
        .get("todos")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| {
            (
                text_arg(item, "content").to_owned(),
                text_arg(item, "status").to_owned(),
            )
        })
        .collect();
    let done = items.iter().filter(|(_, s)| s == "completed").count();
    let heading = div()
        .flex()
        .items_center()
        .gap(px(8.))
        .child(
            icon(Glyph::ListChecks)
                .size(px(15.))
                .text_color(rgb(accent_text())),
        )
        .child(
            div()
                .text_size(px(13.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(text()))
                .child(if latest { "Plan" } else { "Updated plan" }),
        )
        .child(div().flex_1())
        .child(badge(
            format!("{done} of {} done", items.len()),
            if done == items.len() && !items.is_empty() {
                BadgeTone::Success
            } else {
                BadgeTone::Neutral
            },
        ));
    if !latest {
        return div()
            .id(("plan", i))
            .px(px(8.))
            .h(px(30.))
            .flex()
            .items_center()
            .child(heading.opacity(0.7))
            .into_any_element();
    }
    let mut list = div().flex().flex_col().gap(px(6.));
    for (index, (content, status)) in items.into_iter().enumerate() {
        let (glyph, tone, weight): (Icon, u32, FontWeight) = match status.as_str() {
            "completed" => (icon(IconName::CircleCheck), success(), FontWeight::NORMAL),
            "in_progress" => (icon(Glyph::LoaderCircle), accent_text(), FontWeight::MEDIUM),
            _ => (icon(Glyph::CircleDashed), faint(), FontWeight::NORMAL),
        };
        list = list.child(
            div()
                .id(("plan-item", i * 1000 + index))
                .flex()
                .items_start()
                .gap(px(10.))
                .text_size(px(13.5))
                .line_height(relative(1.5))
                .child(glyph.size(px(15.)).mt(px(2.)).text_color(rgb(tone)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_weight(weight)
                        .text_color(rgb(if status == "completed" {
                            muted()
                        } else {
                            text()
                        }))
                        .child(content),
                ),
        );
    }
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(12.))
        .p(px(14.))
        .rounded(px(12.))
        .bg(rgb(panel()))
        .border_1()
        .border_color(rgb(border()))
        .child(heading)
        .child(list)
        .into_any_element()
}
/// Unified diff with added and removed lines tinted; long diffs scroll.
fn diff_view(id: impl Into<ElementId>, diff: &str) -> AnyElement {
    const LINES: usize = 600;
    let mut body = div().flex().flex_col().min_w_full();
    let mut shown = 0;
    for line in diff.lines().take(LINES) {
        shown += 1;
        let (background, foreground) = if line.starts_with("+++") || line.starts_with("---") {
            (None, muted())
        } else if line.starts_with('+') {
            (Some(diff_added_bg()), success())
        } else if line.starts_with('-') {
            (Some(diff_removed_bg()), danger())
        } else if line.starts_with("@@") {
            (None, accent())
        } else {
            (None, text())
        };
        body = body.child(
            div()
                .px_2()
                .whitespace_nowrap()
                .when_some(background, |row, color| row.bg(rgb(color)))
                .text_color(rgb(foreground))
                .child(if line.is_empty() {
                    " ".to_owned()
                } else {
                    line.to_owned()
                }),
        );
    }
    if diff.lines().count() > shown {
        body = body.child(
            div()
                .px_2()
                .text_color(rgb(muted()))
                .child("… diff preview shortened; use full inspection or copy controls"),
        );
    }
    div()
        .id(id)
        .w_full()
        .max_h(px(360.))
        .overflow_scroll()
        .py_1()
        .rounded(px(10.))
        .bg(rgb(code_bg()))
        .border_1()
        .border_color(rgb(border()))
        .font_family("monospace")
        .text_size(px(12.5))
        .line_height(relative(1.55))
        .child(body)
        .into_any_element()
}
/// Selectable, scrollable preview; full action inspection is explicit.
fn code_block(id: impl Into<ElementId>, text: &str) -> AnyElement {
    full_code_block(id, &truncate(text, 6000))
}
fn full_code_block(id: impl Into<ElementId>, text: &str) -> AnyElement {
    let id: ElementId = id.into();
    let mut run = 0;
    let mut longest = 2;
    for ch in text.chars() {
        run = if ch == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest + 1);
    // Markdown parses asynchronously; reserve the inspection viewport before
    // its first measured frame, and never shrink it to zero in a flex card.
    let height = (text.lines().count().max(1) as f32 * 20. + 24.).min(360.);
    div()
        .w_full()
        .h(px(height))
        .flex_shrink_0()
        .max_h(px(360.))
        .id(id.clone())
        .test_support()
        .overflow_scroll()
        .rounded_md()
        .child(TextView::markdown(id, format!("{fence}\n{text}\n{fence}")).selectable(true))
        .into_any_element()
}
fn approval_preview(i: usize, tool: &ToolCard) -> AnyElement {
    match tool.name.as_str() {
        "file_write" | "file_edit" => {
            let diff = text_arg(&tool.input, "diff");
            if diff.trim().is_empty() {
                subtitle("No textual change.").into_any_element()
            } else {
                diff_view(("approval-diff", i), diff)
            }
        }
        "run_command" | "shell_command" => code_block(
            ("approval-command", i),
            &format!("$ {}", command_line(tool)),
        ),
        _ => code_block(
            ("approval-input", i),
            &serde_json::to_string_pretty(&tool.input).unwrap_or_default(),
        ),
    }
}
