//! The live turn, the way Codex reads: while it runs, its text and tool calls
//! stream in order and one activity line at the bottom says what is happening
//! (phase, latest step, elapsed time, Esc to stop). When it finishes, the work
//! folds under a single "Worked for 1m 12s" line and only the answer stays
//! full size, followed by its action row. Activity and answer share one left
//! edge; there is no avatar gutter.
//!
//! Rows are the summary line, one row per [`Segment`], then the footer, so a
//! row's height changes only when its own segment (or the fold) changes. A row
//! with nothing to show renders empty instead of disappearing, which keeps row
//! indices stable for the virtual list. Finished text segments keep their
//! shared string, which lets the markdown view skip re-parsing them; only the
//! streaming tail grows.
use super::tools::{CallRun, merge_identical};
use super::*;
use crate::icons::{self, AppIcon};
use crate::motion::{self, Disclosure};
use crate::state::tool_presentation::one_line;
use crate::state::{PLAN_TOOL, Segment, SubagentState, TurnFailure};

/// How often the activity line's elapsed time advances.
const TURN_CLOCK_TICK: std::time::Duration = std::time::Duration::from_secs(1);

/// The running turn's once-a-second redraw; dropping it stops the timer.
struct TurnClock {
    _timer: Task<()>,
}

/// Calls that never fold into a summary row: they carry their own UI.
fn always_visible(tool: &ToolCard) -> bool {
    matches!(tool.name.as_str(), PLAN_TOOL | "image_generate")
}

/// What a group of finished calls did, in product words and first-seen order:
/// "Read 3 files · Searched 2 patterns · Ran 1 command".
pub(super) fn group_summary<'a>(tools: impl IntoIterator<Item = &'a ToolCard>) -> String {
    let mut counts: Vec<((&str, &str, &str), usize)> = Vec::new();
    for tool in tools {
        let category = tool_category(&tool.name);
        match counts.iter_mut().find(|(known, _)| *known == category) {
            Some((_, count)) => *count += 1,
            None => counts.push((category, 1)),
        }
    }
    counts
        .into_iter()
        .map(|((verb, one, many), count)| {
            format!("{verb} {count} {}", if count == 1 { one } else { many })
        })
        .collect::<Vec<_>>()
        .join(" · ")
}
fn tool_category(name: &str) -> (&'static str, &'static str, &'static str) {
    match name {
        "file_read" | "read_file" => ("Read", "file", "files"),
        "extension_resource" => ("Read", "skill file", "skill files"),
        "search_project" | "find_files" | "grep" | "glob" | "find" => {
            ("Searched", "pattern", "patterns")
        }
        "files_list" => ("Listed", "folder", "folders"),
        "run_command" | "shell_command" | "terminal" => ("Ran", "command", "commands"),
        "file_write" | "file_edit" | "write_file" | "edit_file" => ("Edited", "file", "files"),
        "web_fetch" | "web_search" | "browser" => ("Fetched", "page", "pages"),
        "memory_search" | "memory_remember" => ("Used memory", "time", "times"),
        "extension_skill" => ("Used", "skill", "skills"),
        name if name.starts_with("git_") => ("Checked git", "time", "times"),
        _ => ("Used", "tool", "tools"),
    }
}

