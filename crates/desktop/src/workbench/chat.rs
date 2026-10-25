use super::*;
use gpui_kit::component::IconName;

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
                    subtitle("Beginning of conversation").into_any_element()
                })
                .into_any_element()
        } else if let Some(message) = self.messages.get(index - 1) {
            let user = message.role == "user";
            let content = message.content.clone();
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .p_4()
                .rounded_xl()
                .bg(rgb(if user { message_bg() } else { panel() }))
                .when(self.history_message_highlight(message.id), |row| {
                    row.border_1().border_color(rgb(0x8f82e8))
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(if user { "You" } else { "Xylo" }),
                        )
                        .child(button(("copy-message", index), "Copy").text_xs().on_click(
                            move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(content.clone()))
                            },
                        )),
                )
                .child(
                    TextView::markdown(
                        SharedString::from(message.id.to_string()),
                        message.content.clone(),
                    )
                    .selectable(true),
                )
                .child(self.message_images(message, cx))
                .into_any_element()
        } else if let Some(turn) = self.turns.get(&self.session) {
            let offset = index - self.messages.len() - 1;
            if let Some(tool) = turn.tools.get(offset) {
                self.tool_row(offset, tool, cx)
            } else if offset == turn.tools.len()
                && !turn.text.is_empty()
                && (turn.running || turn.status == "Cancelled")
            {
                TextView::markdown("streamed-answer", turn.text.clone())
                    .selectable(true)
                    .into_any_element()
            } else {
                subtitle("Working on your task…").into_any_element()
            }
        } else {
            div().into_any_element()
        };
        div()
            .w_full()
            .flex()
            .justify_center()
            .px_6()
            .pb_5()
            .child(div().w_full().max_w(px(800.)).child(content))
            .into_any_element()
    }
    fn tool_row(
        &self,
        i: usize,
        tool: &crate::state::ToolCard,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = tool.id;
        let expanded = self.expanded_tools.contains(&id);
        let mut action = card().w_full().max_w(px(800.)).child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(icon(IconName::Settings))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(action_label(&tool.name)),
                        ),
                )
                .child(subtitle(if tool.approval {
                    "Needs approval"
                } else if tool.success == Some(true) {
                    "Completed"
                } else if tool.success == Some(false) {
                    "Failed"
                } else {
                    "Working…"
                })),
        );
        if tool.approval {
            let summary = action_description(&tool.arguments);
            action = action.child(div().text_sm().child(summary)).child(
                div()
                    .flex()
                    .gap_2()
                    .child(primary(("allow", i), "Allow once").on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.engine_command(
                                WorkerCommand::Approve {
                                    action_id: id,
                                    allow: true,
                                },
                                cx,
                            )
                        },
                    )))
                    .child(button(("deny", i), "Don't allow").on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.engine_command(
                                WorkerCommand::Approve {
                                    action_id: id,
                                    allow: false,
                                },
                                cx,
                            )
                        },
                    ))),
            );
        }
        action = action.child(
            button(
                ("tool-detail", i),
                if expanded {
                    "Hide details"
                } else {
                    "View details"
                },
            )
            .text_xs()
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.expanded_tools.remove(&id) {
                    this.expanded_tools.insert(id);
                }
                cx.notify();
            })),
        );
        if expanded {
            action = action.child(
                TextView::markdown(("tool-args", i), action_description(&tool.arguments))
                    .selectable(true),
            );
            if let Some(result) = &tool.result {
                action = action.child(
                    TextView::markdown(("tool-result", i), truncate(result, 6000)).selectable(true),
                );
            }
        }

        action.into_any_element()
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
        for row in 0..2 {
            let mut cards = div().flex().gap_2();
            for col in 0..3 {
                let index = row * 3 + col;
                let (name, description, prompt, glyph) = ASSISTANTS[index].clone();
                cards = cards.child(
                    gpui_kit::base::Button::new(("assistant", index))
                        .accessibility_label(name)
                        .flex_1()
                        .min_w_0()
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
                gpui_kit::base::Button::new(("mode", index))
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
            .max_w(px(800.))
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
                            .items_center()
                            .gap_1()
                            .h(px(32.))
                            .child(self.attachment_controls(cx))
                            .child(div().flex_1().min_w_0())
                            .child(
                                compact_selector("composer-model", model)
                                    .max_w(px(180.))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.navigate(Page::Providers, cx)
                                    })),
                            )
                            .child(
                                compact_selector(
                                    "permission-mode",
                                    if self.settings.permissions == PermissionMode::ReadOnly {
                                        "Read only"
                                    } else {
                                        "Ask before changes"
                                    },
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.settings.permissions = if this.settings.permissions
                                            == PermissionMode::ReadOnly
                                        {
                                            PermissionMode::AskForWrites
                                        } else {
                                            PermissionMode::ReadOnly
                                        };
                                        let settings = this.settings.clone();
                                        this.host_call(cx, |host| host.save_settings(settings));
                                    },
                                )),
                            )
                            .when(self.running(), |row| {
                                row.child(button("cancel-task", "Stop").text_xs().on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.engine_command(WorkerCommand::Cancel, cx)
                                    }),
                                ))
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
fn action_label(name: &str) -> String {
    match name {
        "read_file" | "file_read" => "Read a file".into(),
        "write_file" | "file_write" | "apply_patch" => "Update project files".into(),
        "run_command" | "shell" | "terminal" | "execute" => "Run a command".into(),
        "web_search" | "search" => "Search for information".into(),
        "browser" | "browser_navigate" => "Browse a page".into(),
        "agent_delegate" => "Work with a teammate".into(),
        "memory_search" => "Recall project memory".into(),
        "memory_remember" => "Remember a decision".into(),
        _ => name.replace('_', " "),
    }
}
fn action_description(arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return truncate(arguments, 800);
    };
    for key in ["command", "path", "query", "url", "task", "content"] {
        if let Some(text) = value.get(key).and_then(|v| v.as_str()) {
            return truncate(text, 800);
        }
    }
    "This action uses your project and connected tools.".into()
}
