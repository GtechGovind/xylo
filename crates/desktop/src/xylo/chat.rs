//! The chat page: one centred reading column (760 px) holding the history
//! messages and the live turn (rendered by `transcript`), and the composer at
//! the same width underneath. The transcript fades into the page above the
//! composer and keeps a fixed bottom inset, so its last line is never hidden.
//!
//! Messages: the person's turns sit right-aligned on a neutral raised surface
//! that hugs its text; answers are full-width text on the page with an action
//! row (copy, retry, read aloud, time) that is always shown for the latest
//! answer and appears on hover for earlier ones. Hover actions are overlaid,
//! never reserving layout space.
use super::*;
use crate::icons::{self, AppIcon};
use gpui_kit::base::Disableable;
use gpui_kit::base::TestSupportExt;
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
#[path = "tool_result_view.rs"]
mod tool_result_view;
#[path = "chat/tools.rs"]
mod tools;
#[path = "transcript.rs"]
mod transcript;

/// The composer's prompt on the home screen, where nothing has been said yet.
const HOME_PLACEHOLDER: &str = "How can I help you today?";
/// The composer's prompt inside a conversation.
const REPLY_PLACEHOLDER: &str = "Reply to Xylo…";
/// Width of the reading column, shared by the transcript and the composer.
const COLUMN_WIDTH: f32 = 760.;
/// Space kept under the last transcript row so the fade never covers it;
/// at least [`FADE_HEIGHT`].
const BOTTOM_INSET: f32 = 32.;
/// Height of the fade between the transcript and the composer.
const FADE_HEIGHT: f32 = 28.;
const _: () = assert!(BOTTOM_INSET >= FADE_HEIGHT);
/// Space under an answer, holding its overlaid action row (one 28 px icon
/// button high) clear of the answer's last line.
const ANSWER_GAP: f32 = 30.;

/// Placeholder messages while a conversation's first page loads: a request
/// and a reply, twice, so the layout the reader is about to see is already
/// there.
fn history_skeleton() -> AnyElement {
    use crate::motion::{skeleton_block, skeleton_paragraph};
    let exchange = |reply_lines: usize, request_width: f32| {
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(22.))
            .child(
                div().w_full().flex().justify_end().child(
                    skeleton_block(relative(request_width), px(38.))
                        .map_element(|block| block.rounded(px(18.))),
                ),
            )
            .child(skeleton_paragraph(reply_lines).pt(px(4.)))
    };
    div()
        .id("history-skeleton")
        .test_support()
        .aria_label("Loading conversation")
        .w_full()
        .py(px(14.))
        .flex()
        .flex_col()
        .gap(px(30.))
        .child(exchange(4, 0.42))
        .child(exchange(3, 0.3))
        .into_any_element()
}

/// How a saved reply that did not finish is labelled after a reload.
fn reply_status_label(status: MessageStatus) -> Option<&'static str> {
    match status {
        MessageStatus::Complete => None,
        MessageStatus::Interrupted => Some("Stopped · the reply ended early"),
        MessageStatus::Failed => Some("Failed · the reply is incomplete"),
        MessageStatus::Truncated => Some("Cut off · the answer reached the output limit"),
    }
}

/// When an answer was written: the time today, the date before that.
fn time_label(at_ms: i64, now_ms: i64) -> String {
    use chrono::{Datelike, Local, TimeZone};
    let (Some(at), Some(now)) = (
        Local.timestamp_millis_opt(at_ms).single(),
        Local.timestamp_millis_opt(now_ms).single(),
    ) else {
        return String::new();
    };
    if at.date_naive() == now.date_naive() {
        at.format("%-I:%M %p").to_string()
    } else if at.year() == now.year() {
        at.format("%b %-d, %-I:%M %p").to_string()
    } else {
        at.format("%b %-d %Y, %-I:%M %p").to_string()
    }
}

/// Markdown as it should sound: code blocks are skipped and markup symbols
/// dropped, so "Read aloud" speaks the prose and not the syntax.
fn speakable_text(markdown: &str) -> String {
    let mut spoken = Vec::new();
    let mut in_code = false;
    for line in markdown.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code = !in_code;
            continue;
        }
        if in_code || trimmed.is_empty() || trimmed.chars().all(|ch| "|-: ".contains(ch)) {
            continue;
        }
        let line = trimmed
            .trim_start_matches(['#', '>', '-', '*', '+'])
            .trim_start();
        let mut text = String::with_capacity(line.len());
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '*' | '_' | '`' | '#' | '|' => {}
                // A link reads as its label: "[label](target)" → "label".
                '[' => {
                    for inner in chars.by_ref() {
                        if inner == ']' {
                            break;
                        }
                        text.push(inner);
                    }
                    if chars.peek() == Some(&'(') {
                        for skipped in chars.by_ref() {
                            if skipped == ')' {
                                break;
                            }
                        }
                    }
                }
                other => text.push(other),
            }
        }
        let text = text.trim();
        if !text.is_empty() {
            spoken.push(text.to_owned());
        }
    }
    spoken.join("\n")
}