/// "12s" below a minute, "1m 04s" above.
pub(super) fn duration_label(ms: i64) -> String {
    let seconds = (ms.max(0) / 1000) as u64;
    if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

/// The live activity of a running turn, in plain words.
pub(super) fn phase_label(turn: &TurnState) -> String {
    if turn.pending_approvals().next().is_some() {
        return "Waiting for your approval".into();
    }
    match turn.phase {
        Some(TurnPhase::Preparing) => "Preparing…".into(),
        Some(TurnPhase::Thinking) => "Thinking…".into(),
        Some(TurnPhase::Responding) => "Responding…".into(),
        Some(TurnPhase::RunningTools) => "Running tools…".into(),
        Some(TurnPhase::WaitingForApproval) => "Waiting for approval".into(),
        Some(TurnPhase::Compacting) => "Condensing earlier context…".into(),
        Some(TurnPhase::Delegating) => "Helper agent working…".into(),
        Some(TurnPhase::Retrying { attempt, delay_ms }) => {
            let waited = (now_ms() - turn.phase_since).max(0) as u64;
            let seconds = delay_ms.saturating_sub(waited).div_ceil(1000);
            format!("Retrying in {seconds} s (attempt {attempt})")
        }
        None => turn
            .status
            .clone()
            .filter(|status| !status.trim().is_empty())
            .unwrap_or_else(|| "Working…".into()),
    }
}

/// A helper agent's call in flight, as its row reads ("Read src/main.rs"),
/// for the activity line. A top-level call already shows its own running row
/// just above the line, so it is not repeated; nothing once every call has
/// reported back.
pub(super) fn latest_step(turn: &TurnState) -> Option<String> {
    let tool = turn.tools.last()?;
    if tool.parent_id.is_none() || tool.approval || tool.success.is_some() {
        return None;
    }
    let shown = &tool.presentation;
    Some(one_line(&format!("{} {}", shown.verb, shown.target)))
}

/// The segment that is the turn's answer: the text it ended with. A turn that
/// is still running has no answer yet.
pub(super) fn answer_segment(turn: &TurnState) -> Option<usize> {
    if turn.running || turn.outcome.is_none() {
        return None;
    }
    match turn.segments.last() {
        Some(Segment::Text(text)) if !text.trim().is_empty() => Some(turn.segments.len() - 1),
        _ => None,
    }
}

/// Whether a finished turn did anything besides answering, so it has work to
/// summarise.
pub(super) fn has_work(turn: &TurnState) -> bool {
    turn.segments.len() > usize::from(answer_segment(turn).is_some())
}

/// Headline for a failed turn, chosen from the engine's classification.
pub(super) fn failure_title(failure: &TurnFailure) -> &'static str {
    match failure.kind {
        ErrorKind::Authentication => "The provider did not accept the saved credentials",
        ErrorKind::RateLimited => "The provider is limiting requests right now",
        ErrorKind::Provider => "The model provider could not be reached",
        ErrorKind::ContextOverflow => "This task no longer fits the model's context window",
        ErrorKind::Internal => "The task stopped with an error",
    }
}

impl Xylo {
    /// One transcript row of the current chat's turn: the work summary, a
    /// segment, or the trailing activity/outcome row.
    ///
    /// A segment fades and rises in when it first appears; the footer only
    /// fades, under its own identity, so the activity line does not replay
    /// its entrance each time a segment pushes it down a row.
    pub(super) fn live_row(
        &self,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let turn = self.turns.get(&self.session);
        let segments = turn.map_or(0, |turn| turn.segments.len());
        if offset == 0 {
            return turn
                .map(|turn| self.work_summary(turn, window, cx))
                .unwrap_or_else(|| div().into_any_element());
        }
        let index = offset - 1;
        let Some(turn) = turn.filter(|_| index < segments) else {
            let footer = self.turn_footer(turn, offset, window, cx);
            return motion::fade_in(
                ElementId::Name(format!("live-footer-{}", self.session).into()),
                div().w_full().child(footer),
            )
            .into_any_element();
        };
        let answer = answer_segment(turn);
        let folded = self.work_folded(turn);
        if folded && Some(index) != answer {
            return self.folded_segment(turn, index, cx);
        }
        let row = match &turn.segments[index] {
            Segment::Text(text) => text_segment(
                self.session,
                index,
                text.clone(),
                turn.running || Some(index) == answer,
            ),
            Segment::Tools(ids) => self.tool_group_row(turn, ids, window, cx),
            Segment::Subagent(id) => self.subagent_row(turn, *id, window, cx),
        };
        motion::fade_slide_in(
            ElementId::NamedInteger(format!("live-row-{}", self.session).into(), index as u64),
            div().w_full().child(row),
            6.,
        )
        .into_any_element()
    }

