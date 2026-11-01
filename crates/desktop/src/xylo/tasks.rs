//! The Scheduled page: the tasks Xylo repeats while it runs, each with its
//! project, cadence and next run, and a form to add one. The sidebar always
//! lists the page; the list refreshes when the page opens and when the agent's
//! schedule tools finish. People enter and read times in their local zone;
//! the stored cron is UTC, converted with the zone's offset when the task is
//! created (a later daylight-saving change moves the local time by an hour).
use super::*;
use crate::icons::{self, AppIcon};
use chrono::{FixedOffset, Local};

/// Weekday names in cron order, Sunday first.
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// The cadences the form offers, in order; the index is `schedule_frequency`.
const FREQUENCIES: [&str; 4] = ["Every day", "Weekdays", "Every Monday", "Every hour"];

impl Xylo {
    pub(super) fn schedules_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let zone = LocalZone::now();
        let has_project = self.workspace.is_some();
        let form_open = has_project && (self.schedule_form_open || self.schedules.is_empty());
        let mut page = div()
            .id("schedule-scroll")
            .size_full()
            .overflow_y_scroll()
            .px(px(24.))
            .pt(px(8.))
            .pb(px(24.));
        let mut column = div().max_w(px(820.)).flex().flex_col().gap(px(12.));
        if !has_project {
            // Tasks run in a project, so without one there is nothing to fill in.
            column = column.child(
                div()
                    .id("schedule-no-project")
                    .test_support()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap(px(10.))
                    .child(subtitle("Open a project to schedule tasks in it.").single_line())
                    .child(
                        secondary("schedule-open-project", "Open project…").on_click(
                            cx.listener(|view, _, window, cx| view.pick_workspace(window, cx)),
                        ),
                    ),
            );
        } else if form_open {
            column = column.child(self.schedule_form(&zone, cx));
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
                list = list.child(self.schedule_row(index, task, &zone, cx));
            }
            column = column.child(list);
        }
        page = page.child(column);
        page.into_any_element()
    }
    fn schedule_form(&self, zone: &LocalZone, cx: &mut Context<Self>) -> AnyElement {
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
                    .child(format!(
                        "Runs in {} while Xylo is open",
                        self.workspace
                            .as_deref()
                            .and_then(Path::file_name)
                            .unwrap_or_default()
                            .to_string_lossy()
                    )),
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
                                            .aria_label(format!("Schedule time ({})", zone.name)),
                                    ),
                                )
                                .child(subtitle(zone.name.clone())),
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
                        .disabled(self.schedule_pending.is_some())
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
        zone: &LocalZone,
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
            // The run's own offset and zone name: a run after a
            // daylight-saving change is not shown in today's offset.
            let at = LocalZone::at(task.next_run);
            format!("next {}", run_label(task.next_run, &at))
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
                                task.wakeup.as_ref().map_or_else(
                                    || cadence_label(&task.cron, zone),
                                    |wakeup| format!(
                                        "{} · {}",
                                        wakeup.reason,
                                        wakeup.interval_seconds.map_or_else(
                                            || "once".into(),
                                            |seconds| format!("every {seconds}s")
                                        )
                                    )
                                )
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
        let zone = LocalZone::now();
        let expression = match schedule_expression(self.schedule_frequency, &time, zone.offset) {
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
/// The viewer's time zone: its current offset and a short name for labels.
pub(super) struct LocalZone {
    offset: FixedOffset,
    name: String,
}
impl LocalZone {
    fn now() -> Self {
        Self::at(chrono::Utc::now())
    }
    /// The local zone as it is at `instant`, which may differ from today's
    /// across a daylight-saving change.
    fn at(instant: chrono::DateTime<chrono::Utc>) -> Self {
        let offset = *instant.with_timezone(&Local).offset();
        let name = zone_abbreviation(instant.timestamp()).unwrap_or_else(|| offset_label(offset));
        Self { offset, name }
    }
}
/// A moment in `zone`'s local time with its zone name: "Nov 3, 09:00 PST".
fn run_label(instant: chrono::DateTime<chrono::Utc>, zone: &LocalZone) -> String {
    format!(
        "{} {}",
        instant.with_timezone(&zone.offset).format("%b %-d, %H:%M"),
        zone.name
    )
}
/// "UTC", or "UTC+05:30" style when the zone has no short name.
fn offset_label(offset: FixedOffset) -> String {
    let seconds = offset.local_minus_utc();
    if seconds == 0 {
        return "UTC".into();
    }
    let sign = if seconds < 0 { '-' } else { '+' };
    let minutes = seconds.unsigned_abs() / 60;
    format!("UTC{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}
#[cfg(unix)]
unsafe extern "C" {
    /// POSIX: reloads the zone rules from TZ (not exported by the libc crate
    /// on every Unix).
    fn tzset();
}
/// The local zone's short name at `timestamp` ("IST", "PST" or "PDT") from
/// the C library. chrono's clock knows only the offset.
#[cfg(unix)]
fn zone_abbreviation(timestamp: i64) -> Option<String> {
    // SAFETY: `tzset` only reloads the zone rules; `localtime_r` writes only
    // to the local passed in;
    // `tm_zone` points into the C library's static zone names, valid for the
    // process lifetime, and is checked for null before reading.
    unsafe {
        let time = libc::time_t::try_from(timestamp).ok()?;
        // Pick up the current TZ setting before converting.
        tzset();
        let mut local: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut local).is_null() || local.tm_zone.is_null() {
            return None;
        }
        let name = std::ffi::CStr::from_ptr(local.tm_zone).to_str().ok()?;
        // Some zones report a numeric "+0530"; the offset label reads better.
        (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphabetic())).then(|| name.to_owned())
    }
}
#[cfg(not(unix))]
fn zone_abbreviation(_timestamp: i64) -> Option<String> {
    None
}
/// The offset in whole minutes east of UTC.
fn offset_minutes(offset: FixedOffset) -> i32 {
    offset.local_minus_utc() / 60
}
/// Moves a cron day field by `shift` days (-1, 0 or 1) for the expressions
/// the form makes: every day, Mon-Fri and a single weekday.
fn shift_days(days: &str, shift: i32) -> Option<String> {
    let index = |name: &str| WEEKDAYS.iter().position(|day| *day == name);
    let moved = |position: usize| WEEKDAYS[(position as i32 + shift).rem_euclid(7) as usize];
    match days {
        "*" => Some("*".into()),
        range if range.contains('-') => {
            let (start, end) = range.split_once('-')?;
            Some(format!("{}-{}", moved(index(start)?), moved(index(end)?)))
        }
        day => Some(moved(index(day)?).to_owned()),
    }
}
/// The stored seven-field UTC cron for a cadence and a local "HH:MM" time,
/// converted with `offset`. Hourly tasks run at the top of each local hour,
/// so only their minute moves.
fn schedule_expression(
    frequency: usize,
    time: &str,
    offset: FixedOffset,
) -> Result<String, String> {
    let offset = offset_minutes(offset);
    if frequency == 3 {
        return Ok(format!("0 {} * * * * *", (-offset).rem_euclid(60)));
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
    let local_days = match frequency {
        0 => "*",
        1 => "Mon-Fri",
        2 => "Mon",
        _ => return Err("Choose how often the task should run.".into()),
    };
    let utc = i32::from(hour) * 60 + i32::from(minute) - offset;
    let days = shift_days(local_days, utc.div_euclid(24 * 60))
        .ok_or_else(|| "Choose how often the task should run.".to_string())?;
    let utc = utc.rem_euclid(24 * 60);
    Ok(format!("0 {} {} * * {days} *", utc % 60, utc / 60))
}
/// How often a schedule runs, in local time, for the expressions the form
/// makes; any other expression shows as written.
fn cadence_label(cron: &str, zone: &LocalZone) -> String {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    let [_, minute, hour, "*", "*", days, _] = fields.as_slice() else {
        return cron.to_owned();
    };
    let Ok(minute) = minute.parse::<i32>() else {
        return cron.to_owned();
    };
    let offset = offset_minutes(zone.offset);
    if *hour == "*" && *days == "*" {
        return format!(
            "{} at :{:02}",
            FREQUENCIES[3].to_lowercase(),
            (minute + offset).rem_euclid(60)
        );
    }
    let Ok(hour) = hour.parse::<i32>() else {
        return cron.to_owned();
    };
    let local = hour * 60 + minute + offset;
    let Some(local_days) = shift_days(days, local.div_euclid(24 * 60)) else {
        return cron.to_owned();
    };
    let cadence = match local_days.as_str() {
        "*" => FREQUENCIES[0],
        "Mon-Fri" => FREQUENCIES[1],
        "Mon" => FREQUENCIES[2],
        _ => return cron.to_owned(),
    };
    let local = local.rem_euclid(24 * 60);
    format!(
        "{} at {:02}:{:02} {}",
        cadence.to_lowercase(),
        local / 60,
        local % 60,
        zone.name
    )
}

#[cfg(test)]
#[path = "tasks/tests.rs"]
mod tests;
