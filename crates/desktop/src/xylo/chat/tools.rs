//! One tool call as one quiet line: icon, verb, target and a short fact on the
//! right ("1–162 · 162 lines", "exit 0"). A status mark appears only while the
//! call runs, waits for a decision or failed; a finished call needs none.
//! Consecutive identical calls (the same file read twice) share one row with a
//! count. Clicking a row opens its detail: the readable result and the copy
//! actions. A call waiting for a decision opens the approval card instead.
use super::*;
use crate::icons::{self, AppIcon};
use crate::state::{ToolCard, TurnState};
use serde_json::Value;

/// The icon for a kind of call, so a run of calls scans at a glance.
pub(super) fn tool_icon(name: &str) -> AppIcon {
    match name {
        "file_read" | "read_file" | "extension_resource" => AppIcon::ToolRead,
        "file_write" | "file_edit" | "write_file" | "edit_file" => AppIcon::ToolEdit,
        "run_command" | "shell_command" | "terminal" => AppIcon::Terminal,
        "search_project" | "grep" | "glob" | "find" => AppIcon::Search,
        "files_list" | "find_files" => AppIcon::Folder,
        "git_status" | "git_diff" | "git_log" | "git_commit" => AppIcon::Branch,
        "web_fetch" | "web_search" | "browser" => AppIcon::Web,
        crate::state::PLAN_TOOL => AppIcon::Plan,
        "image_generate" => AppIcon::Image,
        "memory_search" | "memory_remember" => AppIcon::Memory,
        "extension_skill" => AppIcon::Skills,
        crate::state::DELEGATE_TOOL => AppIcon::Agent,
        _ => AppIcon::Tool,
    }
}

/// Consecutive calls that read as one row: card indices of the first and the
/// latest call, and how many there were.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CallRun {
    pub first: usize,
    pub last: usize,
    pub count: usize,
}