    /// Expansion key of a turn's work summary; a new turn starts folded.
    fn work_key(&self, turn: &TurnState) -> Uuid {
        let started = turn.started_at.unwrap_or_default();
        Uuid::new_v5(&self.session, format!("worked-{started}").as_bytes())
    }

    /// A finished turn shows only its answer until the reader opens its work.
    pub(super) fn work_folded(&self, turn: &TurnState) -> bool {
        !turn.running
            && turn.outcome.is_some()
            && has_work(turn)
            && !self.expanded_tools.contains(&self.work_key(turn))
    }

    /// "Worked for 1m 12s ›" above a finished turn; nothing while it runs or
    /// when the turn only answered.
    fn work_summary(
        &self,
        turn: &TurnState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if turn.running || turn.outcome.is_none() || !has_work(turn) {
            return div().into_any_element();
        }
        let key = self.work_key(turn);
        let open = self.expanded_tools.contains(&key);
        let disclosure = Disclosure::new(ElementId::Uuid(key), open, window, cx);
        let label = format!("Worked for {}", duration_label(turn.elapsed_ms()));
        div()
            .w_full()
            .pt(px(6.))
            .pb(px(8.))
            .flex()
            .items_center()
            .gap(px(10.))
            .child(
                focus_ring(gpui_kit::base::Button::new("turn-summary"))
                    .accessibility_label(format!(
                        "{label}. {}",
                        if open { "Hide steps" } else { "Show steps" }
                    ))
                    .ml(px(-6.))
                    .px(px(6.))
                    .h(px(26.))
                    .rounded(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .cursor_pointer()
                    .text_size(px(13.))
                    .text_color(rgb(muted()))
                    .hover(|style| style.text_color(rgb(text_secondary())))
                    .child(div().single_line().child(label))
                    .child(disclosure.chevron())
                    .on_click(cx.listener(move |view, _, _, cx| view.toggle_expanded(key, cx))),
            )
            .child(div().flex_1().h(px(1.)).bg(rgb(border())))
            .into_any_element()
    }

    /// A folded work segment: only generated images stay, since they are
    /// results rather than steps.
    fn folded_segment(&self, turn: &TurnState, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(Segment::Tools(ids)) = turn.segments.get(index) else {
            return div().into_any_element();
        };
        let images: Vec<AnyElement> = ids
            .iter()
            .filter_map(|id| turn.tool(*id))
            .filter(|tool| tool.name == "image_generate" && tool.success == Some(true))
            .map(|tool| self.generated_tool_image_card(tool, cx))
            .collect();
        if images.is_empty() {
            return div().into_any_element();
        }
        div()
            .w_full()
            .py(px(4.))
            .flex()
            .flex_col()
            .gap_2()
            .children(images)
            .into_any_element()
    }

    /// Consecutive finished calls collapse into one summary row; running,
    /// failed and waiting calls stay visible on their own.
    fn tool_group_row(
        &self,
        turn: &TurnState,
        ids: &[Uuid],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut column = div().w_full().flex().flex_col().gap(px(2.));
        let mut run: Vec<usize> = Vec::new();
        for id in ids {
            let Some(index) = turn.tools.iter().position(|tool| tool.id == *id) else {
                continue;
            };
            let tool = &turn.tools[index];
            if tool.name == PLAN_TOOL && turn.plan != Some(tool.id) && tool.success != Some(false) {
                // Earlier checklists fold into the newest one.
                continue;
            }
            if tool.is_done() && !always_visible(tool) {
                run.push(index);
                continue;
            }
            column = column.children(self.finished_run(turn, &mut run, window, cx));
            column = column.child(self.tool_row(index, tool, window, cx));
        }
        column = column.children(self.finished_run(turn, &mut run, window, cx));
        div().w_full().py(px(4.)).child(column).into_any_element()
    }

    /// Renders and empties a run of finished calls: one row when the calls
    /// were identical, otherwise a summary row that opens to them.
    fn finished_run(
        &self,
        turn: &TurnState,
        run: &mut Vec<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let indices = std::mem::take(run);
        let merged = merge_identical(&turn.tools, &indices);
        match merged.as_slice() {
            [] => None,
            [single] => Some(self.call_run_row(turn, *single, window, cx)),
            _ => Some(self.collapsed_group(turn, &indices, &merged, window, cx)),
        }
    }

    /// A run of finished calls behind one summary line, given by their card
    /// indices and the rows they merge into.
    fn collapsed_group(
        &self,
        turn: &TurnState,
        indices: &[usize],
        rows: &[CallRun],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let first = indices[0];
        let key = group_key(turn.tools[first].id);
        let expanded = self.expanded_tools.contains(&key);
        let disclosure = Disclosure::new(ElementId::Uuid(key), expanded, window, cx);
        let summary = group_summary(indices.iter().map(|index| &turn.tools[*index]));
        let header = focus_ring(gpui_kit::base::Button::new(("tool-group", first)))
            .accessibility_label(format!(
                "{summary}. {}",
                if expanded { "Hide calls" } else { "Show calls" }
            ))
            .mx(px(-8.))
            .min_w_0()
            .flex()
            .items_center()
            .justify_start()
            .gap(px(4.))
            .px(px(8.))
            .h(px(30.))
            .rounded(px(8.))
            .cursor_pointer()
            .text_color(rgb(muted()))
            .hover(|style| style.bg(rgb(hover())).text_color(rgb(text_secondary())))
            .child(
                div()
                    .min_w_0()
                    .single_line()
                    .text_size(px(13.))
                    .child(summary),
            )
            .child(
                disclosure
                    .chevron()
                    .flex_shrink_0()
                    .text_color(rgb(icons::rest_color())),
            )
            .on_click(cx.listener(move |view, _, _, cx| view.toggle_expanded(key, cx)));
        let mut group = div().w_full().flex().flex_col().child(header);
        if disclosure.is_visible() {
            let mut calls = div()
                .ml(px(6.))
                .pl(px(12.))
                .border_l_1()
                .border_color(rgb(border()))
                .flex()
                .flex_col();
            for run in rows {
                calls = calls.child(self.call_run_row(turn, *run, window, cx));
            }
            group = group.child(disclosure.reveal(calls, window, cx));
        }
        group.into_any_element()
    }

    /// "Agent · task · 12 tool calls · 1m 04s"; its calls fold underneath and
    /// its final answer follows as markdown.
    fn subagent_row(
        &self,
        turn: &TurnState,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (Some(agent), Some(card_index)) = (
            turn.subagent(id),
            turn.tools.iter().position(|tool| tool.id == id),
        ) else {
            return div().into_any_element();
        };
        let card = &turn.tools[card_index];
        let key = subagent_key(id);
        let expanded = self.expanded_tools.contains(&key);
        let disclosure = Disclosure::new(ElementId::Uuid(key), expanded, window, cx);
        let calls = agent.tools.len();
        let meta = format!(
            "{calls} tool call{} · {}",
            if calls == 1 { "" } else { "s" },
            duration_label(agent.elapsed_ms())
        );
        let header = focus_ring(gpui_kit::base::Button::new(("subagent", card_index)))
            .accessibility_label(format!(
                "Helper agent: {}. {meta}. {}",
                agent.task,
                if expanded { "Hide calls" } else { "Show calls" }
            ))
            .mx(px(-8.))
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
                icons::small(AppIcon::Agent)
                    .flex_shrink_0()
                    .text_color(rgb(icons::rest_color())),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .single_line()
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(text_secondary()))
                    .child(if card.name == crate::state::DELEGATE_TOOL {
                        "Agent"
                    } else {
                        "Skill"
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .single_line()
                    .text_size(px(13.))
                    .text_color(rgb(muted()))
                    .child(one_line(&agent.task)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .single_line()
                    .text_size(px(11.5))
                    .text_color(rgb(faint()))
                    .child(meta),
            )
            .children(
                subagent_state_icon(agent.state).map(|mark| div().flex_shrink_0().child(mark)),
            )
            .child(
                disclosure
                    .chevron()
                    .flex_shrink_0()
                    .text_color(rgb(icons::rest_color())),
            )
            .on_click(cx.listener(move |view, _, _, cx| view.toggle_expanded(key, cx)));
        let mut row = div()
            .id(SharedString::from(format!("subagent-row-{id}")))
            .test_support()
            .w_full()
            .py(px(4.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(header);
        if card.approval {
            row = row.child(self.tool_row(card_index, card, window, cx));
        }
        let mut children = div()
            .ml(px(6.))
            .pl(px(12.))
            .border_l_1()
            .border_color(rgb(border()))
            .flex()
            .flex_col();
        let mut shown = 0;
        let mut live_shown = false;
        for child in &agent.tools {
            let Some(index) = turn.tools.iter().position(|tool| tool.id == *child) else {
                continue;
            };
            let tool = &turn.tools[index];
            // Collapsed, only decisions and the call in flight stay visible.
            let live =
                tool.approval || (agent.state == SubagentState::Running && tool.success.is_none());
            if disclosure.is_visible() || live {
                children = children.child(self.tool_row(index, tool, window, cx));
                shown += 1;
                live_shown |= live;
            }
        }
        if shown > 0 {
            // Calls that stay visible while collapsed must not shrink away with
            // the reveal, so a subagent with live calls opens without it.
            row = row.child(if live_shown {
                children.into_any_element()
            } else {
                disclosure.reveal(children, window, cx)
            });
        }
        if let Some(summary) = agent.summary.clone() {
            let failed = agent.state == SubagentState::Failed;
            row = row.child(
                div()
                    .ml(px(6.))
                    .pl(px(12.))
                    .border_l_1()
                    .border_color(rgb(if failed { danger() } else { border() }))
                    .text_size(px(14.))
                    .line_height(relative(1.6))
                    .text_color(rgb(if failed { danger() } else { text_secondary() }))
                    .child(
                        TextView::markdown(
                            SharedString::from(format!("subagent-summary-{id}")),
                            summary,
                        )
                        .selectable(true),
                    ),
            );
        }
        row.into_any_element()
    }

    /// The last row of a turn: the live activity line, how it ended, or the
    /// answer's action row. `row` is its list index.
    fn turn_footer(
        &self,
        turn: Option<&TurnState>,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match turn {
            Some(turn) if turn.running => {
                self.keep_turn_clock(window, cx);
                activity_line(
                    phase_label(turn),
                    latest_step(turn),
                    turn.elapsed_ms(),
                    turn.pending_approvals().next().is_some(),
                )
            }
            _ if self.sending_turn() => self.waiting_line("Sending…", window, cx),
            Some(turn) if turn.resend => self.waiting_line("Retrying…", window, cx),
            Some(turn) if turn.stopped() => stopped_marker(),
            Some(turn) => match turn.failure() {
                Some(failure) => self.failure_row(failure, turn.request.is_some(), cx),
                None => self.live_answer_actions(turn, 1 + self.messages.len() + row, cx),
            },
            None => div().into_any_element(),
        }
    }

    /// Re-renders the chat once a second while its turn runs, so the elapsed
    /// time and a retry countdown advance even when nothing animates (an
    /// approval is pending, or motion is reduced). The timer lives in the
    /// activity line's element state: it stops when the line leaves the
    /// screen and ends itself when the turn does.
    fn keep_turn_clock(&self, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session;
        let view = cx.entity().downgrade();
        window.use_keyed_state(
            ElementId::Name(format!("turn-clock-{session}").into()),
            cx,
            move |_, cx| {
                let timer = cx.spawn(async move |_, cx| {
                    loop {
                        cx.background_executor().timer(TURN_CLOCK_TICK).await;
                        let running = view
                            .update(cx, |view, cx| {
                                let running =
                                    view.turns.get(&session).is_some_and(|turn| turn.running);
                                if running && view.session == session {
                                    cx.notify();
                                }
                                running
                            })
                            .unwrap_or(false);
                        if !running {
                            break;
                        }
                    }
                });
                TurnClock { _timer: timer }
            },
        );
    }

    /// The action row under the answer of a turn that just completed.
    fn live_answer_actions(
        &self,
        turn: &TurnState,
        index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(answer) = answer_segment(turn) else {
            return div().into_any_element();
        };
        let Some(Segment::Text(text)) = turn.segments.get(answer) else {
            return div().into_any_element();
        };
        div()
            .w_full()
            .pt(px(2.))
            .pb(px(6.))
            .child(self.answer_actions(
                index,
                text.to_string(),
                turn.finished_at.unwrap_or_else(now_ms),
                None,
                true,
                cx,
            ))
            .into_any_element()
    }

    /// The activity line for a wait the host has not acknowledged yet, with
    /// the slow-system hint once it runs past a few seconds.
    fn waiting_line(&self, label: &str, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let id = format!("turn-waiting-{}", self.session);
        let since = motion::shown_since(ElementId::Name(id.clone().into()), window, cx);
        let elapsed = since.elapsed().as_millis() as i64;
        div()
            .w_full()
            .flex()
            .flex_col()
            .child(activity_line(label.to_owned(), None, elapsed, false))
            .children(
                motion::slow_hint(id, since, window, cx).map(|hint| div().pl(px(24.)).child(hint)),
            )
            .into_any_element()
    }

    /// A calm inline error: what happened, the provider's words, and the
    /// actions its classification allows.
    fn failure_row(
        &self,
        failure: &TurnFailure,
        resumable: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let details = failure.message.clone();
        let mut actions = div().flex().flex_wrap().items_center().gap_1();
        if failure.retryable && resumable {
            actions = actions.child(
                secondary("retry-turn", "Retry")
                    .min_h(px(28.))
                    .h(px(28.))
                    .text_size(px(12.5))
                    .disabled(self.running() || self.sending_turn())
                    .on_click(cx.listener(|view, _, _, cx| view.retry_last_message(cx))),
            );
        }
        if failure.kind == ErrorKind::Authentication {
            actions = actions.child(
                super::tools::detail_action("open-providers", "Open providers").on_click(
                    cx.listener(|view, _, _, cx| {
                        view.settings_open = true;
                        view.navigate(Page::Providers, cx);
                    }),
                ),
            );
        }
        actions = actions.child(
            super::tools::detail_action("copy-turn-error", "Copy details").on_click(
                move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(details.clone())),
            ),
        );
        div()
            .id("turn-error")
            .test_support()
            .w_full()
            .py(px(8.))
            .flex()
            .items_start()
            .gap(px(10.))
            .child(
                div()
                    .flex_shrink_0()
                    .pt(px(2.))
                    .child(icons::status(AppIcon::Error)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(text()))
                            .child(failure_title(failure)),
                    )
                    .child(
                        // The provider's own words, in full, so the cause is visible.
                        div()
                            .id("turn-error-detail")
                            .test_support()
                            .w_full()
                            .min_w_0()
                            .text_size(px(13.))
                            .line_height(relative(1.5))
                            .text_color(rgb(muted()))
                            .child(
                                TextView::markdown(
                                    "turn-error-message",
                                    literal_markdown(&failure.message),
                                )
                                .selectable(true),
                            ),
                    )
                    .child(div().ml(px(-8.)).child(actions)),
            )
            .into_any_element()
    }

    /// Helpers of the current chat, newest first, for the work panel.
    pub(in crate::xylo) fn subagents_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(turn) = self
            .turns
            .get(&self.session)
            .filter(|turn| !turn.subagents.is_empty() || !turn.earlier_subagents.is_empty())
        else {
            return div()
                .p_4()
                .child(subtitle(
                    "Helper agents this chat starts appear here with their progress.",
                ))
                .into_any_element();
        };
        let mut list = div()
            .id("subagents-panel")
            .test_support()
            .size_full()
            .overflow_y_scroll()
            .p_2()
            .flex()
            .flex_col()
            .gap_1();
        let live = turn.subagents.iter().rev().map(|agent| (agent, true));
        let earlier = turn
            .earlier_subagents
            .iter()
            .rev()
            .map(|agent| (agent, false));
        for (index, (agent, current)) in live.chain(earlier).enumerate() {
            let id = agent.id;
            let calls = agent.tools.len();
            let entry = focus_ring(gpui_kit::base::Button::new(("subagent-entry", index)))
                .accessibility_label(format!("Helper agent: {}", agent.task))
                .w_full()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py(px(6.))
                .rounded(px(8.))
                .when(current, |entry| {
                    entry
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(hover())))
                        .on_click(cx.listener(move |view, _, _, cx| view.reveal_subagent(id, cx)))
                })
                .child(
                    div()
                        .flex_shrink_0()
                        .w(icons::SMALL)
                        .children(subagent_state_icon(agent.state)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .single_line()
                                .text_size(px(12.5))
                                .text_color(rgb(text()))
                                .child(if agent.task.trim().is_empty() {
                                    "Helper agent".to_owned()
                                } else {
                                    one_line(&agent.task)
                                }),
                        )
                        .child(
                            div()
                                .single_line()
                                .text_size(px(11.))
                                .text_color(rgb(muted()))
                                .child(format!(
                                    "{} · {calls} tool call{} · {}",
                                    subagent_state_word(agent.state),
                                    if calls == 1 { "" } else { "s" },
                                    duration_label(agent.elapsed_ms())
                                )),
                        ),
                );
            list = list.child(entry);
        }
        list.into_any_element()
    }

    /// Scrolls the transcript to a helper's row and opens its calls (and the
    /// turn's folded work, so the row is there to reveal).
    fn reveal_subagent(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some((position, work)) = self.turns.get(&self.session).and_then(|turn| {
            turn.segments
                .iter()
                .position(|segment| *segment == Segment::Subagent(id))
                .map(|position| (position, self.work_key(turn)))
        }) else {
            return;
        };
        self.expanded_tools.insert(subagent_key(id));
        self.expanded_tools.insert(work);
        self.invalidate_live_history();
        self.sync_history_list(cx);
        // The history header, the messages, then the turn's summary row.
        let row = 2 + self.messages.len() + position;
        if let Some(history) = self.histories.get(&self.session) {
            history.list.pause_following_tail();
            history.list.scroll_to_reveal_item(row);
        }
        cx.notify();
    }

    pub(super) fn toggle_expanded(&mut self, key: Uuid, cx: &mut Context<Self>) {
        if !self.expanded_tools.remove(&key) {
            self.expanded_tools.insert(key);
        }
        self.invalidate_live_history();
        self.sync_history_list(cx);
        cx.notify();
    }
}

