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
                    .child(
                        primary(
                            "create-schedule",
                            if self.schedule_pending.is_some() {
                                "Saving…"
                            } else {
                                "Create task"
                            },
                        )
                        .disabled(self.schedule_pending.is_some())
                        .on_click(
                            cx.listener(|this, _, window, cx| this.create_schedule(window, cx)),
                        ),
                    ),
            )
            .child(button("refresh-schedules", "Refresh").on_click(
                cx.listener(|this, _, _, cx| this.host_call(cx, |h| h.list_schedules())),
            ));
        if !self.schedule_notice.is_empty() {
            page = page.child(subtitle(self.schedule_notice.clone()));
        }
        if self.schedules.is_empty() {
            page = page.child(subtitle("No scheduled tasks yet."));
        }
        for (i, task) in self.schedules.iter().enumerate() {
            let mut paused = task.clone();
            paused.enabled = !paused.enabled;
            let id = task.id;
            page = page.child(
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
                                .disabled(self.schedule_pending.is_some())
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.mutate_schedule(Some(paused.clone()), id, cx)
                                    },
                                )),
                            )
                            .child(
                                button(("delete-schedule", i), "Delete")
                                    .disabled(self.schedule_pending.is_some())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.mutate_schedule(None, id, cx)
                                    })),
                            ),
                    ),
            );
        }
        page.into_any_element()
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
            self.schedule_notice.clear();
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
            self.schedule_notice.clear();
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
        if !self
            .schedule_pending
            .as_ref()
            .is_some_and(|(expected, _)| *expected == id)
        {
            return;
        }
        let (_, submitted) = self.schedule_pending.take().unwrap();
        match result {
            Ok(()) => {
                if submitted
                    .as_ref()
                    .is_some_and(|text| self.schedule_prompt.read(cx).value().as_ref() == text)
                {
                    self.schedule_prompt
                        .update(cx, |input, cx| input.set_value("", window, cx));
                }
                self.schedule_notice = "Saved".into();
            }
            Err(error) => self.schedule_notice = format!("Could not save task: {error}"),
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
