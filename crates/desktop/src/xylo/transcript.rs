//! The live turn, the way Codex reads: while it runs, its text and tool calls
//! stream in order and one activity line at the bottom says what is happening
//! (phase, latest step, elapsed time, Esc to stop). When it finishes, the work
//! folds under a single "Activity · 1m 12s" line and only the answer stays
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
        "connector_resources" | "connector_resource_read" => {
            ("Read", "MCP resource", "MCP resources")
        }
        "connector_prompts" | "connector_prompt_get" => ("Loaded", "MCP prompt", "MCP prompts"),
        "ask_user" => ("Asked", "question", "questions"),
        "exit_plan_mode" => ("Reviewed", "plan", "plans"),
        "schedule_wakeup" => ("Scheduled", "follow-up", "follow-ups"),
        "lsp" => ("Inspected", "code location", "code locations"),
        "worktree_enter" | "worktree_exit" => ("Changed", "worktree", "worktrees"),
        "file_read" | "read_file" => ("Read", "file", "files"),
        "extension_resource" => ("Read", "skill file", "skill files"),
        "search_project" | "find_files" | "grep" | "glob" | "find" => {
            ("Searched", "pattern", "patterns")
        }
        "files_list" => ("Listed", "folder", "folders"),
        "run_command" | "shell_command" | "terminal" => ("Ran", "command", "commands"),
        "file_write" | "file_edit" | "notebook_edit" | "write_file" | "edit_file" => {
            ("Edited", "file", "files")
        }
        "shell_output" => ("Read", "command output", "command outputs"),
        "shell_kill" => ("Stopped", "shell job", "shell jobs"),
        "shell_jobs" => ("Listed", "shell job", "shell jobs"),
        "web_fetch" | "browser" => ("Fetched", "page", "pages"),
        "web_search" => ("Searched", "query", "queries"),
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
    if turn.cancel_requested {
        return "Stopping…".into();
    }
    if let Some(tool) = turn.pending_approvals().next() {
        return match tool.name.as_str() {
            "ask_user" => "Waiting for your answers",
            "exit_plan_mode" => "Waiting for your plan decision",
            _ => "Waiting for your approval",
        }
        .into();
    }
    match turn.phase {
        Some(TurnPhase::Preparing) => "Preparing…".into(),
        Some(TurnPhase::Thinking) => "Thinking…".into(),
        Some(TurnPhase::Responding) => "Responding…".into(),
        Some(TurnPhase::RunningTools) => turn
            .tools
            .iter()
            .rev()
            .find(|tool| tool.success.is_none() && !tool.not_completed)
            .map(|tool| match tool.name.as_str() {
                "run_command" | "shell_command" => "Running command…",
                "file_read" | "read_file" | "files_list" => "Reading files…",
                "search_project" | "find_files" | "web_search" => "Searching…",
                _ => "Working…",
            })
            .unwrap_or("Working…")
            .into(),
        Some(TurnPhase::WaitingForApproval) => "Waiting for approval".into(),
        Some(TurnPhase::Compacting) => "Summarizing earlier messages…".into(),
        Some(TurnPhase::Delegating) => "Working on a subtask…".into(),
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

/// The segment that is the turn's answer: the text it ended with. A turn that
/// is still running has no answer yet.
pub(super) fn answer_segment(turn: &TurnState) -> Option<usize> {
    if turn.running || turn.outcome.is_none() {
        return None;
    }
    turn.segments
        .iter()
        .rposition(|segment| matches!(segment, Segment::Text(text) if !text.trim().is_empty()))
}

/// Whether a finished turn did anything besides answering, so it has work to
/// summarise. Harness notes alone are not work.
pub(super) fn has_work(turn: &TurnState) -> bool {
    let answer = answer_segment(turn);
    turn.segments.iter().enumerate().any(|(index, segment)| {
        Some(index) != answer && !matches!(segment, Segment::Note { .. } | Segment::Thinking(_))
    })
}

/// A harness note in the transcript: quiet, one line per sentence, marked as
/// coming from Xylo rather than the model.
fn note_row(text: SharedString) -> AnyElement {
    div()
        .w_full()
        .flex()
        .items_start()
        .gap_2()
        .py_1()
        .text_size(px(13.))
        .text_color(rgb(text_secondary()))
        .child(div().pt(px(2.)).child(icons::status(AppIcon::Info)))
        .child(div().flex_1().min_w_0().whitespace_normal().child(text))
        .into_any_element()
}

/// Headline for a failed turn, chosen from the engine's classification.
pub(super) fn failure_title(failure: &TurnFailure) -> &'static str {
    match failure.kind {
        ErrorKind::Stalled => "The model kept repeating itself, so Xylo stopped the task",
        ErrorKind::EmptyResponse => "The model returned no usable reply",
        ErrorKind::Authentication => "The provider did not accept the saved credentials",
        ErrorKind::RateLimited => "The provider is limiting requests right now",
        ErrorKind::Provider => "The model request failed",
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
            return self.folded_segment(turn, index, window, cx);
        }
        // A finished turn's notes are steps: they live inside its work, and a
        // turn that only answered has no steps to show.
        if !turn.running
            && turn.outcome.is_some()
            && !has_work(turn)
            && matches!(turn.segments[index], Segment::Note { .. })
        {
            return div().into_any_element();
        }
        if !turn.running && !has_work(turn) && matches!(turn.segments[index], Segment::Thinking(_))
        {
            return div().into_any_element();
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
            Segment::Note { text, .. } => note_row(text.clone()),
            Segment::Thinking(_) => {
                let first = turn.segments.iter().position(
                    |segment| matches!(segment, Segment::Thinking(text) if !text.trim().is_empty()),
                );
                if first != Some(index) {
                    return div().into_any_element();
                }
                let text = turn
                    .segments
                    .iter()
                    .filter_map(|segment| match segment {
                        Segment::Thinking(text) if !text.trim().is_empty() => Some(text.as_ref()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                self.thinking_block(
                    Uuid::new_v5(
                        &self.session,
                        format!("thinking-{}-{index}", turn.started_at.unwrap_or_default())
                            .as_bytes(),
                    ),
                    text.into(),
                    window,
                    cx,
                )
            }
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
            && matches!(turn.outcome, Some(crate::state::TurnOutcome::Completed))
            && has_work(turn)
            && !self.expanded_tools.contains(&self.work_key(turn))
    }

    /// "Activity · 1m 12s ›" above a finished turn; nothing while it runs or
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
        let label = if turn.tools.is_empty() {
            "Reviewed the request".to_owned()
        } else {
            group_summary(turn.tools.iter())
        };
        div()
            .w_full()
            .mb(px(18.))
            .flex()
            .items_center()
            .child(
                focus_ring(gpui_kit::base::Button::new("turn-summary"))
                    .aria_expanded(open)
                    .accessibility_label(format!(
                        "{label}. {}",
                        if open {
                            "Hide activity"
                        } else {
                            "Show activity"
                        }
                    ))
                    .ml(px(-5.))
                    .px(px(5.))
                    .h(px(32.))
                    .rounded(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .cursor_pointer()
                    .text_size(px(13.))
                    .text_color(rgb(muted()))
                    .hover(|style| style.bg(rgb(panel())).text_color(rgb(text())))
                    .child(
                        icons::small(
                            if turn.outcome == Some(crate::state::TurnOutcome::Completed) {
                                AppIcon::Done
                            } else {
                                AppIcon::Info
                            },
                        )
                        .size(px(17.)),
                    )
                    .child(label)
                    .child(disclosure.chevron().size(px(13.)))
                    .on_click(cx.listener(move |view, _, _, cx| view.toggle_expanded(key, cx))),
            )
            .into_any_element()
    }

    /// A folded work segment: only generated images stay, since they are
    /// results rather than steps.
    fn folded_segment(
        &self,
        turn: &TurnState,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match turn.segments.get(index) {
            Some(Segment::Tools(ids)) => div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .children(
                    ids.iter()
                        .filter_map(|id| {
                            turn.tools
                                .iter()
                                .enumerate()
                                .find(|(_, tool)| tool.id == *id)
                        })
                        .filter(|(_, tool)| {
                            tool.success != Some(true)
                                || tool.name == "image_generate"
                                || self
                                    .tool_command_job(tool)
                                    .is_some_and(|job| job.running || job.exit_code != Some(0))
                        })
                        .map(|(i, tool)| self.tool_row(i, tool, window, cx)),
                )
                .into_any_element(),
            Some(Segment::Subagent(id))
                if turn
                    .subagent(*id)
                    .is_some_and(|agent| agent.state != SubagentState::Completed) =>
            {
                self.subagent_row(turn, *id, window, cx)
            }
            _ => div().into_any_element(),
        }
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
            if tool.is_done()
                && !always_visible(tool)
                && !self.tool_command_job(tool).is_some_and(|job| job.running)
            {
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
            [single] => Some(self.call_run_row(turn, single.clone(), window, cx)),
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
            .self_start()
            .max_w_full()
            .aria_expanded(expanded)
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
            .min_h(px(CONTROL_SIZE))
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
                .ml(px(7.))
                .pl(px(16.))
                .border_l_1()
                .border_color(rgb(border()))
                .flex()
                .flex_col();
            for run in rows {
                calls = calls.child(self.call_run_row(turn, run.clone(), window, cx));
            }
            group = group.child(disclosure.reveal(calls, window, cx));
        }
        group.into_any_element()
    }

    /// Task identity and compact progress, with calls underneath and a small
    /// action opening the result in the task panel.
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
        let (role, icon) = subagent_identity(&agent.task);
        let meta = format!(
            "{role} · {calls} tool{} · {}",
            if calls == 1 { "" } else { "s" },
            duration_label(agent.elapsed_ms())
        );
        let header = focus_ring(gpui_kit::base::Button::new(("subagent", card_index)))
            .aria_expanded(expanded)
            .accessibility_label(format!(
                "Subtask: {}. {}. {meta}. {}",
                agent.task,
                match agent.state {
                    SubagentState::Running => "Running",
                    SubagentState::Completed => "Completed",
                    SubagentState::Failed => "Failed",
                    SubagentState::NotCompleted => "Incomplete",
                },
                if expanded { "Hide calls" } else { "Show calls" }
            ))
            .mx(px(-8.))
            .min_w_0()
            .flex()
            .items_start()
            .justify_start()
            .gap(px(8.))
            .px(px(8.))
            .py(px(5.))
            .min_h(px(CONTROL_SIZE))
            .rounded(px(8.))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(hover())))
            .child(
                div()
                    .pt(px(2.))
                    .flex_shrink_0()
                    .child(icons::small(icon).text_color(rgb(icons::rest_color()))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .line_clamp(2)
                            .text_size(px(13.))
                            .line_height(relative(1.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(text_secondary()))
                            .child(one_line(&agent.task)),
                    )
                    .child(
                        div()
                            .single_line()
                            .text_size(px(11.))
                            .text_color(rgb(muted()))
                            .child(meta),
                    ),
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
        if agent
            .summary
            .as_ref()
            .is_some_and(|summary| !summary.trim().is_empty())
        {
            row = row.child(
                super::tools::detail_action(("subagent-open-result", card_index), "View result")
                    .self_start()
                    .ml(px(14.))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.expanded_tools
                            .insert(Uuid::new_v5(&id, b"panel-output"));
                        view.settings.ui.work_panel = "details".into();
                        view.settings.ui.work_panel_open = true;
                        cx.notify();
                    })),
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
                div().into_any_element()
            }
            _ if self.sending_turn() => div().into_any_element(),
            Some(turn) if turn.resend => self.waiting_line("Retrying…", window, cx),
            Some(turn) if turn.stopped() => div()
                .w_full()
                .my(px(18.))
                .flex()
                .items_center()
                .gap(px(8.))
                .child(stopped_marker())
                .child(
                    button("continue-stopped-task", "Continue")
                        .text_size(px(13.))
                        .on_click(cx.listener(|view, _, _, cx| {
                            if !view.running()
                                && !view.sending_turn()
                                && view
                                    .turns
                                    .get(&view.session)
                                    .is_some_and(|turn| turn.stopped())
                            {
                                view.submit_message_payload(
                                    super::super::drafts::MessagePayload {
                                        text:
                                            "Continue from the saved response and remaining work."
                                                .into(),
                                        attachments: vec![],
                                    },
                                    cx,
                                );
                            }
                        })),
                )
                .into_any_element(),
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
            .pt(px(12.))
            .pb(px(30.))
            .child(self.answer_actions(
                index,
                super::Answer {
                    id: super::live_answer_key(self.session, turn),
                    text: text.clone(),
                    note: turn.answer_note.clone(),
                    latest: true,
                },
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
                secondary("retry-turn", "Send again")
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
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2();
        for (label, agents, current) in [
            ("This turn", &turn.subagents, true),
            ("Earlier results", &turn.earlier_subagents, false),
        ] {
            if agents.is_empty() {
                continue;
            }
            list = list.child(
                div()
                    .px_2()
                    .pt_2()
                    .text_size(px(11.))
                    .text_color(rgb(muted()))
                    .child(format!("{label} · {}", agents.len())),
            );
            for agent in agents.iter().rev() {
                let id = agent.id;
                let (name, icon) = subagent_identity(&agent.task);
                let key = Uuid::new_v5(&id, b"panel-output");
                let expanded = self.expanded_tools.contains(&key);
                let mut card = div()
                    .id(SharedString::from(format!("subagent-result-{id}")))
                    .test_support()
                    .w_full()
                    .min_w_0()
                    .flex_shrink_0()
                    .p_3()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(rgb(border()))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(icons::small(icon).text_color(rgb(icons::rest_color())))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(rgb(text()))
                                    .child(name),
                            )
                            .children(subagent_state_icon(agent.state))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(muted()))
                                    .child(subagent_state_word(agent.state)),
                            ),
                    );
                if !agent.task.trim().is_empty() {
                    card = card.child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(muted()))
                            .line_clamp(3)
                            .child(one_line(&agent.task)),
                    );
                }
                if let Some(summary) = agent
                    .summary
                    .as_ref()
                    .filter(|value| !value.trim().is_empty())
                {
                    let long = summary.chars().count() > 1200 || summary.lines().count() > 18;
                    let output = if long && !expanded {
                        subagent_output_preview(summary)
                    } else {
                        summary.to_string()
                    };
                    card = card.child(
                        div()
                            .w_full()
                            .min_w_0()
                            .text_size(px(13.))
                            .line_height(relative(1.55))
                            .text_color(rgb(if agent.state == SubagentState::Failed {
                                danger()
                            } else {
                                text_secondary()
                            }))
                            .child(
                                TextView::markdown(
                                    SharedString::from(format!("subagent-panel-output-{id}")),
                                    output,
                                )
                                .selectable(true),
                            ),
                    );
                    if long {
                        card = card.child(
                            super::tools::detail_action(
                                SharedString::from(format!("subagent-output-toggle-{id}")),
                                if expanded {
                                    "Show less"
                                } else {
                                    "Show full output"
                                },
                            )
                            .on_click(cx.listener(
                                move |view, _, _, cx| {
                                    if !view.expanded_tools.remove(&key) {
                                        view.expanded_tools.insert(key);
                                    }
                                    cx.notify();
                                },
                            )),
                        );
                    }
                } else {
                    let message = match agent.state {
                        SubagentState::Running => "Working on this task…",
                        SubagentState::Completed => "Finished without a text result.",
                        SubagentState::Failed => "The helper failed without reporting a reason.",
                        SubagentState::NotCompleted => {
                            "The turn ended before this helper reported back."
                        }
                    };
                    if agent.state != SubagentState::Running {
                        card = card.child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(muted()))
                                .child(message),
                        );
                    }
                }
                let mut footer = div().flex().items_center().justify_between().gap_2().when(
                    agent.started_at > 0,
                    |footer| {
                        footer.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(faint()))
                                .child(duration_label(agent.elapsed_ms())),
                        )
                    },
                );
                if current && !agent.tools.is_empty() {
                    footer = footer.child(
                        super::tools::detail_action(
                            SharedString::from(format!("subagent-calls-link-{id}")),
                            format!("View {} tool calls", agent.tools.len()),
                        )
                        .on_click(cx.listener(move |view, _, _, cx| view.reveal_subagent(id, cx))),
                    );
                }
                list = list.child(card.when(
                    agent.started_at > 0 || (current && !agent.tools.is_empty()),
                    |card| card.child(footer),
                ));
            }
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