/// Plain text as markdown that renders literally: punctuation is escaped and
/// line breaks are kept.
fn literal_markdown(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len() + text.len() / 8);
    for (index, line) in text.lines().enumerate() {
        if index > 0 {
            escaped.push_str("  \n");
        }
        for ch in line.chars() {
            if ch.is_ascii_punctuation() {
                escaped.push('\\');
            }
            escaped.push(ch);
        }
    }
    escaped
}

/// Expansion keys derived from a card id never collide with the card's own.
fn group_key(first: Uuid) -> Uuid {
    Uuid::new_v5(&first, b"tool-group")
}
fn subagent_key(id: Uuid) -> Uuid {
    Uuid::new_v5(&id, b"subagent-calls")
}

fn subagent_state_word(state: SubagentState) -> &'static str {
    match state {
        SubagentState::Running => "Working",
        SubagentState::Completed => "Done",
        SubagentState::Failed => "Failed",
        SubagentState::NotCompleted => "Not completed",
    }
}
/// A helper's status mark; a helper that finished cleanly needs none.
fn subagent_state_icon(state: SubagentState) -> Option<AnyElement> {
    match state {
        SubagentState::Running => Some(
            motion::spinner(icons::SMALL)
                .color(rgb(muted()).into())
                .into_any_element(),
        ),
        SubagentState::Completed => None,
        SubagentState::Failed => Some(icons::status(AppIcon::Error).into_any_element()),
        SubagentState::NotCompleted => Some(
            icons::small(AppIcon::Pending)
                .text_color(rgb(icons::rest_color()))
                .into_any_element(),
        ),
    }
}