/// Folds consecutive finished calls that show the same row (same tool, same
/// target) into one run. Running, failed and waiting calls stay on their own,
/// so their status is never hidden behind a count.
pub(super) fn merge_identical(tools: &[ToolCard], indices: &[usize]) -> Vec<CallRun> {
    let mut runs: Vec<CallRun> = Vec::with_capacity(indices.len());
    for &index in indices {
        let tool = &tools[index];
        if let Some(run) = runs.last_mut() {
            let previous = &tools[run.last];
            if tool.is_done()
                && previous.is_done()
                && previous.name == tool.name
                && previous.presentation.target == tool.presentation.target
            {
                run.last = index;
                run.count += 1;
                continue;
            }
        }
        runs.push(CallRun {
            first: index,
            last: index,
            count: 1,
        });
    }
    runs
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

/// The status mark of a call and the word screen readers hear for it. A call
/// that finished cleanly has no mark.
fn call_status(tool: &ToolCard) -> (Option<AnyElement>, &'static str) {
    if tool.approval {
        return (
            Some(icons::status(AppIcon::Warning).into_any_element()),
            "needs approval",
        );
    }
    match tool.success {
        None if tool.not_completed => (
            Some(
                icons::small(AppIcon::Pending)
                    .text_color(rgb(icons::rest_color()))
                    .into_any_element(),
            ),
            "not completed",
        ),
        None => (
            Some(
                crate::motion::spinner(icons::SMALL)
                    .color(rgb(muted()).into())
                    .into_any_element(),
            ),
            "in progress",
        ),
        Some(true) => (None, "done"),
        Some(false) if tool.decision_lost => (
            Some(icons::status(AppIcon::Warning).into_any_element()),
            "decision not applied",
        ),
        Some(false) => (
            Some(icons::status(AppIcon::Error).into_any_element()),
            "failed",
        ),
    }
}

impl Xylo {
    /// One call on its own row.
    pub(super) fn tool_row(
        &self,
        i: usize,
        tool: &ToolCard,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.call_row(i, tool, tool, 1, window, cx)
    }

    /// A run of identical calls; the row keeps the first call's identity and
    /// shows the latest call's result.
    pub(super) fn call_run_row(
        &self,
        turn: &TurnState,
        run: CallRun,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.call_row(
            run.first,
            &turn.tools[run.first],
            &turn.tools[run.last],
            run.count,
            window,
            cx,
        )
    }

    fn call_row(
        &self,
        i: usize,
        first: &ToolCard,
        tool: &ToolCard,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if tool.name == crate::state::PLAN_TOOL && tool.success != Some(false) {
            return plan_card(i, tool);
        }
        let key = first.id;
        let shown = &tool.presentation;
        // Calls without a readable body (skills, skill files) never expand.
        let expandable = shown.expandable() && !tool.approval;
        let expanded = expandable && self.expanded_tools.contains(&key);
        let disclosure = expandable.then(|| {
            crate::motion::Disclosure::new(
                ElementId::Name(format!("tool-details-{key}").into()),
                expanded,
                window,
                cx,
            )
        });
        let details_visible = disclosure.as_ref().is_some_and(|d| d.is_visible());
        let (status, state_word) = call_status(tool);
        let group: SharedString = format!("tool-row-{key}").into();
        let header = focus_ring(gpui_kit::base::Button::new(("tool-detail", i)))
            .accessibility_label(format!(
                "{} {}{}, {state_word}.{}",
                shown.verb,
                shown.target,
                if count > 1 {
                    format!(", {count} times")
                } else {
                    String::new()
                },
                match (expandable, expanded) {
                    (false, _) => "",
                    (true, true) => " Hide details",
                    (true, false) => " Show details",
                }
            ))
            .group(group.clone())
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .justify_start()
            .gap(px(8.))
            .px(px(8.))
            .h(px(30.))
            .rounded(px(8.))
            .when_some(shown.tooltip.clone(), |header, tooltip| {
                header.tooltip(move |_, cx| cx.new(|_| XyloTooltip(tooltip.clone().into())).into())
            })
            .child(
                icons::small(tool_icon(&tool.name))
                    .flex_shrink_0()
                    .text_color(rgb(if tool.approval {
                        warning()
                    } else {
                        icons::rest_color()
                    })),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .single_line()
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(text_secondary()))
                    .child(shown.verb.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .single_line()
                    .text_size(px(12.5))
                    .font_family(mono(cx))
                    .text_color(rgb(muted()))
                    .child(shown.target.clone()),
            )
            .when_some(shown.meta.clone(), |row, meta| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .single_line()
                        .text_size(px(11.5))
                        .text_color(rgb(faint()))
                        .child(meta),
                )
            })
            .when(count > 1, |row| {
                row.child(
                    div()
                        .id(("tool-repeat", i))
                        .test_support()
                        .flex_shrink_0()
                        .px(px(6.))
                        .rounded_full()
                        .bg(rgb(hover()))
                        .single_line()
                        .text_size(px(11.))
                        .text_color(rgb(muted()))
                        .child(format!("×{count}")),
                )
            })
            .children(status.map(|status| div().flex_shrink_0().child(status)))
            .when_some(disclosure.clone(), |header, disclosure| {
                header
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(hover())))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(rgb(faint()))
                            .when(!expanded, |chevron| {
                                chevron
                                    .opacity(0.)
                                    .group_hover(group.clone(), |style| style.opacity(1.))
                            })
                            .child(disclosure.chevron()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_expanded(key, cx)))
            });
        let framed = tool.approval || details_visible;
        // The row bleeds 8 px past the column on both sides so its icon sits
        // on the column's edge while the hover fill still has room.
        let mut card = div()
            .mx(px(-8.))
            .flex()
            .flex_col()
            .rounded(px(10.))
            .border_1()
            .border_color(transparent_black())
            .when(framed, |card| {
                card.border_color(rgb(border())).bg(rgb(panel()))
            })
            .child(header);
        // A failed call stays one row with its reason underneath.
        if let Some(error) = &shown.error {
            card = card.child(
                div()
                    .id(("tool-error", i))
                    .test_support()
                    .pl(px(30.))
                    .pr_2()
                    .pb(px(6.))
                    .text_size(px(12.5))
                    .line_height(relative(1.45))
                    .text_color(rgb(danger()))
                    .child(error.clone()),
            );
        }
        if tool.name == "image_generate" && tool.success == Some(true) {
            card = card.child(
                div()
                    .px_2()
                    .pb_2()
                    .child(self.generated_tool_image_card(tool, cx)),
            );
        }
        if tool.approval {
            card = card
                .border_color(rgb(border_strong()))
                .shadow_sm()
                .child(self.approval_panel(i, tool, cx));
            // A decision the task is waiting on announces itself once.
            return crate::motion::emphasize_in(
                ElementId::Name(format!("approval-enter-{key}").into()),
                card,
                rgb(warning()).into(),
                rgb(border_strong()).into(),
            )
            .into_any_element();
        }
        if let Some(disclosure) = disclosure.filter(|_| details_visible) {
            let details = self.structured_tool_details(i, tool, cx);
            card = card.child(disclosure.reveal(details, window, cx));
        }
        card.into_any_element()
    }

    fn full_action_inspector(
        &self,
        i: usize,
        tool: &ToolCard,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = tool.id;
        let expanded = self.raw_tool_details.contains(&id);
        let full = tool.presentation.pretty_input.clone();
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
                    .gap_1()
                    .child(
                        detail_action(
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
                    .child(
                        detail_action(("approval-copy", i), "Copy full action").on_click(
                            move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy.to_string()))
                            },
                        ),
                    ),
            )
            .when(expanded, |body| {
                body.when_some(diff, |body, diff| {
                    let copy = diff.clone();
                    body.child(
                        detail_action(("approval-copy-diff", i), "Copy full diff").on_click(
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

    /// The decision card: what the agent wants to do, a preview of it, and
    /// Allow / Deny with their shortcuts.
    fn approval_panel(&self, i: usize, tool: &ToolCard, cx: &mut Context<Self>) -> AnyElement {
        let id = tool.id;
        let incomplete = tool.input.get("diff_truncated").and_then(Value::as_bool) == Some(true);
        let shortcut = if cfg!(target_os = "macos") {
            "⌘⇧↩ allow · Esc deny"
        } else {
            "Ctrl+Shift+Enter allow · Esc deny"
        };
        div()
            .px(px(14.))
            .pt(px(4.))
            .pb(px(14.))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_size(px(14.))
                    .line_height(relative(1.45))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(text()))
                    .child(tool.presentation.question.clone()),
            )
            .when_some(tool.reason.clone(), |panel, reason| {
                panel.child(subtitle(reason).id(("approval-reason", i)).test_support())
            })
            .child(approval_preview(i, tool))
            .child(self.full_action_inspector(i, tool, cx))
            .when(incomplete, |panel| {
                panel.child(subtitle(
                    "This change preview was shortened. Deny it and ask for a smaller change so you can review all of it.",
                ))
            })
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        primary(("allow", i), "Allow once")
                            .disabled(incomplete)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.decide_action(id, true, false, cx)
                            })),
                    )
                    .child(secondary(("deny", i), "Deny").on_click(cx.listener(
                        move |this, _, _, cx| this.decide_action(id, false, false, cx),
                    )))
                    .child(div().flex_1())
                    .child(subtitle(shortcut)),
            )
            .when_some(tool.rule.clone(), |panel, rule| {
                panel.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .pt_3()
                        .border_t_1()
                        .border_color(rgb(border()))
                        .child(div().flex_1().min_w_0().child(subtitle(format!(
                            "Stop asking for {rule} in this project. You can revoke it in Settings."
                        ))))
                        .child(
                            secondary(("allow-always", i), "Always allow")
                                .accessibility_label(format!("Always allow {rule}"))
                                .disabled(incomplete)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.decide_action(id, true, true, cx)
                                })),
                        ),
                )
            })
            .into_any_element()
    }
}