/// What an answer's action row acts on and shows.
struct Answer {
    /// The message id, or the live answer's key.
    id: Uuid,
    text: SharedString,
    /// When it was written, in milliseconds.
    at_ms: i64,
    /// Why it is incomplete (stopped, failed, cut off), if it is.
    note: Option<SharedString>,
    /// The answer that ends the conversation: its row is always shown.
    latest: bool,
}

/// The identity of a live turn's answer until history stores it; the
/// archived reply takes the same id.
pub(super) fn live_answer_key(session: Uuid, turn: &TurnState) -> Uuid {
    let started = turn.started_at.unwrap_or_default();
    Uuid::new_v5(&session, format!("answer-{started}").as_bytes())
}

/// Image files attach as images; every other file is referenced by path in
/// the message, where the agent can read it.
pub(super) fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "gif"
            )
        })
}

/// A reveal-on-hover style shared by every overlaid action: hidden at rest,
/// shown while its message (including the gap holding the actions) is
/// hovered.
fn reveal_on_hover<E: Styled + InteractiveElement>(element: E, always: bool) -> E {
    if always {
        element
    } else {
        element
            .opacity(0.)
            .group_hover("chat-message", |style| style.opacity(1.))
    }
}

/// A small icon action of a message; keyboard focus always reveals it.
fn message_action(
    id: impl Into<ElementId>,
    glyph: AppIcon,
    label: &'static str,
    always: bool,
) -> gpui_kit::base::Button {
    reveal_on_hover(icons::icon_button(id, glyph, label), always).focus(|style| style.opacity(1.))
}