/// Streamed assistant text. The answer, and everything while the turn runs,
/// reads at full size; steps of a finished turn read quieter.
fn text_segment(session: Uuid, index: usize, body: SharedString, prominent: bool) -> AnyElement {
    div()
        .w_full()
        .min_w_0()
        .py(px(4.))
        .text_size(px(if prominent { 15. } else { 14. }))
        .line_height(relative(1.7))
        .text_color(rgb(if prominent { text() } else { text_secondary() }))
        .child(
            TextView::markdown(
                SharedString::from(format!("turn-text-{session}-{index}")),
                body,
            )
            .selectable(true),
        )
        .into_any_element()
}

/// Spinner, what is happening, the call in flight, for how long, and how to
/// stop it. The label cross-fades when the phase changes, but not when only a
/// countdown in it ticks, so "Retrying in 3 s" does not flash every second.
fn activity_line(
    label: String,
    step: Option<String>,
    elapsed_ms: i64,
    approval: bool,
) -> AnyElement {
    let phase: String = label.chars().filter(|ch| !ch.is_ascii_digit()).collect();
    div()
        .id("turn-activity")
        .test_support()
        .w_full()
        .h(px(32.))
        .flex()
        .items_center()
        .gap(px(10.))
        .child(
            div()
                .flex_shrink_0()
                .flex()
                .items_center()
                .child(if approval {
                    icons::status(AppIcon::Warning).into_any_element()
                } else {
                    motion::spinner(icons::SMALL)
                        .color(rgb(accent_text()).into())
                        .into_any_element()
                }),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(13.5))
                .child(motion::fade_on_change(
                    "turn-activity-label",
                    &phase,
                    div()
                        .single_line()
                        .text_color(rgb(text_secondary()))
                        .child(label),
                )),
        )
        .child(
            div()
                .id("turn-activity-step")
                .test_support()
                .flex_1()
                .min_w_0()
                .single_line()
                .text_size(px(12.5))
                .text_color(rgb(muted()))
                .children(step),
        )
        .child(
            div()
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(12.))
                .text_color(rgb(faint()))
                .child(duration_label(elapsed_ms))
                .child("·")
                .child(kbd("Esc"))
                .child(if approval { "to deny" } else { "to stop" }),
        )
        .into_any_element()
}

/// The quiet marker a stopped turn leaves under the text it kept.
fn stopped_marker() -> AnyElement {
    div()
        .id("turn-stopped")
        .test_support()
        .w_full()
        .py(px(6.))
        .flex()
        .items_center()
        .gap(px(6.))
        .text_size(px(12.5))
        .text_color(rgb(muted()))
        .child(icons::small(AppIcon::Stop))
        .child("You stopped this task")
        .into_any_element()
}

#[cfg(test)]
#[path = "transcript/tests.rs"]
mod tests;
