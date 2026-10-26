use super::*;
use gpui_kit::component::{
    IconName,
    button::Button as MenuButton,
    menu::{DropdownMenu, PopupMenuItem},
};

const ASSISTANTS: [(&str, &str, &str, IconName); 6] = [
    (
        "Cowork",
        "Your all-purpose assistant",
        "Help me plan and complete a task in this project. ",
        IconName::Bot,
    ),
    (
        "Code review",
        "Review changes with care",
        "Review this project and suggest the most useful improvements. ",
        IconName::SquareTerminal,
    ),
    (
        "Writing",
        "Turn ideas into documents",
        "Help me write a clear document for this project. ",
        IconName::FileText,
    ),
    (
        "Research",
        "Explore and compare ideas",
        "Research the following topic and compare the evidence: ",
        IconName::Search,
    ),
    (
        "Data analysis",
        "Find the story in your data",
        "Help me analyze the data in this project. ",
        IconName::Info,
    ),
    (
        "Planning",
        "Make your next steps clear",
        "Help me make a practical plan for: ",
        IconName::Calendar,
    ),
];
impl Workbench {
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
                    .px_6()
                    .pb_5()
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
                    button("older-messages", "Load earlier messages")
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
                .flex()
                .flex_col()
                .gap_2()
                .when(user, |row| {
                    row.max_w(px(600.))
                        .px_4()
                        .py_3()
                        .rounded_xl()
                        .bg(rgb(message_bg()))
                })
                .when(!user, |row| row.w_full().py_2())
                .when(self.history_message_highlight(message.id), |row| {
                    row.border_1().border_color(rgb(accent()))
                })
                .child(
                    TextView::markdown(
                        SharedString::from(message.id.to_string()),
                        message.content.clone(),
                    )
                    .selectable(true),
                )
                .child(self.message_images(message, cx));
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .when(user, |row| row.items_end())
                .child(body)
                .child(
                    button(("copy-message", index), "Copy")
                        .text_xs()
                        .text_color(rgb(muted()))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(content.clone()))
                        }),
                )
                .into_any_element()
        } else if let Some(turn) = self.turns.get(&self.session) {
            let offset = index - self.messages.len() - 1;
            let latest_plan = turn.tools.iter().rposition(|t| t.name == "todo_write");
            let streamed = !turn.text.is_empty() && (turn.running || turn.status == "Cancelled");
            if let Some(tool) = turn.tools.get(offset) {
                self.tool_row(offset, tool, latest_plan == Some(offset), cx)
            } else if offset == turn.tools.len() && streamed {
                TextView::markdown("streamed-answer", turn.text.clone())
                    .selectable(true)
                    .into_any_element()
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
            .px_6()
            .py_1()
            .when(index > 0 && index <= self.messages.len(), |row| row.py_3())
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
            card.approval = false;
            if !allow {
                card.success = Some(false);
                card.result = Some("You denied this action.".into());
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
        let summary = describe(tool);
        let (state, state_word): (AnyElement, &str) = if tool.approval {
            (
                icon(IconName::TriangleAlert)
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
                    icon(IconName::CircleCheck)
                        .text_color(rgb(success()))
                        .into_any_element(),
                    "done",
                ),
                Some(false) => (
                    icon(IconName::CircleX)
                        .text_color(rgb(danger()))
                        .into_any_element(),
                    "failed",
                ),
            }
        };
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
            .gap_2()
            .px_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .hover(|style| style.bg(rgb(hover())))
            .child(div().flex_shrink_0().child(state))
            .child(
                div()
                    .flex_shrink_0()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
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
                    .font_family("monospace")
                    .text_color(rgb(muted()))
                    .child(summary.target.clone()),
            )
            .when_some(summary.meta.clone(), |row, (meta, tone)| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(rgb(tone.color()))
                        .child(meta),
                )
            })
            .child(
                icon(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(px(14.))
                .text_color(rgb(muted())),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.expanded_tools.remove(&id) {
                    this.expanded_tools.insert(id);
                }
                this.sync_history_list(cx);
                cx.notify();
            }));
        let mut card = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .when(tool.approval, |card| {
                card.p_3()
                    .rounded_lg()
                    .bg(rgb(warning_bg()))
                    .border_1()
                    .border_color(rgb(warning()))
            })
            .child(header);
        if tool.approval {
            card = card.child(self.approval_panel(i, tool, &summary, cx));
        } else if expanded {
            card = card.child(div().pl_6().child(tool_detail(i, tool)));
        }
        card.into_any_element()
    }
    fn approval_panel(
        &self,
        i: usize,
        tool: &crate::state::ToolCard,
        summary: &ActionSummary,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = tool.id;
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
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(summary.question.clone()),
            )
            .child(approval_preview(i, tool))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(primary(("allow", i), "Allow").on_click(
                        cx.listener(move |this, _, _, cx| this.decide_action(id, true, false, cx)),
                    ))
                    .when_some(tool.rule.clone(), |row, rule| {
                        row.child(
                            button(("allow-always", i), format!("Always allow {rule}"))
                                .border_color(rgb(border()))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.decide_action(id, true, true, cx)
                                })),
                        )
                    })
                    .child(button(("deny", i), "Deny").on_click(
                        cx.listener(move |this, _, _, cx| this.decide_action(id, false, false, cx)),
                    ))
                    .child(div().flex_1())
                    .child(subtitle(shortcut)),
            )
            .into_any_element()
    }
    fn turn_error(&self, error: &str, _cx: &mut Context<Self>) -> AnyElement {
        let details = error.to_owned();
        div()
            .w_full()
            .flex()
            .items_start()
            .gap_2()
            .p_3()
            .rounded_lg()
            .bg(rgb(danger_bg()))
            .border_1()
            .border_color(rgb(danger()))
            .child(icon(IconName::CircleAlert).text_color(rgb(danger())))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child("The task stopped with an error"),
                    )
                    .child(div().text_sm().child(product_error(error)))
                    .child(subtitle(
                        "Progress so far is saved. Send a message to continue or adjust the request.",
                    )),
            )
            .child(
                button("copy-turn-error", "Copy details")
                    .text_xs()
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(details.clone()))
                    }),
            )
            .into_any_element()
    }
    fn chat_home(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut assistants = div().flex().flex_col().gap_2().mt_6().child(
            div()
                .text_sm()
                .text_color(rgb(muted()))
                .mb_1()
                .text_center()
                .child("Select an assistant to start a task"),
        );
        // Cards wrap onto more rows as the window narrows instead of squeezing.
        let mut cards = div().flex().flex_wrap().gap_2();
        {
            for (index, (name, description, prompt, glyph)) in ASSISTANTS.into_iter().enumerate() {
                cards = cards.child(
                    focus_ring(gpui_kit::base::Button::new(("assistant", index)))
                        .accessibility_label(format!("{name}: {description}"))
                        .flex_1()
                        .min_w(px(200.))
                        .p_3()
                        .rounded(px(14.))
                        .bg(rgb(panel()))
                        .border_1()
                        .border_color(rgb(border()))
                        .hover(|s| s.border_color(rgb(accent())))
                        .flex()
                        .items_center()
                        .justify_start()
                        .gap_2()
                        .child(
                            div()
                                .size(px(32.))
                                .rounded_full()
                                .bg(rgb(sidebar()))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(icon(glyph)),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .items_start()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(rgb(muted()))
                                        .child(description),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.selected_assistant = index;
                            this.composer.update(cx, |s, cx| {
                                s.set_value(prompt, window, cx);
                                s.focus(window, cx)
                            });
                            cx.notify();
                        })),
                );
            }
            assistants = assistants.child(cards);
        }
        let mut modes = div()
            .flex()
            .items_center()
            .gap_1()
            .p_1()
            .rounded_full()
            .bg(rgb(lavender()));
        for (index, label, glyph) in [
            (0, "Xylo", IconName::Bot),
            (1, "Code", IconName::SquareTerminal),
            (2, "Write", IconName::FileText),
            (3, "Research", IconName::Search),
        ] {
            modes = modes.child(
                focus_ring(gpui_kit::base::Button::new(("mode", index)))
                    .selected(self.selected_assistant == index)
                    .accessibility_label(label)
                    .px_3()
                    .py_2()
                    .rounded_full()
                    .text_size(px(13.))
                    .bg(rgb(if self.selected_assistant == index {
                        panel()
                    } else {
                        lavender()
                    }))
                    .hover(|d| d.bg(rgb(panel())))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(icon(glyph))
                    .child(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.selected_assistant = index;
                        if this.composer.read(cx).value().trim().is_empty() {
                            let prompt = ASSISTANTS[index].2;
                            this.composer
                                .update(cx, |s, cx| s.set_value(prompt, window, cx));
                        }
                        cx.notify();
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
            .px_4()
            .pb_8()
            .child(
                div()
                    .w_full()
                    .max_w(px(800.))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_center()
                            .mb_4()
                            .child("Hi, what's your plan for today?"),
                    )
                    .child(div().flex().justify_center().mb_4().child(modes))
                    .child(self.prompt_box(cx))
                    .child(assistants),
            )
            .into_any_element()
    }
    fn model_switcher(&self, label: String, cx: &Context<Self>) -> AnyElement {
        let weak = cx.entity().downgrade();
        let saved = self.settings.providers.clone();
        let active = self.settings.provider.clone();
        MenuButton::new("composer-model")
            .label(label)
            .icon(IconName::ChevronDown)
            .max_w(px(200.))
            .dropdown_menu(move |mut menu, _, _| {
                menu = menu.label("Your models");
                for provider in saved.iter().filter(|p| !p.model.is_empty()) {
                    let provider = provider.clone();
                    let checked = provider.endpoint == active.endpoint
                        && provider.model == active.model
                        && provider.family == active.family;
                    let view = weak.clone();
                    menu = menu.item(
                        PopupMenuItem::new(format!("{} · {}", provider.model, provider.name))
                            .checked(checked)
                            .on_click(move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.settings.provider = provider.clone();
                                    this.provider_form(&provider, window, cx);
                                    let settings = this.settings.clone();
                                    this.host_call(cx, |host| host.save_settings(settings));
                                    cx.notify();
                                });
                            }),
                    );
                }
                let view = weak.clone();
                menu.separator()
                    .item(
                        PopupMenuItem::new("Manage models…").on_click(move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| this.navigate(Page::Providers, cx));
                        }),
                    )
            })
            .into_any_element()
    }

    /// How much the agent may do without asking, applied to the next message.
    fn permission_switcher(&self, cx: &Context<Self>) -> AnyElement {
        let weak = cx.entity().downgrade();
        let current = self.settings.permissions;
        MenuButton::new("permission-mode")
            .label(current.label())
            .icon(IconName::ChevronDown)
            .max_w(px(200.))
            .tooltip(current.description())
            .dropdown_menu(move |mut menu, _, _| {
                menu = menu.label("Permissions for the next message");
                for mode in PermissionMode::ALL {
                    let view = weak.clone();
                    let hint = match mode {
                        PermissionMode::AskForWrites => "approve every change",
                        PermissionMode::AcceptEdits => "commands still ask",
                        PermissionMode::Auto => "external actions still ask",
                        PermissionMode::ReadOnly => "nothing is changed",
                    };
                    menu = menu.item(
                        PopupMenuItem::new(format!("{} · {hint}", mode.label()))
                            .checked(mode == current)
                            .on_click(move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.settings.permissions = mode;
                                    let settings = this.settings.clone();
                                    this.host_call(cx, |host| host.save_settings(settings));
                                    cx.notify();
                                });
                            }),
                    );
                }
                menu
            })
            .into_any_element()
    }
    fn prompt_box(&self, cx: &mut Context<Self>) -> AnyElement {
        let model = if self.settings.provider.model.is_empty() {
            "Select model".to_string()
        } else {
            self.settings.provider.model.clone()
        };
        let project = self
            .workspace
            .as_ref()
            .map(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_else(|| "Work in a project".into());
        div()
            .w_full()
            .max_w(px(760.))
            .when(!self.speech.transcript.is_empty(), |panel| {
                panel.child(
                    button("review-voice-transcript", "Review voice transcript")
                        .text_xs()
                        .on_click(cx.listener(|view, _, _, cx| view.navigate(Page::Voice, cx))),
                )
            })
            .child(
                div()
                    .id("image-drop-target")
                    .w_full()
                    .rounded(px(18.))
                    .bg(rgb(panel()))
                    .border_1()
                    .border_color(rgb(border()))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .on_drop(cx.listener(|view, paths: &ExternalPaths, _, cx| {
                        view.attach_image_paths(paths.paths().to_vec(), AttachmentSource::Drop, cx)
                    }))
                    .child(
                        div().min_h(px(56.)).px_1().py_1().child(
                            Textarea::new(&self.composer)
                                .aria_label("Message")
                                .appearance(false)
                                .bordered(false)
                                .disabled(self.draft_flush_pending()),
                        ),
                    )
                    .when(
                        !self.draft_attachments().is_empty() || self.attachments.importing(),
                        |surface| surface.child(self.attachment_strip(cx)),
                    )
                    .when(!self.attachments.notice.is_empty(), |surface| {
                        surface.child(subtitle(self.attachments.notice.clone()))
                    })
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_1()
                            .min_h(px(32.))
                            .child(self.attachment_controls(cx))
                            .child(div().flex_1().min_w_0())
                            .child(self.model_switcher(model, cx))
                            .child(self.permission_switcher(cx))
                            .when(self.running(), |row| {
                                row.child(
                                    focus_ring(gpui_kit::base::Button::new("cancel-task"))
                                        .accessibility_label("Stop the task (Esc)")
                                        .h(px(28.))
                                        .px_2()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .rounded_md()
                                        .text_xs()
                                        .text_color(rgb(danger()))
                                        .cursor_pointer()
                                        .hover(|style| style.bg(rgb(danger_bg())))
                                        .child(icon(IconName::Square).size(px(12.)))
                                        .child("Stop")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.engine_command(WorkerCommand::Cancel, cx)
                                        })),
                                )
                            })
                            .child(self.speech_capture_button(cx))
                            .child(
                                gpui_kit::base::Button::new("send-prompt")
                                    .accessibility_label("Send message")
                                    .size(px(32.))
                                    .rounded_full()
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(rgb(accent()))
                                    .text_color(rgb(0xffffff))
                                    .cursor_pointer()
                                    .disabled(
                                        self.attachments.importing() || self.draft_flush_pending(),
                                    )
                                    .child(icon(IconName::ArrowUp))
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.submit(window, cx)),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .px_1()
                    .pt_1()
                    .child(
                        compact_selector("composer-project", project)
                            .max_w(px(220.))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.pick_workspace(window, cx)),
                            ),
                    )
                    .child(div().flex_1().min_w_0().child(self.draft_status_footer(cx))),
            )
            .into_any_element()
    }
}
use crate::state::{ToolCard, TurnState};
use gpui_kit::component::{Sizable, spinner::Spinner};
use serde_json::Value;

