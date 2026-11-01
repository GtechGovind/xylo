//! Usage receipts, style selection and the composer's compact status bar.
use super::*;
#[path = "usage/forms.rs"]
mod forms;
#[path = "usage/status.rs"]
mod status;
use gpui_kit::base::TestSupportExt;
use gpui_kit::component::{
    button::{Button as MenuButton, ButtonVariants},
    menu::{DropdownMenu, PopupMenuItem},
};
use xylo_core::pricing::{CostEstimate, ModelPrice};
use xylo_runtime::workspace::output_styles::OutputStyle;
use xylo_runtime::{UsageReport, UsageTotals};

pub(super) struct UsageUi {
    pub report: Option<(Uuid, UsageReport)>,
    pub branch: Option<String>,
    pending: Option<(Uuid, Option<PathBuf>)>,
    key: Option<(Uuid, Vec<Uuid>, i64, Option<PathBuf>)>,
    pub styles: Vec<OutputStyle>,
    style_request: Option<(Uuid, PathBuf)>,
    status_request: Option<(Uuid, Uuid, PathBuf, String)>,
    status_context: Option<(Uuid, PathBuf, String)>,
    status_tick: Option<Task<()>>,
    status_last: Option<std::time::Instant>,
    pub status_text: Option<String>,
    model: Entity<InputState>,
    rates: [Entity<InputState>; 4],
    command: Entity<InputState>,
    pub editing_price: bool,
    pub editing_command: bool,
    pub form_error: Option<String>,
    pub command_error: Option<String>,
    pub report_error: Option<String>,
    pub style_error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl UsageUi {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Xylo>) -> Self {
        let mut input = |placeholder: &str| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.to_owned()))
        };
        let model = input("Model identifier");
        let rates = [
            input("Input"),
            input("Output"),
            input("Cache read"),
            input("Cache write"),
        ];
        let command = input("Optional shell command");
        let mut subscriptions = Vec::new();
        for field in std::iter::once(&model).chain(rates.iter()) {
            subscriptions.push(cx.subscribe(field, |view, _, event: &InputEvent, _| {
                if matches!(event, InputEvent::Change) {
                    view.usage_ui.editing_price = true;
                }
            }));
        }
        subscriptions.push(cx.subscribe(&command, |view, _, event: &InputEvent, _| {
            if matches!(event, InputEvent::Change) {
                view.usage_ui.editing_command = true;
            }
        }));
        Self {
            _subscriptions: subscriptions,
            report: None,
            branch: None,
            pending: None,
            key: None,
            styles: [
                ("default", "Default"),
                ("explanatory", "Explanatory"),
                ("learning", "Learning"),
            ]
            .into_iter()
            .map(|(key, label)| OutputStyle {
                key: key.into(),
                label: label.into(),
                source: None,
                prompt: String::new(),
            })
            .collect(),
            style_request: None,
            status_request: None,
            status_context: None,
            status_tick: None,
            status_last: None,
            status_text: None,
            model,
            rates,
            command,
            editing_price: false,
            editing_command: false,
            form_error: None,
            command_error: None,
            report_error: None,
            style_error: None,
        }
    }
}

fn day_range() -> (i64, i64) {
    use chrono::TimeZone;
    let date = chrono::Local::now().date_naive();
    let start = chrono::Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        .earliest();
    let end = chrono::Local
        .from_local_datetime(&date.succ_opt().unwrap().and_hms_opt(0, 0, 0).unwrap())
        .earliest();
    match (start, end) {
        (Some(start), Some(end)) => (start.timestamp_millis(), end.timestamp_millis()),
        _ => {
            let now = now_ms();
            (now.saturating_sub(86_400_000), now.saturating_add(1))
        }
    }
}

pub(super) fn cost_label(cost: Option<CostEstimate>) -> String {
    match cost {
        Some(cost) if cost.unpriced_calls == 0 => format!("${:.4} est.", cost.usd),
        Some(cost) if cost.usd > 0. => {
            format!("${:.4}+ est. · {} unpriced", cost.usd, cost.unpriced_calls)
        }
        _ => "Cost unavailable".into(),
    }
}

fn usage_label(usage: &Usage) -> String {
    format!(
        "{} in · {} out · {} cache read · {} cache write · {}",
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_tokens.unwrap_or(0),
        usage.cache_write_tokens.unwrap_or(0),
        cost_label(usage.estimated_cost)
    )
}

fn totals_label(totals: &UsageTotals) -> String {
    format!(
        "{} in · {} out · {} cache read · {} cache write · {}",
        model_picker::format_tokens(totals.input_tokens),
        model_picker::format_tokens(totals.output_tokens),
        model_picker::format_tokens(totals.cache_read_tokens),
        model_picker::format_tokens(totals.cache_write_tokens),
        cost_label(Some(CostEstimate {
            usd: totals.estimated_usd,
            unpriced_calls: totals.unpriced_calls
        }))
    )
}

impl Xylo {
    /// Schedules a bounded read only when the visible answer set or day changes.
    pub(super) fn refresh_usage(&mut self, cx: &mut Context<Self>) {
        if self.host.is_none() || !self.settings_loaded {
            return;
        }
        let (start, end) = day_range();
        let mut ids: Vec<_> = self
            .messages
            .iter()
            .filter(|message| message.role == "assistant")
            .map(|message| message.id)
            .take(499)
            .collect();
        if let Some(turn) = self.turns.get(&self.session) {
            ids.push(chat::live_answer_key(self.session, turn));
        }
        let key = (self.session, ids.clone(), start, self.workspace.clone());
        if self.usage_ui.pending.is_none() && self.usage_ui.key.as_ref() != Some(&key) {
            let id = Uuid::new_v4();
            let session = self.session;
            let workspace = self.workspace.clone();
            self.usage_ui.pending = Some((id, workspace.clone()));
            self.usage_ui.key = Some(key);
            self.host_call(cx, |host| {
                host.load_usage(id, session, ids, start, end, workspace)
            });
        }
        self.refresh_status_command(cx);
    }

