//! The Scheduled page: the tasks Xylo repeats while it runs, each with its
//! project, cadence and next run, and a form to add one. The sidebar always
//! lists the page; the list refreshes when the page opens and when the agent's
//! schedule tools finish. Times are UTC, as stored.
use super::*;
use crate::icons::{self, AppIcon};

/// The cadences the form offers, in order; the index is `schedule_frequency`.
const FREQUENCIES: [&str; 4] = ["Every day", "Weekdays", "Every Monday", "Every hour"];

impl Xylo {
    pub(super) fn schedules_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let form_open = self.schedule_form_open || self.schedules.is_empty();
        let mut page = div()
            .id("schedule-scroll")
            .size_full()
            .overflow_y_scroll()
            .px(px(24.))
            .pt(px(8.))
            .pb(px(24.));
        let mut column = div().max_w(px(820.)).flex().flex_col().gap(px(12.));
        if form_open {
            column = column.child(self.schedule_form(cx));
        } else {
            column = column.child(secondary("new-schedule", "New task").self_start().on_click(
                cx.listener(|view, _, _, cx| {
                    view.schedule_form_open = true;
                    cx.notify();
                }),
            ));
        }
        if !self.schedules.is_empty() {
            let mut list = div()
                .id("schedule-list")
                .flex()
                .flex_col()
                .rounded(px(12.))
                .border_1()
                .border_color(rgb(border()))
                .bg(rgb(panel()));
            for (index, task) in self.schedules.iter().enumerate() {
                if index > 0 {
                    list = list.child(divider());
                }
                list = list.child(self.schedule_row(index, task, cx));
            }
            column = column.child(list);
        }
        page = page.child(column);
        page.into_any_element()
    }
    fn schedule_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut frequency = segments().self_start();
        for (index, label) in FREQUENCIES.into_iter().enumerate() {
            frequency = frequency.child(
                segment(
                    ("schedule-frequency", index),
                    label,
                    self.schedule_frequency == index,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.schedule_frequency = index;
                    cx.notify();
                })),
            );
        }
        let can_cancel = !self.schedules.is_empty();
        card()
            .id("schedule-form")
            .test_support()
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(rgb(muted()))
                    .single_line()
                    .child(match self.workspace.as_deref() {
                        Some(root) => format!(
                            "Runs in {} while Xylo is open",
                            root.file_name()
                                .unwrap_or(root.as_os_str())
                                .to_string_lossy()
                        ),
                        None => "Open a project to schedule a task in it".into(),
                    }),
            )
            .child(Textarea::new(&self.schedule_prompt))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(12.))
                    .child(frequency)
                    .when(self.schedule_frequency != 3, |row| {
                        row.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .child(subtitle("at"))
                                .child(
                                    div().w(px(88.)).child(
                                        Input::new(&self.schedule_time)
                                            .aria_label("Schedule time (UTC)"),
                                    ),
                                )
                                .child(subtitle("UTC")),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .when(can_cancel, |row| {
                        row.child(button("cancel-schedule", "Cancel").on_click(cx.listener(
                            |view, _, _, cx| {
                                view.schedule_form_open = false;
                                cx.notify();
                            },
                        )))
                    })
                    .child(
                        crate::motion::busy_button(
                            primary("create-schedule", "Create task"),
                            self.schedule_pending.is_some(),
                        )
                        .disabled(self.schedule_pending.is_some() || self.workspace.is_none())
                        .on_click(
                            cx.listener(|this, _, window, cx| this.create_schedule(window, cx)),
                        ),
                    ),
            )
            .into_any_element()
    }
    fn schedule_row(
        &self,
        index: usize,
        task: &xylo_runtime::scheduler::ScheduledTask,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut toggled = task.clone();
        toggled.enabled = !toggled.enabled;
        let id = task.id;
        let project = task
            .workspace
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Project".into());
        let when = if task.enabled {
            format!("next {} UTC", task.next_run.format("%b %-d, %H:%M"))
        } else {
            "paused".into()
        };
        let busy = self.schedule_pending.is_some();
        div()
            .id(("schedule-row", index))
            .px(px(14.))
            .py(px(10.))
            .flex()
            .items_center()
            .gap(px(12.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .single_line()
                            .text_size(px(13.5))
                            .text_color(rgb(if task.enabled { text() } else { muted() }))
                            .child(task.prompt.lines().next().unwrap_or_default().to_owned()),
                    )
                    .child(
                        div()
                            .single_line()
                            .text_size(px(12.))
                            .text_color(rgb(muted()))
                            .child(format!(
                                "{project} · {} · {when}",
                                cadence_label(&task.cron)
                            )),
                    ),
            )
            .child(
                button(
                    ("pause-schedule", index),
                    if task.enabled { "Pause" } else { "Resume" },
                )
                .text_size(px(12.5))
                .disabled(busy)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.mutate_schedule(Some(toggled.clone()), id, cx)
                })),
            )
            .child(
                icons::icon_button(("delete-schedule", index), AppIcon::Delete, "Delete task")
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_delete_schedule(id, window, cx)
                    })),
            )
            .into_any_element()
    }
    /// Deleting a schedule cannot be undone, so it asks first.
    fn confirm_delete_schedule(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            "Delete this scheduled task?",
            Some("It stops running. Chats it already started stay."),
            &[
                PromptButton::cancel("Keep task"),
                PromptButton::new("Delete task"),
            ],
            cx,
        );
        cx.spawn(async move |view, cx| {
            if matches!(answer.await, Ok(1)) {
                let _ = view.update(cx, |view, cx| view.mutate_schedule(None, id, cx));
            }
        })
        .detach();
    }
    /// Asks the host for the current schedules; the answer replaces the list.
    pub(super) fn refresh_schedules(&mut self, cx: &mut Context<Self>) {
        self.host_call(cx, |host| host.list_schedules());
    }
    fn create_schedule(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.schedule_pending.is_some() {
            return;
        }
        let Some(workspace) = self.workspace.clone() else {
            self.error = Some("Choose a project folder first.".into());
            cx.notify();
            return;
        };
        let prompt = self.schedule_prompt.read(cx).value().trim().to_string();
        if prompt.is_empty() {
            self.error = Some("Describe the task you want to schedule.".into());
            cx.notify();
            return;
        }
        let time = self.schedule_time.read(cx).value().to_string();
        let expression = match schedule_expression(self.schedule_frequency, &time) {
            Ok(value) => value,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let request_id = Uuid::new_v4();
        let submitted = self.schedule_prompt.read(cx).value().to_string();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|h| h.create_schedule_tracked(request_id, prompt, expression, workspace));
        if result.is_ok() {
            self.schedule_pending = Some((request_id, Some(submitted)));
        }
        self.result(result, cx);
    }
    fn mutate_schedule(
        &mut self,
        task: Option<xylo_runtime::scheduler::ScheduledTask>,
        id: Uuid,
        cx: &mut Context<Self>,
    ) {
        if self.schedule_pending.is_some() {
            return;
        }
        let request_id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| match task {
                Some(task) => host.save_schedule_tracked(request_id, task),
                None => host.remove_schedule_tracked(request_id, id),
            });
        if result.is_ok() {
            self.schedule_pending = Some((request_id, None));
        }
        self.result(result, cx);
    }
    pub(super) fn schedule_mutated(
        &mut self,
        id: Uuid,
        result: Result<(), String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, submitted)) = self
            .schedule_pending
            .take_if(|(expected, _)| *expected == id)
        else {
            return;
        };
        match result {
            Ok(()) => {
                if let Some(text) = submitted {
                    // Newer typing stays in the open form.
                    if self.schedule_prompt.read(cx).value().as_ref() == text {
                        self.schedule_prompt
                            .update(cx, |input, cx| input.set_value("", window, cx));
                        self.schedule_form_open = false;
                    }
                    self.status = "Scheduled task created".into();
                }
            }
            Err(error) => {
                tracing::warn!(detail = %error, "Schedule change failed");
                self.error = Some("The scheduled task could not be saved. Try again.".into());
            }
        }
        cx.notify();
    }
}
fn schedule_expression(frequency: usize, time: &str) -> Result<String, String> {
    if frequency == 3 {
        return Ok("0 0 * * * * *".into());
    }
    let time = if time.trim().is_empty() {
        "09:00"
    } else {
        time.trim()
    };
    let Some((hour, minute)) = time.split_once(':') else {
        return Err("Enter a time such as 09:00.".into());
    };
    let (hour, minute) = (
        hour.parse::<u8>()
            .map_err(|_| "Enter a valid hour.".to_string())?,
        minute
            .parse::<u8>()
            .map_err(|_| "Enter a valid minute.".to_string())?,
    );
    if hour > 23 || minute > 59 {
        return Err("Time must be between 00:00 and 23:59.".into());
    }
    let days = match frequency {
        0 => "*",
        1 => "Mon-Fri",
        2 => "Mon",
        _ => return Err("Choose how often the task should run.".into()),
    };
    Ok(format!("0 {minute} {hour} * * {days} *"))
}
/// How often a schedule runs, in words, for the expressions the form makes;
/// any other expression shows as written.
fn cadence_label(cron: &str) -> String {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    let [_, minute, hour, "*", "*", days, _] = fields.as_slice() else {
        return cron.to_owned();
    };
    if *minute == "0" && *hour == "*" && *days == "*" {
        return FREQUENCIES[3].to_lowercase();
    }
    let (Ok(hour), Ok(minute)) = (hour.parse::<u8>(), minute.parse::<u8>()) else {
        return cron.to_owned();
    };
    let cadence = match *days {
        "*" => FREQUENCIES[0],
        "Mon-Fri" => FREQUENCIES[1],
        "Mon" => FREQUENCIES[2],
        _ => return cron.to_owned(),
    };
    format!("{} at {hour:02}:{minute:02}", cadence.to_lowercase())
}

#[cfg(test)]
#[path = "tasks/tests.rs"]
mod tests;