/// Task words choose useful roles without inventing a personal identity.
fn subagent_identity(task: &str) -> (&'static str, AppIcon) {
    let words: Vec<_> = task
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    let has = |needles: &[&str]| words.iter().any(|word| needles.contains(&word.as_str()));
    if has(&["review", "audit", "inspect"]) {
        ("Review", AppIcon::ToolRead)
    } else if has(&["test", "tests", "testing", "verify", "validation"]) {
        ("Verification", AppIcon::Plan)
    } else if has(&["research", "search", "investigate", "explore", "find"]) {
        ("Research", AppIcon::Search)
    } else if has(&["design", "layout", "visual", "ui", "ux"]) {
        ("Interface", AppIcon::Image)
    } else if has(&["fix", "implement", "build", "refactor", "edit", "update"]) {
        ("Implementation", AppIcon::ToolEdit)
    } else if has(&["document", "documentation", "docs", "write", "summarize"]) {
        ("Writing", AppIcon::Drafts)
    } else {
        ("Helper", AppIcon::Agent)
    }
}

fn subagent_output_preview(summary: &str) -> String {
    let mut preview = String::new();
    for line in summary.lines().take(18) {
        if !preview.is_empty() && preview.chars().count() + line.chars().count() > 1200 {
            break;
        }
        if !preview.is_empty() {
            preview.push('\n');
        }
        preview.extend(line.chars().take(1200));
    }
    preview.push_str("\n\n…");
    preview
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
        .line_height(relative(1.75))
        .text_color(rgb(if prominent { text() } else { text_secondary() }))
        .child(
            TextView::markdown(
                SharedString::from(format!("turn-text-{session}-{index}")),
                body,
            )
            .style(super::tokens::response_style())
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

#[cfg(all(test, not(feature = "validation")))]
#[path = "transcript/tests.rs"]
mod tests;

#[cfg(all(test, not(feature = "validation")))]
mod subagent_panel_tests {
    use super::{subagent_identity, subagent_output_preview};

    #[test]
    fn task_roles_match_words_and_keep_a_truthful_fallback() {
        assert_eq!(
            subagent_identity("Review authentication handling").0,
            "Review"
        );
        assert_eq!(subagent_identity("Implement sidebar layout").0, "Interface");
        assert_eq!(
            subagent_identity("Run tests for cancellation").0,
            "Verification"
        );
        assert_eq!(
            subagent_identity("Search provider documentation").0,
            "Research"
        );
        assert_eq!(subagent_identity("preview latest state").0, "Helper");
        assert_eq!(subagent_identity("").0, "Helper");
    }

    #[test]
    fn output_preview_keeps_unicode_and_limits_long_results() {
        let summary = "界".repeat(1400);
        let preview = subagent_output_preview(&summary);
        assert!(preview.starts_with(&"界".repeat(1200)));
        assert!(preview.ends_with("\n\n…"));
        let lines = (0..30)
            .map(|i| format!("Line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = subagent_output_preview(&lines);
        assert!(preview.contains("Line 17"));
        assert!(!preview.contains("Line 18"));
    }
}
