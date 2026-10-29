use super::*;
impl Xylo {
    pub(super) fn schedules_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let project = self
            .workspace
            .as_ref()
            .map(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_else(|| "Choose a project before scheduling a task".into());
        let mut frequency = div().flex().gap_2();
        for (index, label) in ["Every day", "Weekdays", "Every Monday", "Every hour"]
            .into_iter()
            .enumerate()
        {
            frequency = frequency.child(
                button(("schedule-frequency", index), label)
                    .bg(rgb(if self.schedule_frequency == index {
                        lavender()
                    } else {
                        bg()
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.schedule_frequency = index;
                        cx.notify();
                    })),
            );
        }
        let mut page = div()
            .id("schedule-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(heading(
                "Scheduled Tasks",
                "Give your assistant a task to repeat while Xylo is running.",
            ))
            .child(
                card()
                    .max_w(px(820.))
                    .child(subtitle(project))
                    .child(Textarea::new(&self.schedule_prompt))
                    .child(frequency)
                    .when(self.schedule_frequency != 3, |d| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(subtitle("Time (UTC)"))
                                .child(
                                    div().w(px(140.)).child(
                                        Input::new(&self.schedule_time)
                                            .aria_label("Schedule time (UTC)"),
                                    ),
                                ),
                        )
                    })
                    .child(primary("create-schedule", "Create task").on_click(
                        cx.listener(|this, _, window, cx| this.create_schedule(window, cx)),
                    )),
            )
            .child(button("refresh-schedules", "Refresh").on_click(
                cx.listener(|this, _, _, cx| this.host_call(cx, |h| h.list_schedules())),
            ));
        if self.schedules.is_empty() {
            page = page.child(subtitle("No scheduled tasks yet."));
        }
        for (i, task) in self.schedules.iter().enumerate() {
            let mut paused = task.clone();
            paused.enabled = !paused.enabled;
            let id = task.id;
            page =
                page.child(
                    card()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child(task.prompt.clone()),
                        )
                        .child(subtitle(format!(
                            "{} · Next run {} UTC",
                            if task.enabled { "Enabled" } else { "Paused" },
                            task.next_run.format("%b %d, %H:%M")
                        )))
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(
                                    button(
                                        ("pause-schedule", i),
                                        if task.enabled { "Pause" } else { "Resume" },
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.host_call(cx, |h| h.save_schedule(paused.clone()))
                                        },
                                    )),
                                )
                                .child(button(("delete-schedule", i), "Delete").on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.host_call(cx, |h| h.remove_schedule(id))
                                    }),
                                )),
                        ),
                );
        }
        page.into_any_element()
    }
    fn create_schedule(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|h| h.create_schedule(prompt, expression, workspace));
        if result.is_ok() {
            self.schedule_prompt
                .update(cx, |s, cx| s.set_value("", window, cx));
        }
        self.result(result, cx);
    }
    pub(super) fn teams_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut page = div()
            .id("team-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(heading(
                "Teams",
                "Follow tasks your assistant has shared with teammates.",
            ))
            .child(
                button("refresh-teams", "Refresh")
                    .on_click(cx.listener(|this, _, _, cx| this.host_call(cx, |h| h.team_tasks()))),
            );
        if self.team_tasks.is_empty() {
            page = page.child(
                card()
                    .child(div().text_sm().child("No team tasks yet."))
                    .child(subtitle(
                        "Ask your assistant to work on a task with teammates.",
                    )),
            );
        }
        for (i, task) in self.team_tasks.iter().enumerate() {
            page = page.child(
                card()
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(task.title.clone()),
                            )
                            .child(subtitle(task.status.clone())),
                    )
                    .when(!task.summary.is_empty(), |d| {
                        d.child(
                            TextView::markdown(("team-summary", i), truncate(&task.summary, 6000))
                                .selectable(true),
                        )
                    }),
            );
        }
        page.into_any_element()
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
#[cfg(test)]
mod tests {
    use super::schedule_expression;
    #[test]
    fn schedules_use_valid_utc_fields() {
        assert_eq!(
            schedule_expression(1, "18:45").unwrap(),
            "0 45 18 * * Mon-Fri *"
        );
        assert!(schedule_expression(0, "24:00").is_err());
        assert_eq!(schedule_expression(3, "").unwrap(), "0 0 * * * * *");
    }
}
