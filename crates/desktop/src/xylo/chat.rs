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
                    .px_6()
                    .pb_6()
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
                .text_size(px(12.)).line_height(relative(if user {1.75} else {1.85}))
                .when(user, |row| {
                    row.max_w(px(695.))
                        .px(px(15.))
                        .py(px(13.))
                        .rounded(px(12.))
                        .border_1()
                        .border_color(rgb(border()))
                        .bg(rgb(sidebar()))
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
            div().id(("chat-message", index)).test_support()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .group("chat-message")
                .when(user, |row| row.items_end())
                .child(body)
                .child(
                    gpui_kit::base::Button::new(("copy-message", index))
                        .accessibility_label("Copy message")
                        .opacity(0.).group_hover("chat-message",|style|style.opacity(1.)).focus(|style|style.opacity(1.))
                        .size(px(22.)).rounded_md().flex().items_center().justify_center()
                        .child(icon(IconName::Copy).size(px(11.)).text_color(rgb(muted())))
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
            .child(div().w_full().max_w(px(740.)).child(content))
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
        let summary = ActionSummary {verb:cached.verb.clone(),target:cached.target.clone(),meta:cached.meta.clone().map(|text|(text,Tone::Muted)),question:cached.question.clone()};
        let (state, state_word): (AnyElement, &str) = if tool.approval {
            (
                icon(IconName::TriangleAlert).size(px(11.))
                    .text_color(rgb(warning()))
                    .into_any_element(),
                "needs approval",
            )
        } else {
            match tool.success {
                None => (
                    div().size(px(5.)).rounded_full().bg(rgb(muted())).into_any_element(),
                    "in progress",
                ),
                Some(true) => (
                    icon(IconName::CircleCheck).size(px(11.))
                        .text_color(rgb(success()))
                        .into_any_element(),
                    "done",
                ),
                Some(false) => (
                    icon(IconName::CircleX).size(px(11.))
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
            .px(px(9.))
            .py(px(7.))
            .rounded(px(7.))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(hover())))
            .child(div().flex_shrink_0().child(state))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.))
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
                    .text_size(px(11.))
                    .font_family(gpui_kit::component::Theme::global(cx).mono_font_family.clone())
                    .text_color(rgb(muted()))
                    .child(summary.target.clone()),
            )
            .when_some(summary.meta.clone(), |row, (meta, tone)| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(10.))
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
                .size(px(11.))
                .text_color(rgb(muted())),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.expanded_tools.remove(&id) {
                    this.expanded_tools.insert(id);
                }
                this.invalidate_live_history();
                this.sync_history_list(cx);
                cx.notify();
            }));
        let mut card = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_0()
            .border_1().border_color(rgb(border())).rounded(px(7.))
            .when(tool.approval, |card| {
                card.p_3()
                    .rounded_lg()
                    .bg(rgb(warning_bg()))
                    .border_1()
                    .border_color(rgb(warning()))
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
        if let Some(error)=&cached.error {
            card=card.child(div().px_3().pb_2().text_size(px(11.)).text_color(rgb(danger())).child(error.clone()));
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
        div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .px_6()
            .pb(px(110.))
            .child(
                div()
                    .w_full()
                    .max_w(px(740.))
                    .flex()
                    .flex_col()
                    .gap_6()
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_center()
                            .child("What are we working on?"),
                    )
                    .child(self.prompt_box(cx)),
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
            .disabled(self.attachments.importing())
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
            .bg(rgb(panel())).border_1().border_color(rgb(border())).rounded(px(16.))
            .trigger_style(trigger_layout.style().clone())
            .anchor(Anchor::BottomRight)
            .offset(px(8.))
            .trigger(
                MenuButton::new("composer-model")
                    .ghost()
                    .bg(rgb(panel()))
                    .label(label)
                    .h(px(30.))
                    .max_w(px(250.))
                    .w_full()
                    .min_w_0()
                    .flex_shrink(1.)
                    .gap_1()
                    .child(
                        div()
                            .flex_shrink_0()
                            .whitespace_nowrap()
                            .text_color(rgb(muted()))
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
                    .relative().h(px(32.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_center()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(accent()))
                            .child(title),
                    )
                    .when(!efforts.is_empty(), |row| {
                        row.child(
                            MenuButton::new("composer-effort-default")
                                .absolute().right_0().top_0()
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
                        .child(div().flex_shrink_0().size(px(if selected {22.} else {4.})).rounded_full().bg(rgb(if selected {text()} else {muted()})))
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
            .max_w(px(144.))
            .h(px(30.))
            .text_xs()
            .text_color(rgb(muted()))
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
            .w(px(28.)).px_0()
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
        let model = if self.settings.provider.model.is_empty() {
            "Select model".to_string()
        } else {
            self.settings.provider.model.clone()
        };
        div()
            .w_full()
            .max_w(px(740.))
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
                    .rounded(px(22.))
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
                        div().min_h(px(62.)).px_1().py_1().child(
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
                            .items_center()
                            .gap_1()
                            .min_h(px(32.))
                            .child(self.composer_context_actions(cx))
                            .child(self.composer_permissions(cx))
                            .child(div().flex_1().min_w_0())
                            .child(self.composer_context(cx))
                            .child(self.unified_model_control(model, cx))
                            .child(self.speech_capture_button(cx))
                            .child({
                                let stop = self.running()
                                    && self.composer.read(cx).value().trim().is_empty()
                                    && self.draft_attachments().is_empty();
                                gpui_kit::base::Button::new(if stop {
                                    "cancel-task"
                                } else {
                                    "send-prompt"
                                })
                                .accessibility_label(if stop {
                                    "Stop the task (Esc)"
                                } else if self.running() {
                                    "Send guidance"
                                } else {
                                    "Send message"
                                })
                                .size(px(32.))
                                .rounded_full()
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(rgb(text()))
                                .text_color(rgb(panel()))
                                .cursor_pointer()
                                .disabled(
                                    !stop
                                        && (self.attachments.importing()
                                            || self.draft_flush_pending()),
                                )
                                .child(
                                    icon(if stop {
                                        IconName::Square
                                    } else {
                                        IconName::ArrowUp
                                    })
                                    .size(px(if stop {
                                        12.
                                    } else {
                                        18.
                                    })),
                                )
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        if stop {
                                            this.engine_command(WorkerCommand::Cancel, cx)
                                        } else {
                                            this.submit(window, cx)
                                        }
                                    },
                                ))
                            }),
                    ),
            )
            .when(self.draft_needs_attention(), |surface| {
                surface.child(div().pt_1().child(self.draft_status_footer(cx)))
            })
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