/// A compact text action inside a detail view or the approval card.
pub(super) fn detail_action(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui_kit::base::Button {
    button(id, label)
        .min_h(px(26.))
        .h(px(26.))
        .px(px(8.))
        .text_size(px(12.))
        .font_weight(FontWeight::NORMAL)
        .text_color(rgb(text_secondary()))
}

/// The agent's checklist. Earlier versions fold into the newest one.
fn plan_card(i: usize, tool: &ToolCard) -> AnyElement {
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
        .child(icons::small(AppIcon::Plan).text_color(rgb(icons::rest_color())))
        .child(
            div()
                .text_size(px(13.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(text()))
                .child("Plan"),
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
    let mut list = div().flex().flex_col().gap(px(6.));
    for (index, (content, status)) in items.into_iter().enumerate() {
        let (mark, weight) = match status.as_str() {
            "completed" => (
                icons::status(AppIcon::Success).into_any_element(),
                FontWeight::NORMAL,
            ),
            "in_progress" => (
                crate::motion::spinner(icons::SMALL)
                    .color(rgb(accent_text()).into())
                    .into_any_element(),
                FontWeight::MEDIUM,
            ),
            _ => (
                icons::small(AppIcon::Pending)
                    .text_color(rgb(icons::rest_color()))
                    .into_any_element(),
                FontWeight::NORMAL,
            ),
        };
        list = list.child(
            div()
                .id(("plan-item", i * 1000 + index))
                .flex()
                .items_start()
                .gap(px(10.))
                .text_size(px(13.5))
                .line_height(relative(1.5))
                .child(div().flex_shrink_0().pt(px(3.)).child(mark))
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
        .my_1()
        .flex()
        .flex_col()
        .gap(px(10.))
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
pub(super) fn diff_view(id: impl Into<ElementId>, diff: &str) -> AnyElement {
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
        .rounded(px(8.))
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
        _ => code_block(("approval-input", i), &tool.presentation.pretty_input),
    }
}

#[cfg(test)]
#[path = "tools/tests.rs"]
mod tests;