    pub(super) fn invalidate_usage(&mut self) {
        self.usage_ui.key = None;
    }

    pub(super) fn usage_received(
        &mut self,
        id: Uuid,
        session: Uuid,
        result: Result<(UsageReport, Option<String>), String>,
        cx: &mut Context<Self>,
    ) {
        let Some((pending, root)) = self.usage_ui.pending.take() else {
            return;
        };
        if pending != id {
            self.usage_ui.pending = Some((pending, root));
            return;
        }
        if session == self.session && root == self.workspace {
            match result {
                Ok((report, branch)) => {
                    self.usage_ui.report = Some((session, report));
                    self.usage_ui.branch = branch;
                    self.usage_ui.report_error = None;
                }
                Err(error) => self.usage_ui.report_error = Some(error),
            }
        }
        cx.notify();
    }

    pub(super) fn answer_usage(&self, id: Uuid, index: usize) -> Option<AnyElement> {
        let usage = self
            .usage_ui
            .report
            .as_ref()
            .filter(|(session, _)| *session == self.session)
            .and_then(|(_, report)| report.messages.get(&id))
            .or_else(|| {
                self.turns
                    .get(&self.session)
                    .filter(|turn| chat::live_answer_key(self.session, turn) == id)
                    .map(|turn| &turn.usage)
            })?;
        if usage.input_tokens == 0 && usage.output_tokens == 0 && usage.estimated_cost.is_none() {
            return None;
        }
        Some(
            div()
                .id(("answer-usage", index))
                .test_support()
                .ml_2()
                .min_w_0()
                .flex_1()
                .single_line()
                .text_size(px(11.))
                .text_color(rgb(muted()))
                .opacity(0.)
                .group_hover("chat-message", |style| style.opacity(1.))
                .child(usage_label(usage))
                .tooltip({
                    let label = usage_label(usage);
                    move |_, cx| cx.new(|_| XyloTooltip(label.clone().into())).into()
                })
                .into_any_element(),
        )
    }

    pub(super) fn usage_totals(&self) -> AnyElement {
        let Some((_, report)) = self
            .usage_ui
            .report
            .as_ref()
            .filter(|(session, _)| *session == self.session)
        else {
            return subtitle(
                self.usage_ui
                    .report_error
                    .clone()
                    .unwrap_or_else(|| "Usage appears after the first completed answer.".into()),
            )
            .into_any_element();
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .text_size(px(11.5))
            .text_color(rgb(text_secondary()))
            .child(format!(
                "Conversation · {}",
                totals_label(&report.conversation)
            ))
            .child(format!("Today · {}", totals_label(&report.daily)))
            .child("Estimates cover reported turn tokens, including helpers. Title generation, service and tool fees are excluded.")
            .children(
                self.usage_ui
                    .report_error
                    .as_ref()
                    .map(|error| settings_ui::inline_error(error)),
            )
            .into_any_element()
    }

    pub(super) fn request_output_styles(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.workspace.clone() else {
            return;
        };
        let id = Uuid::new_v4();
        self.usage_ui.style_request = Some((id, root.clone()));
        self.host_call(cx, |host| host.output_styles(id, root));
    }

    pub(super) fn output_styles_received(
        &mut self,
        id: Uuid,
        result: Result<Vec<OutputStyle>, String>,
        cx: &mut Context<Self>,
    ) {
        let Some((pending, root)) = self.usage_ui.style_request.take() else {
            return;
        };
        if pending != id {
            self.usage_ui.style_request = Some((pending, root));
            return;
        }
        if self.workspace.as_ref() == Some(&root) {
            match result {
                Ok(styles) => {
                    self.usage_ui.styles = styles;
                    self.usage_ui.style_error = None;
                }
                Err(error) => self.usage_ui.style_error = Some(error),
            }
        }
        cx.notify();
    }

    pub(super) fn output_style_control(&self, cx: &Context<Self>) -> AnyElement {
        let selected = self
            .usage_ui
            .styles
            .iter()
            .find(|style| style.key == self.settings.output_style)
            .map(|style| style.label.as_str())
            .unwrap_or("Choose style");
        let weak = cx.entity().downgrade();
        let picker = MenuButton::new("output-style-picker")
            .ghost()
            .accessibility_label(format!("Output style: {selected}"))
            .child(format!("Output style · {selected}"))
            .dropdown_menu(move |mut menu, _, cx| {
                let Ok((styles, selected)) = weak.update(cx, |view, _| {
                    (
                        view.usage_ui.styles.clone(),
                        view.settings.output_style.clone(),
                    )
                }) else {
                    return menu;
                };
                for style in styles {
                    let view = weak.clone();
                    let checked = style.key == selected;
                    menu = menu.item(PopupMenuItem::new(style.label).checked(checked).on_click(
                        move |_, _, cx| {
                            let _ = view.update(cx, |view, cx| {
                                view.choose_output_style(style.key.clone(), cx)
                            });
                        },
                    ));
                }
                menu
            });
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(picker)
            .children(
                self.usage_ui
                    .style_error
                    .as_ref()
                    .map(|error| settings_ui::inline_error(error)),
            )
            .into_any_element()
    }

    pub(super) fn choose_output_style(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.usage_ui.styles.iter().any(|style| style.key == key) {
            return;
        }
        self.settings.output_style = key;
        let settings = self.settings.clone();
        self.host_call(cx, |host| host.save_settings(settings));
        cx.notify();
    }
}

#[cfg(test)]
#[path = "usage/tests.rs"]
mod tests;
