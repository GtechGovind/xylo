//! A call's detail view: only the human-readable presentation (file rows, an
//! excerpt, search hits, the full command and its output, a diff), then the
//! copy actions. Raw input and output are copied, never rendered. Parsing and
//! bounding happen in the state reducer.
use super::tools::{detail_action, diff_view};
use super::*;
use crate::icons::{self, AppIcon};
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
                .items_center()
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
                        .single_line()
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
        .child(format!("{count} more"))
        .into_any_element()
}
impl Xylo {
    /// Copies a call's complete arguments as JSON.
    fn copy_tool_input(&self, id: Uuid, cx: &mut Context<Self>) {
        if let Some(tool) = self.turns.get(&self.session).and_then(|turn| turn.tool(id)) {
            cx.write_to_clipboard(ClipboardItem::new_string(
                tool.presentation.pretty_input.to_string(),
            ));
        }
    }
    /// Copies a call's complete result, read at click time so rendering never
    /// clones large outputs.
    fn copy_tool_result(&self, id: Uuid, cx: &mut Context<Self>) {
        if let Some(result) = self
            .turns
            .get(&self.session)
            .and_then(|turn| turn.tool(id))
            .and_then(|tool| tool.result.as_ref())
        {
            cx.write_to_clipboard(ClipboardItem::new_string(result.clone()));
        }
    }
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
                                icons::small(if entry.directory {
                                    AppIcon::Folder
                                } else {
                                    AppIcon::File
                                })
                                .text_color(rgb(muted())),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .single_line()
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
                                    .child(icons::small(AppIcon::File))
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
                            .whitespace_normal()
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
                            .child("Output shortened · Copy output for all of it"),
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
        let has_output = tool.result.is_some();
        let id = tool.id;
        body.child(
            div()
                .ml(px(-8.))
                .flex()
                .items_center()
                .gap_1()
                .child(
                    detail_action(("tool-copy-input", i), "Copy input")
                        .on_click(cx.listener(move |view, _, _, cx| view.copy_tool_input(id, cx))),
                )
                .when(has_output, |row| {
                    row.child(
                        detail_action(("tool-copy-output", i), "Copy output").on_click(
                            cx.listener(move |view, _, _, cx| view.copy_tool_result(id, cx)),
                        ),
                    )
                }),
        )
        .into_any_element()
    }
}
