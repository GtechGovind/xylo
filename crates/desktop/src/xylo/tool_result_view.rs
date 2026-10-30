//! Compact native tool bodies. Parsing and bounding happen in the state reducer.
use super::*;
use crate::state::tool_presentation::{OutputLine, ToolBody};
use gpui_kit::base::TestSupportExt;

fn output_lines(lines: &[OutputLine], cx: &Context<Xylo>) -> AnyElement {
    let mut body = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .text_size(px(11.))
        .font_family(
            gpui_kit::component::Theme::global(cx)
                .mono_font_family
                .clone(),
        );
    for line in lines {
        body = body.child(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .gap_3()
                .when_some(line.number, |row, number| {
                    row.child(
                        div()
                            .w(px(36.))
                            .flex_shrink_0()
                            .text_color(rgb(muted()))
                            .child(number.to_string()),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(line.text.clone()),
                ),
        );
    }
    body.into_any_element()
}
fn remaining(count: usize) -> AnyElement {
    div()
        .pt_1()
        .text_size(px(10.))
        .text_color(rgb(muted()))
        .child(format!("{count} more · output preview in Raw details"))
        .into_any_element()
}
impl Xylo {
    pub(super) fn structured_tool_details(
        &self,
        i: usize,
        tool: &ToolCard,
        cx: &Context<Self>,
    ) -> AnyElement {
        let mut body = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(rgb(border()));
        match &tool.presentation.body {
            ToolBody::Empty => {}
            ToolBody::Files {
                entries,
                remaining: count,
            } => {
                let mut files = div()
                    .id(("tool-files", i))
                    .test_support()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1();
                for entry in entries {
                    files = files.child(
                        div()
                            .flex()
                            .min_w_0()
                            .items_center()
                            .gap_2()
                            .py(px(2.))
                            .child(
                                icon(if entry.directory {
                                    IconName::Folder
                                } else {
                                    IconName::File
                                })
                                .size(px(12.))
                                .text_color(rgb(muted())),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_size(px(11.))
                                    .child(entry.name.clone()),
                            ),
                    );
                }
                body = body
                    .child(files)
                    .when(*count > 0, |body| body.child(remaining(*count)));
            }
            ToolBody::Excerpt {
                lines,
                remaining: count,
            } => {
                body = body
                    .child(
                        div()
                            .id(("tool-read", i))
                            .test_support()
                            .child(output_lines(lines, cx)),
                    )
                    .when(*count > 0, |body| body.child(remaining(*count)));
            }
            ToolBody::Search {
                groups,
                remaining: count,
            } => {
                let mut search = div()
                    .id(("tool-search", i))
                    .test_support()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_3();
                for group in groups {
                    search = search.child(
                        div()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .text_size(px(11.))
                                    .text_color(rgb(muted()))
                                    .child(icon(IconName::File).size(px(11.)))
                                    .child(group.path.clone()),
                            )
                            .child(output_lines(&group.matches, cx)),
                    );
                }
                body = body
                    .child(search)
                    .when(*count > 0, |body| body.child(remaining(*count)));
            }
            ToolBody::Command {
                command,
                exit,
                stdout,
                stderr,
                truncated,
            } => {
                let mut command_body = div()
                    .id(("tool-command", i))
                    .test_support()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(11.))
                            .font_family(
                                gpui_kit::component::Theme::global(cx)
                                    .mono_font_family
                                    .clone(),
                            )
                            .child(format!("$ {command}")),
                    );
                if !stdout.is_empty() {
                    command_body = command_body.child(output_lines(stdout, cx));
                }
                if !stderr.is_empty() {
                    command_body = command_body.child(
                        div()
                            .text_color(rgb(if exit.is_some_and(|code| code != 0) {
                                danger()
                            } else {
                                muted()
                            }))
                            .child(output_lines(stderr, cx)),
                    );
                }
                if *truncated {
                    command_body = command_body.child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(muted()))
                            .child("Output shortened · output preview in Raw details"),
                    );
                }
                body = body.child(command_body);
            }
            ToolBody::Markdown(text) => {
                body = body.child(
                    div()
                        .id(("tool-delegate", i))
                        .test_support()
                        .text_size(px(12.))
                        .line_height(relative(1.75))
                        .child(
                            TextView::markdown(("tool-answer", i), text.clone()).selectable(true),
                        ),
                )
            }
            ToolBody::Diff(text) => body = body.child(diff_view(("tool-change", i), text)),
            ToolBody::Message(text) => {
                body = body.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(muted()))
                        .child(text.clone()),
                )
            }
        }
        let raw = self.raw_tool_details.contains(&tool.id);
        let id = tool.id;
        body = body.child(
            button(
                ("tool-raw-toggle", i),
                if raw {
                    "Hide raw details"
                } else {
                    "Raw details"
                },
            )
            .text_size(px(10.))
            .text_color(rgb(muted()))
            .self_start()
            .on_click(cx.listener(move |view, _, _, cx| {
                if !view.raw_tool_details.remove(&id) {
                    view.raw_tool_details.insert(id);
                }
                view.invalidate_live_history();
                view.sync_history_list(cx);
                cx.notify();
            })),
        );
        if raw {
            body = body.child(
                div()
                    .id(("tool-raw-input", i))
                    .test_support()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(muted()))
                            .child("Arguments preview"),
                    )
                    .child(button(("tool-copy-input", i), "Copy full arguments")
                        .text_size(px(10.))
                        .on_click({
                            let input = serde_json::to_string_pretty(&tool.input).unwrap_or_default();
                            move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(input.clone()))
                        }))
                    .child(code_block(
                        ("raw-arguments", i),
                        &tool.presentation.raw_input,
                    )),
            );
            if let Some(result) = &tool.presentation.raw_result {
                body = body.child(
                    div()
                        .id(("tool-raw-result", i))
                        .test_support()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(rgb(muted()))
                                        .child("Result preview"),
                                )
                                .child(
                                    button(("tool-copy-raw", i), "Copy full result")
                                        .text_size(px(10.))
                                        .on_click(cx.listener(move |view, _, _, cx| {
                                            if let Some(result) = view
                                                .turns
                                                .get(&view.session)
                                                .and_then(|turn| {
                                                    turn.tools.iter().find(|tool| tool.id == id)
                                                })
                                                .and_then(|tool| tool.result.as_ref())
                                            {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    result.clone(),
                                                ));
                                            }
                                        })),
                                ),
                        )
                        .child(code_block(("raw-result", i), result)),
                );
            }
        }
        body.into_any_element()
    }
}