#[derive(Clone, Copy)]
enum Tone {
    Muted,
    Bad,
}
impl Tone {
    fn color(self) -> u32 {
        match self {
            Tone::Muted => muted(),
            Tone::Bad => danger(),
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
fn summary_field<'a>(output: &'a Option<Value>, key: &str) -> Option<&'a Value> {
    output.as_ref()?.get("summary")?.get(key)
}
fn elapsed(tool: &ToolCard) -> Option<(String, Tone)> {
    let ms = tool.finished_at? - tool.started_at;
    (ms >= 1000).then(|| (format!("{:.1} s", ms as f64 / 1000.0), Tone::Muted))
}
fn describe(tool: &ToolCard) -> ActionSummary {
    let input = &tool.input;
    let output = tool.output();
    let path = text_arg(input, "path").to_owned();
    let count = |key: &str, unit: &str| {
        summary_field(&output, key)
            .and_then(Value::as_u64)
            .map(|n| {
                (
                    format!("{n} {unit}{}", if n == 1 { "" } else { "s" }),
                    Tone::Muted,
                )
            })
    };
    let changes = || {
        let added = input.get("added").and_then(Value::as_u64)?;
        let removed = input.get("removed").and_then(Value::as_u64)?;
        Some((format!("+{added} −{removed}"), Tone::Muted))
    };
    let (verb, target, meta, question): (&str, String, Option<(String, Tone)>, String) = match tool
        .name
        .as_str()
    {
        "file_read" => {
            let lines = match (
                summary_field(&output, "start_line").and_then(Value::as_u64),
                summary_field(&output, "end_line").and_then(Value::as_u64),
                summary_field(&output, "total_lines").and_then(Value::as_u64),
            ) {
                (Some(start), Some(end), Some(total)) if end > 0 && (start > 1 || end < total) => {
                    Some((format!("lines {start}–{end} of {total}"), Tone::Muted))
                }
                (_, _, Some(total)) => Some((format!("{total} lines"), Tone::Muted)),
                _ => None,
            };
            ("Read", path.clone(), lines, format!("Read {path}?"))
        }
        "file_edit" => (
            "Edit",
            path.clone(),
            changes(),
            format!("Allow Xylo to edit {path}?"),
        ),
        "file_write" => {
            let created = input.get("created").and_then(Value::as_bool) == Some(true);
            (
                if created { "Create" } else { "Write" },
                path.clone(),
                changes(),
                format!(
                    "Allow Xylo to {} {path}?",
                    if created { "create" } else { "overwrite" }
                ),
            )
        }
        "files_list" => (
            "List",
            if path.is_empty() {
                ".".into()
            } else {
                path.clone()
            },
            None,
            "List this folder?".into(),
        ),
        "find_files" => (
            "Find files",
            text_arg(input, "pattern").to_owned(),
            count("count", "file"),
            "Find files?".into(),
        ),
        "search_project" => {
            let mut target = format!("“{}”", text_arg(input, "query"));
            if let Some(glob) = input.get("glob").and_then(Value::as_str) {
                target.push_str(&format!(" in {glob}"));
            }
            (
                "Search",
                target,
                count("matches", "match"),
                "Search the project?".into(),
            )
        }
        "git_status" => (
            "Git status",
            String::new(),
            None,
            "Check Git status?".into(),
        ),
        "git_diff" => (
            "Git diff",
            if input.get("staged").and_then(Value::as_bool) == Some(true) {
                "staged".into()
            } else {
                path.clone()
            },
            None,
            "Read the Git diff?".into(),
        ),
        "run_command" | "shell_command" => {
            let exit = output
                .as_ref()
                .and_then(|o| {
                    o.get("exit_code")
                        .or_else(|| o.pointer("/summary/exit_code"))
                })
                .and_then(Value::as_i64);
            let meta = match (exit, tool.success) {
                (Some(code), _) if code != 0 => Some((format!("exit {code}"), Tone::Bad)),
                _ => elapsed(tool),
            };
            (
                "Run",
                command_line(tool),
                meta,
                "Allow Xylo to run this command in your project?".into(),
            )
        }
        "agent_delegate" => (
            "Delegate",
            text_arg(input, "task").to_owned(),
            elapsed(tool),
            "Start a helper agent?".into(),
        ),
        "memory_search" => (
            "Recall",
            text_arg(input, "query").to_owned(),
            None,
            "Search project memory?".into(),
        ),
        "memory_remember" => (
            "Remember",
            text_arg(input, "content").to_owned(),
            None,
            "Allow Xylo to remember this fact for the project?".into(),
        ),
        "search_tools" => (
            "Find tools",
            text_arg(input, "query").to_owned(),
            None,
            "Look up tools?".into(),
        ),
        "connector_call" => (
            "Use connector",
            text_arg(input, "tool").to_owned(),
            None,
            "Allow Xylo to use this connected service?".into(),
        ),
        "connector_tools" => (
            "Connect",
            text_arg(input, "connector_id").to_owned(),
            None,
            "Allow Xylo to start this connector?".into(),
        ),
        "call_tool" => (
            "Use",
            text_arg(input, "name").replace('_', " "),
            None,
            "Allow this action?".into(),
        ),
        name => {
            let target = ["command", "path", "query", "url", "task", "name"]
                .iter()
                .find_map(|key| input.get(*key).and_then(Value::as_str))
                .unwrap_or_default()
                .to_owned();
            let verb = name.replace('_', " ");
            let question = format!("Allow Xylo to {verb}?");
            return ActionSummary {
                verb: capitalize(&verb),
                target,
                meta: elapsed(tool),
                question,
            };
        }
    };
    ActionSummary {
        verb: verb.into(),
        target,
        meta: if tool.success == Some(false) && meta.is_none() {
            Some(("failed".into(), Tone::Bad))
        } else {
            meta
        },
        question,
    }
}
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
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
            Spinner::new()
                .small()
                .color(rgb(accent()).into())
                .into_any_element(),
            if turn.status.is_empty() || turn.status == "Working" {
                "Working…".into()
            } else {
                turn.status.clone()
            },
        )
    };
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .py_2()
        .text_sm()
        .child(indicator)
        .child(div().text_color(rgb(text())).child(label))
        .child(subtitle(format!("{} · Esc to stop", elapsed_label(turn))))
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
        .gap_2()
        .text_sm()
        .child(icon(IconName::BookOpen).text_color(rgb(muted())))
        .child(div().font_weight(FontWeight::MEDIUM).child(if latest {
            "Plan"
        } else {
            "Updated plan"
        }))
        .child(subtitle(format!("{done} of {} done", items.len())));
    if !latest {
        return div()
            .id(("plan", i))
            .px_2()
            .py_1()
            .child(heading)
            .into_any_element();
    }
    let mut list = div().flex().flex_col().gap_1();
    for (index, (content, status)) in items.into_iter().enumerate() {
        let (glyph, tone, weight) = match status.as_str() {
            "completed" => (IconName::CircleCheck, success(), FontWeight::NORMAL),
            "in_progress" => (IconName::ChevronRight, accent(), FontWeight::MEDIUM),
            _ => (IconName::Square, muted(), FontWeight::NORMAL),
        };
        list = list.child(
            div()
                .id(("plan-item", i * 1000 + index))
                .flex()
                .items_start()
                .gap_2()
                .text_sm()
                .child(icon(glyph).size(px(14.)).mt(px(2.)).text_color(rgb(tone)))
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
        .gap_2()
        .p_3()
        .rounded_lg()
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
                .child("… more lines in the full diff"),
        );
    }
    div()
        .id(id)
        .w_full()
        .max_h(px(360.))
        .overflow_scroll()
        .py_1()
        .rounded_md()
        .bg(rgb(code_bg()))
        .border_1()
        .border_color(rgb(border()))
        .font_family("monospace")
        .text_size(px(12.))
        .child(body)
        .into_any_element()
}
/// Selectable monospace text (commands, output, search results).
fn code_block(id: impl Into<ElementId>, text: &str) -> AnyElement {
    let fence = if text.contains("```") { "````" } else { "```" };
    div()
        .w_full()
        .max_h(px(360.))
        .overflow_hidden()
        .rounded_md()
        .child(
            TextView::markdown(
                id,
                format!("{fence}text\n{}\n{fence}", truncate(text, 6000)),
            )
            .selectable(true),
        )
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
fn tool_detail(i: usize, tool: &ToolCard) -> AnyElement {
    let output = tool.output();
    let summary = output.as_ref().and_then(|o| o.get("summary"));
    let field = |key: &str| summary.and_then(|s| s.get(key)).and_then(Value::as_str);
    let raw = tool.result.clone().unwrap_or_default();
    let mut detail = div().flex().flex_col().gap_2();
    match tool.name.as_str() {
        "file_write" | "file_edit" => {
            let diff = field("diff").unwrap_or_else(|| text_arg(&tool.input, "diff"));
            if !diff.trim().is_empty() {
                detail = detail.child(diff_view(("tool-diff", i), diff));
            }
        }
        "run_command" | "shell_command" => {
            let mut text = format!("$ {}\n", command_line(tool));
            match (field("stdout"), field("stderr")) {
                (None, None) if !raw.is_empty() && output.is_none() => text.push_str(&raw),
                (stdout, stderr) => {
                    text.push_str(stdout.unwrap_or_default());
                    if let Some(stderr) = stderr.filter(|s| !s.trim().is_empty()) {
                        text.push_str("\n[stderr]\n");
                        text.push_str(stderr);
                    }
                }
            }
            detail = detail.child(code_block(("tool-output", i), &text));
        }
        "file_read" => {
            if let Some(content) = field("content") {
                detail = detail.child(code_block(("tool-output", i), content));
            }
        }
        "search_project" => {
            if let Some(results) = field("results") {
                detail = detail.child(code_block(("tool-output", i), results));
            }
        }
        "find_files" => {
            let files: Vec<&str> = summary
                .and_then(|s| s.get("files"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            detail = detail.child(code_block(("tool-output", i), &files.join("\n")));
        }
        _ => {
            detail = detail.child(code_block(
                ("tool-input", i),
                &serde_json::to_string_pretty(&tool.input).unwrap_or_default(),
            ));
        }
    }
    let handled = detail_handled(tool.name.as_str(), &output);
    if !handled && !raw.is_empty() {
        let shown = output
            .as_ref()
            .map(|value| serde_json::to_string_pretty(value).unwrap_or_default())
            .unwrap_or(raw);
        detail = detail.child(code_block(("tool-result", i), &shown));
    }
    detail.into_any_element()
}
/// Whether the structured view above already shows the useful part of the result.
fn detail_handled(name: &str, output: &Option<Value>) -> bool {
    let Some(summary) = output.as_ref().and_then(|o| o.get("summary")) else {
        return matches!(name, "run_command" | "shell_command");
    };
    match name {
        "file_write" | "file_edit" => summary.get("diff").is_some(),
        "run_command" | "shell_command" => {
            summary.get("stdout").is_some() || summary.get("stderr").is_some()
        }
        "file_read" => summary.get("content").is_some(),
        "search_project" => summary.get("results").is_some(),
        "find_files" => summary.get("files").is_some(),
        _ => false,
    }
}