impl Xylo {
    pub(super) fn chat(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.recovery.open {
            return self.recovery_panel(cx);
        }
        if self.messages.is_empty()
            && !self.running()
            && !self.history_loading()
            && !self.has_live_transcript()
        {
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
        let Some(history) = self.histories.get(&self.session) else {
            return div().into_any_element();
        };
        let state = history.list.clone();
        let jump = self.running() && history.reader_scrolled_away();
        let weak = cx.entity().downgrade();
        let list = list(state, move |index, window, cx| {
            weak.update(cx, |view, cx| view.chat_row(index, window, cx))
                .unwrap_or_else(|_| div().into_any_element())
        })
        .size_full()
        .pb(px(BOTTOM_INSET));
        let canvas_color: Hsla = rgb(bg()).into();
        // Escape stops the task only while the transcript or composer has focus.
        let pane = div()
            .track_focus(&self.transcript_focus)
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(list)
            .child(
                // The transcript dissolves into the page above the composer.
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .h(px(FADE_HEIGHT))
                    .bg(linear_gradient(
                        0.,
                        linear_color_stop(canvas_color, 0.),
                        linear_color_stop(canvas_color.opacity(0.), 1.),
                    )),
            )
            .when(jump, |pane| {
                pane.child(
                    div()
                        .absolute()
                        .bottom(px(10.))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(crate::motion::fade_slide_in(
                            "jump-to-latest-enter",
                            pill("jump-to-latest", "Jump to latest")
                                .accessibility_label("Jump to the latest output")
                                .bg(rgb(panel()))
                                .border_color(rgb(border_strong()))
                                .shadow_sm()
                                .on_click(cx.listener(|view, _, _, cx| view.jump_to_latest(cx))),
                            8.,
                        )),
                )
            });
        div()
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .child(self.recovery_banner(cx))
            .child(pane)
            .child(self.history_later_footer(cx))
            .child(
                div()
                    .w_full()
                    .flex()
                    .justify_center()
                    .px(px(28.))
                    .pb(px(20.))
                    .flex_shrink_0()
                    .child(self.prompt_box(cx)),
            )
            .into_any_element()
    }
    /// Applies one engine event to its chat's turn. A new turn first moves the
    /// previous reply into the message list so the transcript stays in order.
    pub(super) fn engine_event(
        &mut self,
        session: Uuid,
        event: EngineEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.log(match &event {
            EngineEvent::Delta { .. } => String::new(),
            EngineEvent::ToolStarted { name, .. } => format!("Tool started: {name}"),
            EngineEvent::ToolFinished { name, success, .. } => {
                format!("Tool finished: {name}, success={success}")
            }
            EngineEvent::ApprovalRequired { name, .. } => format!("Approval required: {name}"),
            other => format!("{other:?}"),
        });
        // A schedule the agent created or changed shows on the Scheduled page
        // without waiting for a restart.
        if let EngineEvent::ToolFinished {
            name,
            success: true,
            ..
        } = &event
            && name.starts_with("schedule_")
        {
            self.refresh_schedules(cx);
        }
        let running = self.turns.get(&session).is_some_and(|turn| turn.running);
        let marked_before = self.chat_busy(session);
        let opens_turn = matches!(event, EngineEvent::Started)
            || (!running
                && matches!(
                    event,
                    EngineEvent::Phase {
                        phase: TurnPhase::Preparing
                    }
                ));
        if opens_turn {
            self.archive_finished_turn(session);
            if session == self.session {
                self.refresh_composer_placeholder(cx);
            }
        }
        let finished_tool = match &event {
            EngineEvent::ToolFinished { action_id, .. } => Some(*action_id),
            _ => None,
        };
        let completed = matches!(event, EngineEvent::Completed { .. });
        let review_changed = matches!(
            &event,
            EngineEvent::Started
                | EngineEvent::Completed { .. }
                | EngineEvent::Cancelled
                | EngineEvent::Error { .. }
                | EngineEvent::ToolFinished { .. }
        );
        self.turns.entry(session).or_default().apply(event);
        if let Some(action) = finished_tool
            && session == self.session
        {
            // The reducer already parsed the result once for its card.
            let artifacts: Vec<PathBuf> = self
                .turns
                .get(&session)
                .and_then(|turn| turn.tool(action))
                .and_then(|tool| tool.presentation.parsed_output.as_deref())
                .and_then(|value| value.get("artifacts").and_then(|v| v.as_array()))
                .into_iter()
                .flatten()
                .filter_map(|path| path.as_str().map(PathBuf::from))
                .collect();
            for path in artifacts {
                self.record_artifact(path);
            }
        }
        if completed {
            if session != self.session
                && let Some(history) = self.histories.get_mut(&session)
            {
                history.stale = true;
            }
            let _ = self.host.as_ref().map(|h| h.list_conversations());
        }
        if session == self.session && review_changed {
            self.recovery_activate(window, cx);
            self.refresh_action_reviews(cx);
        }
        // A chat in the background is drawn only as its sidebar row, so its
        // events redraw the window only when that row's mark changes.
        if session == self.session || marked_before != self.chat_busy(session) {
            cx.notify();
        }
    }
    /// Rows after the history messages: the turn's work summary, one row per
    /// transcript segment, and its activity or outcome row.
    pub(super) fn live_row_count(&self) -> usize {
        let turn = self.turns.get(&self.session);
        let segments = turn.map_or(0, |turn| turn.segments.len());
        let active =
            self.sending_turn() || turn.is_some_and(|turn| turn.running || turn.outcome.is_some());
        if segments == 0 && !active {
            0
        } else {
            segments + 2
        }
    }
    pub(super) fn has_live_transcript(&self) -> bool {
        self.live_row_count() > 0
    }
    /// A message was sent to an idle chat and the host has not started it yet.
    pub(super) fn sending_turn(&self) -> bool {
        !self.running() && self.draft_send_pending()
    }
    /// Moves a finished turn's reply into the message list, as history stores
    /// it, so the next turn renders after it.
    pub(super) fn archive_finished_turn(&mut self, session: Uuid) {
        let Some(turn) = self.turns.get_mut(&session) else {
            return;
        };
        if turn.running || (turn.segments.is_empty() && turn.outcome.is_none()) {
            return;
        }
        // A retried turn resumes under the same id; its partial reply is not history.
        let reply = turn.reply().filter(|_| !turn.resend);
        let finished_at = turn.finished_at;
        let answer = live_answer_key(session, turn);
        turn.archive();
        if let Some((text, status)) = reply {
            if session == self.session {
                let mut reply = message(session, "assistant", text);
                // The answer keeps its identity, so a reading in progress
                // still belongs to it.
                reply.id = answer;
                reply.status = status;
                if let Some(at) = finished_at {
                    reply.created_at = at;
                }
                self.messages.push(reply);
            } else if let Some(history) = self.histories.get_mut(&session) {
                history.stale = true;
            }
        }
        if session == self.session {
            self.invalidate_live_history();
        }
    }
    /// Runs the failed turn again under its own id: the engine resumes it from
    /// its checkpoint and the host's user-message registration is idempotent,
    /// so the prompt is not repeated in the transcript.
    pub(super) fn retry_last_message(&mut self, cx: &mut Context<Self>) {
        if self.running() || self.sending_turn() || self.recovery_blocks_submit() {
            return;
        }
        if let Err(error) = self.settings.provider.validate() {
            self.error = Some(error.to_string());
            self.settings_open = true;
            self.navigate(Page::Providers, cx);
            return;
        }
        let Some(turn) = self.turns.get_mut(&self.session) else {
            return;
        };
        let Some(mut request) = turn
            .request
            .clone()
            .filter(|_| turn.failure().is_some_and(|failure| failure.retryable) && !turn.resend)
        else {
            self.error = Some("This task can no longer be retried. Send the message again.".into());
            cx.notify();
            return;
        };
        turn.resend = true;
        turn.render_revision = turn.render_revision.wrapping_add(1);
        // The same prompt and images, with the connection fixed since the failure.
        request.provider = self.settings.provider.clone();
        request.permissions = self.settings.permissions;
        request.approved_rules = Vec::new();
        self.engine_command(WorkerCommand::Run { turn: request }, cx);
        self.invalidate_live_history();
        self.sync_history_list(cx);
        cx.notify();
    }
    /// The prompt "Retry" under the latest answer asks again: the person's
    /// last message, when it was text only and nothing else is in flight or
    /// typed.
    fn retry_prompt(&self, cx: &App) -> Option<&str> {
        if self.running()
            || self.sending_turn()
            || !self.composer.read(cx).value().trim().is_empty()
        {
            return None;
        }
        self.messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .filter(|message| message.attachments.is_empty() && !message.content.trim().is_empty())
            .map(|message| message.content.as_str())
    }
    /// Asks the latest question again through the normal send path, so drafts,
    /// admission and the queue treat it like any other message.
    fn retry_answer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.retry_prompt(cx).map(str::to_owned) else {
            return;
        };
        self.composer
            .update(cx, |input, cx| input.set_value(prompt, window, cx));
        self.draft_changed(cx);
        self.submit(window, cx);
    }
    /// Puts a sent message back in the composer to edit and send again. A
    /// draft already being typed is kept above it.
    fn edit_message(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.composer.read(cx).value().trim_end().to_owned();
        let value = if draft.is_empty() {
            text.to_owned()
        } else {
            format!("{draft}\n\n{text}")
        };
        self.composer.update(cx, |input, cx| {
            input.set_value(value, window, cx);
            input.focus(window, cx);
        });
        self.draft_changed(cx);
        cx.notify();
    }
    /// Whether the answer `answer` (a message id, or the live answer's key)
    /// is the one being read aloud: its speech is still being made or plays.
    fn reading_answer(&self, answer: Uuid) -> bool {
        self.reading_aloud == Some(answer) && (self.speech.playing || self.speech.busy())
    }
    /// Reads an answer aloud with the selected voice, or stops it when it is
    /// the one being read.
    fn read_aloud(&mut self, answer: Uuid, markdown: &str, cx: &mut Context<Self>) {
        if self.reading_answer(answer) {
            self.reading_aloud = None;
            self.stop_speech(cx);
            cx.notify();
            return;
        }
        self.reading_aloud = Some(answer);
        self.speech_speak_text(speakable_text(markdown), cx);
    }
    fn chat_row(&self, index: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let is_message = index > 0 && index <= self.messages.len();
        let content = if index == 0 {
            self.history_header(cx)
        } else if let Some(message) = self.messages.get(index - 1) {
            self.message_row(index, message, cx)
        } else {
            self.live_row(index - 1 - self.messages.len(), window, cx)
        };
        let user = is_message && self.messages[index - 1].role == "user";
        div()
            .w_full()
            .flex()
            .justify_center()
            .px(px(28.))
            .when(is_message && user, |row| row.pt(px(2.)).pb(px(10.)))
            .when(is_message && !user, |row| row.pt(px(4.)))
            .child(div().w_full().max_w(px(COLUMN_WIDTH)).child(content))
            .into_any_element()
    }
    /// The first list row: placeholder messages while a conversation loads,
    /// a spinner row while an earlier page loads, or the control that asks
    /// for one.
    fn history_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let history = self.histories.get(&self.session);
        let loading = history.is_some_and(|history| history.loading);
        if loading && self.messages.is_empty() && !self.has_live_transcript() {
            return history_skeleton();
        }
        div()
            .flex()
            .justify_center()
            .py_2()
            .child(if loading {
                div()
                    .id("history-loading-older")
                    .test_support()
                    .h(px(30.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(12.5))
                    .text_color(rgb(muted()))
                    .child(crate::motion::spinner_inline())
                    .child(if history.is_some_and(|history| history.loading_older()) {
                        "Loading earlier messages…"
                    } else {
                        "Updating conversation…"
                    })
                    .into_any_element()
            } else if history.is_some_and(|history| history.has_more) {
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
    }
    fn message_row(
        &self,
        index: usize,
        message: &ChatMessage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if message.role == "user" {
            return div()
                .id(("chat-message", index))
                .test_support()
                .w_full()
                .min_w_0()
                .group("chat-message")
                .child(self.user_message(index, message, cx))
                .into_any_element();
        }
        // The hover group spans the gap below the answer, where its action
        // row is overlaid, so moving onto the actions keeps them shown.
        div()
            .w_full()
            .min_w_0()
            .relative()
            .pb(px(ANSWER_GAP))
            .group("chat-message")
            .child(
                div()
                    .id(("chat-message", index))
                    .test_support()
                    .w_full()
                    .min_w_0()
                    .child(self.answer_message(index, message, cx)),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .bottom_0()
                    .child(self.answer_actions(
                        index,
                        Answer {
                            id: message.id,
                            text: self.message_text(message),
                            at_ms: message.created_at,
                            note: reply_status_label(message.status).map(SharedString::from),
                            latest: index == self.messages.len() && !self.has_live_transcript(),
                        },
                        cx,
                    )),
            )
            .into_any_element()
    }
    /// The person's message: a neutral bubble that hugs its text, at most 80%
    /// of the column, with copy and edit beside it on hover.
    fn user_message(
        &self,
        index: usize,
        message: &ChatMessage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = self.message_text(message);
        let copy_text = content.clone();
        let edit_text = content.clone();
        let bubble = div()
            .id(("message-body", index))
            .test_support()
            .min_w_0()
            .max_w(relative(0.8))
            .flex()
            .flex_col()
            .gap_2()
            .px(px(14.))
            .py(px(8.))
            .rounded(px(18.))
            .bg(rgb(well()))
            .text_size(px(15.))
            .line_height(relative(1.5))
            .text_color(rgb(text()))
            .when(self.history_message_highlight(message.id), |row| {
                row.border_1().border_color(rgb(brand()))
            })
            .when(!message.content.is_empty(), |row| {
                row.child(
                    TextView::markdown(SharedString::from(message.id.to_string()), content)
                        .selectable(true),
                )
            })
            .when(!message.attachments.is_empty(), |row| {
                row.child(self.message_images(message, cx))
            });
        let actions = div()
            .id(("message-actions", index))
            .flex_shrink_0()
            .flex()
            .items_center()
            .child(
                message_action(("copy-message", index), AppIcon::Copy, "Copy", false).on_click(
                    move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_text.to_string()))
                    },
                ),
            )
            .when(!message.content.trim().is_empty(), |row| {
                row.child(
                    message_action(("edit-message", index), AppIcon::Edit, "Edit", false).on_click(
                        cx.listener(move |view, _, window, cx| {
                            view.edit_message(&edit_text, window, cx)
                        }),
                    ),
                )
            });
        div()
            .w_full()
            .flex()
            .justify_end()
            .items_end()
            .gap(px(4.))
            .child(actions)
            .child(bubble)
            .into_any_element()
    }
    /// An answer from history: text on the page.
    fn answer_message(
        &self,
        index: usize,
        message: &ChatMessage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(("message-body", index))
            .test_support()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .text_size(px(15.))
            .line_height(relative(1.7))
            .text_color(rgb(text()))
            .when(self.history_message_highlight(message.id), |row| {
                row.border_1().border_color(rgb(brand()))
            })
            .when(!message.content.is_empty(), |row| {
                row.child(
                    TextView::markdown(
                        SharedString::from(message.id.to_string()),
                        self.message_text(message),
                    )
                    .selectable(true),
                )
            })
            .when(!message.attachments.is_empty(), |row| {
                row.child(self.message_images(message, cx))
            })
            .into_any_element()
    }
    /// Copy, Retry (latest answer only), Read aloud and when it was written.
    /// The latest answer shows the row; earlier ones reveal it on hover.
    fn answer_actions(&self, index: usize, answer: Answer, cx: &mut Context<Self>) -> AnyElement {
        let Answer {
            id: answer,
            text,
            at_ms,
            note,
            latest,
        } = answer;
        let copy_text = text.clone();
        let reading = self.reading_answer(answer);
        // Another answer (or a recording) is using the speaker.
        let speech_busy = (self.speech.busy() || self.speech.playing) && !reading;
        let retry = latest && self.retry_prompt(cx).is_some();
        // A status note (stopped, failed, cut off) is always shown.
        let show_time = latest || note.is_some();
        // The glyph sits 6 px inside its hit area; pull the row left so the
        // first icon lines up with the answer text.
        div()
            .id(("message-actions", index))
            .ml(px(-6.))
            .flex()
            .items_center()
            .child(
                message_action(("copy-message", index), AppIcon::Copy, "Copy", latest).on_click(
                    move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_text.to_string()))
                    },
                ),
            )
            .when(retry, |row| {
                row.child(
                    message_action(("retry-message", index), AppIcon::Retry, "Retry", true)
                        .on_click(cx.listener(|view, _, window, cx| view.retry_answer(window, cx))),
                )
            })
            .child(
                message_action(
                    ("read-aloud", index),
                    if reading {
                        AppIcon::Stop
                    } else {
                        AppIcon::ReadAloud
                    },
                    if reading {
                        "Stop reading"
                    } else {
                        "Read aloud"
                    },
                    latest || reading,
                )
                .disabled(speech_busy)
                .on_click(cx.listener(move |view, _, _, cx| view.read_aloud(answer, &text, cx))),
            )
            .child(reveal_on_hover(
                div()
                    .id(("message-time", index))
                    .pl(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .single_line()
                    .text_size(px(12.))
                    .text_color(rgb(faint()))
                    .child(time_label(at_ms, now_ms()))
                    .when_some(note, |row, note| {
                        row.child("·").child(
                            div()
                                .id(("message-status", index))
                                .test_support()
                                .text_color(rgb(muted()))
                                .child(note),
                        )
                    }),
                show_time,
            ))
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
            // The engine shortens very large change previews; a change nobody
            // could inspect in full is never approved by pointer or keyboard.
            if allow
                && card
                    .input
                    .get("diff_truncated")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
            {
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
    /// ⌘⇧↩ approves only what the person is looking at: the chat page with
    /// nothing layered above it, its transcript on screen, and focus in the
    /// composer or the transcript (the same rule as Escape).
    fn approval_shortcut_applies(&self, window: &Window, cx: &App) -> bool {
        self.page == Page::Chat
            && !self.settings_open
            && !self.recovery.open
            && !self.model_picker.open
            && !self.history_search.is_open()
            && self.has_live_transcript()
            && (self
                .composer
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
                || self.transcript_focus.contains_focused(window, cx))
    }
    /// Keyboard: allow the oldest pending action in this chat, when its card
    /// is in front of the person.
    pub(super) fn allow_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.approval_shortcut_applies(window, cx) {
            return;
        }
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
            None if self.running() || self.sending_turn() => {
                self.engine_command(WorkerCommand::Cancel, cx)
            }
            None => {}
        }
    }
    /// ⌘.: stops the task from anywhere, but denies a pending action only
    /// when its card is in front of the person (as ⌘⇧↩ allows only then).
    pub(super) fn deny_or_stop_shortcut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending = self
            .turns
            .get(&self.session)
            .is_some_and(|turn| turn.pending_approvals().next().is_some());
        if pending && !self.approval_shortcut_applies(window, cx) {
            return;
        }
        self.deny_or_stop(cx);
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
        // Context line under the composer: where we are. The model picker in
        // the composer already names the model, so only a missing one is noted.
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
            (Some(name), ready) => {
                context = context
                    .child(icons::small(AppIcon::Folder).text_color(rgb(icons::rest_color())))
                    .child(name.clone())
                    .when(!ready, |context| context.child("· loading project…"));
            }
        }
        if !configured {
            context = context.child("·").child("No model set up");
        }
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
                    .max_w(px(COLUMN_WIDTH))
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
                                    (AppIcon::Skills, "Explain this project", "Explain this project using read-only inspection. Summarize its architecture, key entry points, and how to run its tests. Do not edit files."),
                                    (AppIcon::Changes, "Review my changes", "Review the current project changes using read-only inspection. Look for bugs, regressions, and missing tests. Do not edit files."),
                                    (AppIcon::Plan, "Plan before editing", "Inspect this project and help me plan a change before making any edits. Ask what I want to change, then propose a plan and wait for my approval before editing files."),
                                ].into_iter().enumerate().map(|(index, (glyph, label, prompt))| {
                                    chip_with(("chat-home-starter", index), Some(icons::small(glyph)), label)
                                        .on_click(cx.listener(move |view, _, window, cx| {
                                            view.populate_chat_starter(prompt, window, cx);
                                        }))
                                })),
                        )
                    }),
            )
            .into_any_element()
    }
    /// The composer's "+" menu: attach files or images, or transcribe an
    /// audio file into the message. Pasting and saved drafts live where they
    /// already work (⌘V and the sidebar).
    fn composer_context_actions(&self, cx: &Context<Self>) -> AnyElement {
        let weak = cx.entity().downgrade();
        icons::menu_trigger("composer-context-actions", AppIcon::Add, "Add context")
            .tooltip("Add files or audio")
            .disabled(self.attachments.importing_for(self.session))
            .dropdown_menu(move |menu, _, _| {
                let attach = weak.clone();
                let transcribe = weak.clone();
                menu.item(PopupMenuItem::new("Attach files or images…").on_click(
                    move |_, window, cx| {
                        let _ = attach.update(cx, |view, cx| view.attachment_picker(window, cx));
                    },
                ))
                .item(PopupMenuItem::new("Transcribe audio file…").on_click(
                    move |_, window, cx| {
                        let _ = transcribe
                            .update(cx, |view, cx| view.transcribe_audio_file(window, cx));
                    },
                ))
            })
            .into_any_element()
    }
    /// Picks an audio file and transcribes it on this computer; the text then
    /// waits above the composer to be inserted.
    fn transcribe_audio_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |view, cx| {
            let mut picker = rfd::AsyncFileDialog::new()
                .set_title("Transcribe audio file")
                .add_filter("Audio", &["wav", "mp3", "m4a", "flac", "ogg"]);
            if let Some(workspace) = workspace {
                picker = picker.set_directory(workspace);
            }
            if let Some(file) = picker.pick_file().await {
                let path = file.path().to_path_buf();
                let _ = view.update_in(cx, |view, _, cx| view.speech_transcribe_path(path, cx));
            }
        })
        .detach();
    }
    /// Adds paths that are not images to the message as references the agent
    /// can open.
    pub(super) fn reference_files_in_composer(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        let references = paths
            .iter()
            .map(|path| {
                let shown = self
                    .workspace
                    .as_ref()
                    .and_then(|root| path.strip_prefix(root).ok())
                    .unwrap_or(path);
                format!("`{}`", shown.display())
            })
            .collect::<Vec<_>>()
            .join(" ");
        let draft = self.composer.read(cx).value().trim_end().to_owned();
        let value = if draft.is_empty() {
            references
        } else {
            format!("{draft} {references}")
        };
        self.composer.update(cx, |input, cx| {
            input.set_value(value, window, cx);
            input.focus(window, cx);
        });
        self.draft_changed(cx);
        cx.notify();
    }
    /// Moves a finished transcription into the message.
    fn insert_transcript(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let transcript = std::mem::take(&mut self.speech.transcript);
        let draft = self.composer.read(cx).value().trim_end().to_owned();
        let value = if draft.is_empty() {
            transcript
        } else {
            format!("{draft} {transcript}")
        };
        self.composer.update(cx, |input, cx| {
            input.set_value(value, window, cx);
            input.focus(window, cx);
        });
        self.draft_changed(cx);
        cx.notify();
    }
    /// Speech work for the composer: a transcription waiting to be inserted,
    /// or a calm progress line while audio is transcribed or synthesized.
    fn speech_strip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.speech.transcript.is_empty() {
            return Some(
                div()
                    .id("composer-transcript")
                    .test_support()
                    .mb_2()
                    .px(px(12.))
                    .py(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .rounded(px(12.))
                    .bg(rgb(panel()))
                    .border_1()
                    .border_color(rgb(border()))
                    .child(icons::small(AppIcon::Mic).text_color(rgb(icons::rest_color())))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .single_line()
                            .text_size(px(13.))
                            .text_color(rgb(text_secondary()))
                            .child(self.speech.transcript.clone()),
                    )
                    .child(
                        tools::detail_action("insert-transcript", "Insert").on_click(
                            cx.listener(|view, _, window, cx| view.insert_transcript(window, cx)),
                        ),
                    )
                    .child(
                        icons::icon_button("dismiss-transcript", AppIcon::Close, "Discard")
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.speech.transcript.clear();
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }
        let processing =
            self.speech.busy() && !self.speech.playing && !self.speech_recording_active();
        (processing && !self.speech.notice.is_empty()).then(|| {
            div()
                .id("composer-speech-status")
                .test_support()
                .mb_2()
                .px(px(4.))
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(12.5))
                .text_color(rgb(muted()))
                .child(crate::motion::spinner_inline())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .single_line()
                        .child(self.speech.notice.clone()),
                )
                .child(
                    tools::detail_action("cancel-speech", "Cancel")
                        .on_click(cx.listener(|view, _, _, cx| view.cancel_speech_work(cx))),
                )
                .into_any_element()
        })
    }
    /// Cancels the transcription or synthesis in flight: the host stops the
    /// request, its result is ignored if it still arrives, the strip goes
    /// away and Read aloud is offered again.
    fn cancel_speech_work(&mut self, cx: &mut Context<Self>) {
        self.stop_speech(cx);
        self.speech.notice.clear();
        self.reading_aloud = None;
        cx.notify();
    }
    /// The composer's prompt for the page as it is now: the home greeting
    /// before anything is said, "Reply to Xylo…" inside a conversation.
    fn composer_placeholder(&self) -> &'static str {
        if self.messages.is_empty() && !self.has_live_transcript() && !self.draft_send_pending() {
            HOME_PLACEHOLDER
        } else {
            REPLY_PLACEHOLDER
        }
    }
    /// Brings the composer's prompt in step with the page. Called where the
    /// page changes (new chat, opening a conversation, sending, a turn
    /// starting); the update runs once the current event is handled, outside
    /// any draw, so the window repaints with it.
    pub(super) fn refresh_composer_placeholder(&self, cx: &mut Context<Self>) {
        let wanted = self.composer_placeholder();
        let composer = self.composer.clone();
        cx.defer(move |cx| {
            for window in cx.windows() {
                let updated = window.update(cx, |_, window, cx| {
                    composer.update(cx, |input, cx| {
                        if input.presentation().placeholder().as_ref() != wanted {
                            input.set_placeholder(wanted, window, cx);
                        }
                    })
                });
                if updated.is_ok() {
                    break;
                }
            }
        });
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
        let stoppable = running || self.sending_turn();
        div()
            .w_full()
            .max_w(px(COLUMN_WIDTH))
            .child(self.pending_messages(cx))
            .children(self.speech_strip(cx))
            .child(
                div()
                    .id("image-drop-target")
                    .test_support()
                    .w_full()
                    .rounded(px(18.))
                    .bg(rgb(panel()))
                    .border_1()
                    .border_color(rgb(border_strong()))
                    .shadow_sm()
                    .flex()
                    .flex_col()
                    .px(px(14.))
                    .pt(px(12.))
                    .pb(px(10.))
                    .on_drop(cx.listener(|view, paths: &ExternalPaths, _, cx| {
                        view.attach_image_paths(paths.paths().to_vec(), AttachmentSource::Drop, cx)
                    }))
                    .child(
                        div().text_size(px(15.)).child(
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
                        |surface| surface.child(div().pb_2().child(self.attachment_strip(cx))),
                    )
                    // While images import, the strip's own progress row says so.
                    .when(
                        !self.attachments.notice.is_empty()
                            && !self.attachments.importing_for(self.session),
                        |surface| {
                            surface.child(
                                div().pb_2().child(subtitle(self.attachments.notice.clone())),
                            )
                        },
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .when(running && (!draft_empty || has_attachments), |row| row.flex_wrap())
                            .min_h(px(32.))
                            .child(self.composer_context_actions(cx))
                            .child(self.composer_permissions(cx))
                            .child(div().flex_1().min_w_0())
                            .children(self.composer_context(cx))
                            .child(self.unified_model_control(model, cx))
                            .child(self.speech_capture_button(cx))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .flex_shrink_0()
                                    .when(running && !draft_empty, |row| {
                                        row.child(pill("steer-prompt", "Steer")
                                            .accessibility_label("Steer current task")
                                            .border_color(rgb(border_strong()))
                                            .disabled(self.pending_guidance_text().is_some() || self.draft_send_pending() || has_attachments || self.attachments.importing_for(self.session) || self.draft_flush_pending())
                                            .tooltip(|_,cx| cx.new(|_| XyloTooltip("Update the active task at its next safe step. Images can be queued for the next turn.".into())).into())
                                            .on_click(cx.listener(|view,_,window,cx| view.steer_composer(window,cx))))
                                    })
                                    .when(stoppable, |row| {
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
                                                crate::motion::spinner(icons::SMALL)
                                                    .color(rgb(brand_foreground()).into())
                                                    .into_any_element()
                                            } else if running {
                                                div().text_size(px(12.5)).font_weight(FontWeight::MEDIUM).child("Queue").into_any_element()
                                            } else {
                                                icons::icon(AppIcon::Send).into_any_element()
                                            })
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.submit(window, cx)
                                            }))
                                        })
                                    }),
                            ),
                    ),
            )
            .when_some(self.composer_hint(), |surface, hint| {
                surface.child(
                    div()
                        .id("composer-hint")
                        .test_support()
                        .pt_1()
                        .px_2()
                        .text_size(px(12.))
                        .text_color(rgb(muted()))
                        .child(hint.to_owned()),
                )
            })
            .when(self.draft_needs_attention(), |surface| {
                surface.child(div().pt_1().child(self.draft_status_footer(cx)))
            })
            .into_any_element()
    }
}
use crate::state::ToolCard;

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

#[cfg(test)]
#[path = "chat/tests.rs"]
mod tests;
#[cfg(test)]
#[path = "chat/transcript_flow_tests.rs"]
mod transcript_flow_tests;
