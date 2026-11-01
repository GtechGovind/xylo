//! On-demand selectable output, bounded to one page and scoped to its chat.
use super::*;
const PAGE_BYTES: usize = 32 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OutputTarget {
    Action(Uuid),
    Command { turn: Uuid, job: Uuid },
}
#[derive(Default)]
pub(super) struct OutputUi {
    open: Option<OutputDocument>,
    dialog: Option<(Uuid, OutputTarget)>,
    height: f32,
}
struct OutputDocument {
    session: Uuid,
    target: OutputTarget,
    title: String,
    source: Option<std::sync::Arc<str>>,
    page: SharedString,
    raw_page: String,
    offset: u64,
    next: Option<u64>,
    previous: Vec<u64>,
    request: Option<Uuid>,
    error: Option<String>,
    copy_request: Option<Uuid>,
    copied: bool,
    end_requested: bool,
    scroll: ScrollHandle,
}
fn text_page(text: &str, offset: usize) -> (String, Option<u64>) {
    let offset = offset.min(text.len());
    let mut end = (offset + PAGE_BYTES).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (
        text[offset..end].to_owned(),
        (end < text.len()).then_some(end as u64),
    )
}
fn code_text(text: &str) -> SharedString {
    let longest = text
        .split(|ch| ch != '`')
        .map(str::len)
        .max()
        .unwrap_or(0)
        .max(2);
    let fence = "`".repeat(longest + 1);
    format!("{fence}\n{text}\n{fence}").into()
}
impl Xylo {
    pub(super) fn sync_output_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::WindowExt;
        self.output_view.height =
            (f32::from(window.viewport_size().height) - 130.).clamp(180., 650.);
        let active = self
            .output_view
            .open
            .as_ref()
            .filter(|doc| doc.session == self.session)
            .map(|doc| (doc.session, doc.target));
        if self.output_view.dialog == active {
            return;
        }
        if self.output_view.dialog.take().is_some() {
            window.close_dialog(cx);
        }
        self.output_view.dialog = active;
        if let Some(key) = active {
            let weak = cx.entity().downgrade();
            window.open_dialog(cx, move |dialog, _, cx| {
                let content = weak
                    .update(cx, |view, cx| {
                        view.output_panel(cx)
                            .unwrap_or_else(|| div().into_any_element())
                    })
                    .unwrap_or_else(|_| div().into_any_element());
                let close = weak.clone();
                dialog
                    .width(px(720.))
                    .p_0()
                    .rounded(px(16.))
                    .border_1()
                    .bg(rgb(panel()))
                    .border_color(rgb(border_strong()))
                    .close_button(false)
                    .on_close(move |_, _, cx| {
                        let _ = close.update(cx, |view, cx| {
                            if view.output_view.dialog == Some(key) {
                                view.output_view.dialog = None;
                                view.close_output(cx);
                            }
                        });
                    })
                    .child(content)
            });
        }
    }
    pub(super) fn open_tool_output(&mut self, action: Uuid, cx: &mut Context<Self>) {
        let Some(tool) = self
            .turns
            .get(&self.session)
            .and_then(|turn| turn.tool(action))
        else {
            return;
        };
        if let Some(job) = self.tool_command_job(tool).map(|job| job.id) {
            self.open_command_output(job, cx);
            return;
        }
        let Some(result) = tool.result.as_ref() else {
            return;
        };
        let title = format!("{} {}", tool.presentation.verb, tool.presentation.target);
        self.show_action_output(self.session, action, title, result.clone(), cx);
    }
    pub(super) fn show_action_output(
        &mut self,
        session: Uuid,
        action: Uuid,
        title: String,
        output: String,
        cx: &mut Context<Self>,
    ) {
        if session != self.session {
            return;
        }
        let (page, next) = text_page(&output, 0);
        self.output_view.open = Some(OutputDocument {
            session,
            target: OutputTarget::Action(action),
            title,
            source: Some(output.into()),
            page: code_text(&page),
            raw_page: page,
            offset: 0,
            next,
            previous: vec![],
            request: None,
            error: None,
            copy_request: None,
            copied: false,
            end_requested: false,
            scroll: ScrollHandle::new(),
        });
        cx.notify();
    }
    pub(super) fn open_command_output(&mut self, job: Uuid, cx: &mut Context<Self>) {
        let Some(command) = self
            .turns
            .get(&self.session)
            .and_then(|turn| turn.shell_jobs.iter().find(|item| item.id == job))
        else {
            return;
        };
        let turn = command.turn_id;
        let title = command.command.clone();
        self.open_retained_command_output(self.session, turn, job, title, cx);
    }
    pub(super) fn open_retained_command_output(
        &mut self,
        session: Uuid,
        turn: Uuid,
        job: Uuid,
        title: String,
        cx: &mut Context<Self>,
    ) {
        if session != self.session {
            return;
        }
        self.output_view.open = Some(OutputDocument {
            session,
            target: OutputTarget::Command { turn, job },
            title,
            source: None,
            page: "".into(),
            raw_page: String::new(),
            offset: 0,
            next: None,
            previous: vec![],
            request: None,
            error: None,
            copy_request: None,
            copied: false,
            end_requested: false,
            scroll: ScrollHandle::new(),
        });
        self.load_output_page(0, cx);
    }

    fn load_output_page(&mut self, offset: u64, cx: &mut Context<Self>) {
        let Some(doc) = self
            .output_view
            .open
            .as_mut()
            .filter(|doc| doc.session == self.session)
        else {
            return;
        };
        doc.error = None;
        if let Some(source) = &doc.source {
            let (page, next) = text_page(source, offset as usize);
            doc.page = code_text(&page);
            doc.raw_page = page;
            doc.offset = offset;
            doc.next = next;
        } else if let OutputTarget::Command { turn, job } = doc.target {
            let id = Uuid::new_v4();
            match self
                .host
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Task service unavailable"))
                .and_then(|host| host.command_output(id, doc.session, turn, job, offset))
            {
                Ok(()) => {
                    doc.request = Some(id);
                }
                Err(error) => doc.error = Some(error.to_string()),
            }
        }
        cx.notify();
    }
    pub(super) fn command_output_received(
        &mut self,
        id: Uuid,
        session: Uuid,
        turn: Uuid,
        job: Uuid,
        result: Result<xylo_runtime::host::CommandOutputPage, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(doc) = self.output_view.open.as_mut().filter(|doc| {
            doc.request == Some(id)
                && doc.session == session
                && doc.target == (OutputTarget::Command { turn, job })
        }) else {
            return;
        };
        doc.request = None;
        if doc.end_requested {
            doc.scroll.scroll_to_bottom();
            doc.end_requested = false;
        }
        match result { Ok(page) => { doc.offset = page.offset; doc.next = page.next; doc.page = code_text(&page.text); doc.raw_page = page.text; }, Err(_) => doc.error = Some("This command’s full log is unavailable. Its saved output preview is still in the conversation.".into()) }
        cx.notify();
    }
    pub(super) fn command_output_copy_received(
        &mut self,
        id: Uuid,
        session: Uuid,
        turn: Uuid,
        job: Uuid,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(doc) = self.output_view.open.as_mut().filter(|doc| {
            doc.session == session
                && doc.target == (OutputTarget::Command { turn, job })
                && doc.copy_request == Some(id)
        }) else {
            return;
        };
        doc.copy_request = None;
        if self.session != session {
            return;
        }
        match result {
            Ok(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                doc.copied = true;
            }
            Err(error) => doc.error = Some(error),
        }
        cx.notify();
    }
    fn copy_complete_output(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = self
            .output_view
            .open
            .as_mut()
            .filter(|doc| doc.session == self.session && doc.copy_request.is_none())
        else {
            return;
        };
        if let Some(text) = &doc.source {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
            doc.copied = true;
        } else if let OutputTarget::Command { turn, job } = doc.target {
            let id = Uuid::new_v4();
            match self
                .host
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Task service unavailable"))
                .and_then(|host| host.command_output_copy(id, doc.session, turn, job))
            {
                Ok(()) => doc.copy_request = Some(id),
                Err(error) => doc.error = Some(error.to_string()),
            }
        }
        cx.notify();
    }
    fn output_go_to_end(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = self
            .output_view
            .open
            .as_mut()
            .filter(|doc| doc.session == self.session && doc.request.is_none())
        else {
            return;
        };
        if let Some(text) = &doc.source {
            let mut offset = text.len().saturating_sub(PAGE_BYTES);
            while !text.is_char_boundary(offset) {
                offset += 1;
            }
            if offset as u64 != doc.offset {
                doc.previous.push(doc.offset);
            }
            self.load_output_page(offset as u64, cx);
        } else if let OutputTarget::Command { turn, job } = doc.target {
            let id = Uuid::new_v4();
            match self
                .host
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Task service unavailable"))
                .and_then(|host| host.command_output_end(id, doc.session, turn, job))
            {
                Ok(()) => {
                    doc.previous.push(doc.offset);
                    doc.request = Some(id);
                    doc.end_requested = true;
                }
                Err(error) => doc.error = Some(error.to_string()),
            }
        }
        if let Some(doc) = self.output_view.open.as_mut() {
            doc.scroll.scroll_to_bottom();
        }
        cx.notify();
    }
    pub(super) fn close_output(&mut self, cx: &mut Context<Self>) -> bool {
        if self
            .output_view
            .open
            .as_ref()
            .is_some_and(|doc| doc.session == self.session)
        {
            self.output_view.open = None;
            cx.notify();
            true
        } else {
            false
        }
    }
    pub(super) fn output_panel(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let doc = self
            .output_view
            .open
            .as_ref()
            .filter(|doc| doc.session == self.session)?;
        let next = doc.next;
        let pending = doc.request.is_some();
        Some(
            div()
                .id("chat-full-output")
                .test_support()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation()
                })
                .on_key_down(cx.listener(|view, event: &KeyDownEvent, _, cx| {
                    if event.keystroke.key == "escape" {
                        view.close_output(cx);
                        cx.stop_propagation();
                    }
                }))
                .child(
                    div()
                        .w(px(720.))
                        .max_w_full()
                        .h(px(self.output_view.height.max(180.)))
                        .max_h_full()
                        .min_h_0()
                        .rounded(px(16.))
                        .bg(rgb(panel()))
                        .overflow_hidden()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(10.))
                                .px(px(20.))
                                .py(px(16.))
                                .border_b_1()
                                .border_color(rgb(border_strong()))
                                .child(
                                    div()
                                        .flex_1()
                                        .text_size(px(15.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .child("Command output"),
                                )
                                .child(
                                    icons::icon_button(
                                        "close-chat-output",
                                        AppIcon::Close,
                                        "Close output",
                                    )
                                    .size(px(34.))
                                    .on_click(cx.listener(
                                        |view, _, _, cx| {
                                            view.close_output(cx);
                                        },
                                    )),
                                ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .p(px(20.))
                                .flex()
                                .flex_col()
                                .gap(px(14.))
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(rgb(muted()))
                                        .font_family(mono(cx))
                                        .child(doc.title.clone()),
                                )
                                .when_some(doc.error.clone(), |panel, error| {
                                    panel.child(subtitle(error))
                                })
                                .child(
                                    div()
                                        .id("chat-output-page")
                                        .test_support()
                                        .track_scroll(&doc.scroll)
                                        .flex_1()
                                        .min_h_0()
                                        .overflow_scroll()
                                        .rounded(px(8.))
                                        .bg(rgb(bg()))
                                        .text_size(px(12.5))
                                        .line_height(relative(1.8))
                                        .child(
                                            TextView::markdown(
                                                SharedString::from(format!(
                                                    "output-{:?}-{}",
                                                    doc.target, doc.offset
                                                )),
                                                doc.page.clone(),
                                            )
                                            .style(
                                                chat::tokens::response_style().with_code_block(
                                                    StyleRefinement::default()
                                                        .p(px(14.))
                                                        .border_0()
                                                        .rounded(px(8.))
                                                        .bg(rgb(bg()))
                                                        .text_size(px(12.5))
                                                        .line_height(relative(1.8))
                                                        .text_color(rgb(text_secondary())),
                                                ),
                                            )
                                            .selectable(true),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .gap(px(8.))
                                        .items_center()
                                        .flex_wrap()
                                        .child(
                                            secondary(
                                                "copy-full-chat-output",
                                                if doc.copy_request.is_some() {
                                                    "Copying…"
                                                } else if doc.copied {
                                                    "Copied"
                                                } else {
                                                    "Copy output"
                                                },
                                            )
                                            .text_size(px(13.))
                                            .disabled(doc.copy_request.is_some())
                                            .on_click(
                                                cx.listener(|view, _, _, cx| {
                                                    view.copy_complete_output(cx)
                                                }),
                                            ),
                                        )
                                        .child(
                                            button("output-go-to-end", "Go to end")
                                                .disabled(pending)
                                                .on_click(cx.listener(|view, _, _, cx| {
                                                    view.output_go_to_end(cx)
                                                })),
                                        )
                                        .child(div().flex_1())
                                        .when(!doc.previous.is_empty() || next.is_some(), |row| {
                                            row.child(
                                                secondary("output-previous-page", "Previous page")
                                                    .disabled(pending || doc.previous.is_empty())
                                                    .on_click(cx.listener(|view, _, _, cx| {
                                                        if let Some(offset) = view
                                                            .output_view
                                                            .open
                                                            .as_mut()
                                                            .and_then(|doc| doc.previous.pop())
                                                        {
                                                            view.load_output_page(offset, cx);
                                                        }
                                                    })),
                                            )
                                            .child(
                                                secondary("output-next-page", "Next page")
                                                    .disabled(pending || next.is_none())
                                                    .on_click(cx.listener(
                                                        move |view, _, _, cx| {
                                                            if let Some(next) = next {
                                                                if let Some(doc) =
                                                                    view.output_view.open.as_mut()
                                                                {
                                                                    doc.previous.push(doc.offset);
                                                                }
                                                                view.load_output_page(next, cx);
                                                            }
                                                        },
                                                    )),
                                            )
                                        })
                                        .when(doc.source.is_none(), |bar| {
                                            bar.child(
                                                secondary("output-refresh", "Refresh")
                                                    .disabled(pending)
                                                    .on_click(cx.listener(|view, _, _, cx| {
                                                        if let Some(offset) = view
                                                            .output_view
                                                            .open
                                                            .as_ref()
                                                            .map(|doc| doc.offset)
                                                        {
                                                            view.load_output_page(offset, cx);
                                                        }
                                                    })),
                                            )
                                        })
                                        .when(pending, |bar| {
                                            bar.child(subtitle("Loading output…"))
                                        }),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}
#[cfg(all(test, not(feature = "validation")))]
mod tests {
    use super::{PAGE_BYTES, text_page};
    #[test]
    fn full_output_pages_preserve_long_lines_unicode_and_final_errors() {
        let text = format!(
            "{}\n{}final error",
            "x".repeat(600),
            "output αβγ\n".repeat(5000)
        );
        let mut offset = 0;
        let mut reconstructed = String::new();
        loop {
            let (page, next) = text_page(&text, offset);
            assert!(page.len() <= PAGE_BYTES);
            reconstructed.push_str(&page);
            if let Some(next) = next {
                offset = next as usize;
            } else {
                break;
            }
        }
        assert_eq!(text, reconstructed);
        assert_eq!(text_page("", 0), (String::new(), None));
    }
}

#[cfg(all(test, not(feature = "validation")))]
mod response_tests {
    use super::{OutputTarget, Uuid, Xylo};
    use gpui_kit::{AppContext, WindowOptions};
    #[gpui_kit::test]
    fn owned_output_finishes_loading_while_another_chat_is_open(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let (window, view) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("fixture")), window, cx))
            })
            .unwrap()
        });
        cx.update_window(window, |_, _, cx| {
            view.update(cx, |view, cx| {
                let owner = view.session;
                let turn = Uuid::new_v4();
                let job = Uuid::new_v4();
                let request = Uuid::new_v4();
                view.show_action_output(owner, Uuid::new_v4(), "Output".into(), "".into(), cx);
                let doc = view.output_view.open.as_mut().unwrap();
                doc.target = OutputTarget::Command { turn, job };
                doc.source = None;
                doc.request = Some(request);
                view.session = Uuid::new_v4();
                view.command_output_received(
                    request,
                    owner,
                    turn,
                    job,
                    Ok(xylo_runtime::host::CommandOutputPage {
                        offset: 0,
                        next: None,
                        text: "Complete log".into(),
                    }),
                    cx,
                );
                assert!(view.output_panel(cx).is_none());
                assert!(view.output_view.open.as_ref().unwrap().request.is_none());
                view.session = owner;
                assert!(view.output_panel(cx).is_some());
                assert_eq!(
                    view.output_view.open.as_ref().unwrap().raw_page,
                    "Complete log"
                );
            })
        })
        .unwrap();
    }
}
