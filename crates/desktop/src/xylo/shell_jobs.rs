//! Live shell jobs in the work panel, with a bounded output tail and Stop.
use super::*;

impl Xylo {
    pub(super) fn tool_command_job(&self, tool: &crate::state::ToolCard) -> Option<&ShellJob> {
        let jobs = &self.turns.get(&self.session)?.shell_jobs;
        if let Some(job) = jobs.iter().find(|job| job.action_id == Some(tool.id)) {
            return Some(job);
        }
        let parsed = tool.presentation.parsed_output.as_deref();
        let job_id = parsed
            .and_then(|value| value.get("summary").unwrap_or(value).get("job_id"))
            .and_then(serde_json::Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())?;
        self.turns
            .get(&self.session)?
            .shell_jobs
            .iter()
            .find(|job| job.id == job_id)
    }
    pub(super) fn command_tail(&self, job: &ShellJob, cx: &Context<Self>) -> AnyElement {
        let id = job.id;
        let label = if job.running {
            "Running command"
        } else if job.exit_code == Some(0) {
            "Command finished"
        } else if job.exit_code.is_some() {
            "Command failed"
        } else {
            "Command stopped"
        };
        let tail = job
            .output
            .lines()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        let copy = job.command.clone();
        div()
            .id(SharedString::from(format!("command-tail-{id}")))
            .test_support()
            .w_full()
            .mt(px(16.))
            .mb(px(18.))
            .rounded(px(12.))
            .border_1()
            .border_color(rgb(border_strong()))
            .bg(rgb(panel()))
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(14.))
                    .py(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(9.))
                    .border_b_1()
                    .border_color(rgb(border()))
                    .text_size(px(13.))
                    .child(icons::small(AppIcon::Terminal).size(px(17.)))
                    .child(div().flex_1().font_weight(FontWeight::MEDIUM).child(label))
                    .when(job.running, |row| {
                        row.child(crate::motion::spinner(px(14.)).color(rgb(muted()).into()))
                    })
                    .when(job.exit_code == Some(0), |row| {
                        row.child(icons::small(AppIcon::Done).text_color(rgb(success())))
                    }),
            )
            .child(
                div()
                    .px(px(16.))
                    .py(px(9.))
                    .bg(rgb(bg()))
                    .text_size(px(12.))
                    .line_height(relative(1.5))
                    .font_family(mono(cx))
                    .text_color(rgb(muted()))
                    .overflow_hidden()
                    .child(job.command.clone()),
            )
            .child(
                div()
                    .id(SharedString::from(format!("command-live-output-{id}")))
                    .max_h(px(158.))
                    .overflow_scroll()
                    .px(px(16.))
                    .py(px(13.))
                    .text_size(px(12.5))
                    .line_height(relative(1.8))
                    .font_family(mono(cx))
                    .text_color(rgb(text_secondary()))
                    .whitespace_nowrap()
                    .child(tail),
            )
            .child(
                div()
                    .px(px(8.))
                    .py(px(5.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .border_t_1()
                    .border_color(rgb(border()))
                    .child(
                        button(
                            SharedString::from(format!("command-output-{id}")),
                            "Open output",
                        )
                        .h(px(34.))
                        .text_size(px(12.))
                        .font_weight(FontWeight::NORMAL)
                        .text_color(rgb(text_secondary()))
                        .on_click(
                            cx.listener(move |view, _, _, cx| view.open_command_output(id, cx)),
                        ),
                    )
                    .child(
                        button(
                            SharedString::from(format!("command-copy-{id}")),
                            "Copy command",
                        )
                        .h(px(34.))
                        .text_size(px(12.))
                        .font_weight(FontWeight::NORMAL)
                        .text_color(rgb(text_secondary()))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                        }),
                    )
                    .child(div().flex_1())
                    .when(job.running, |row| {
                        row.child(
                            button(
                                SharedString::from(format!("command-stop-{id}")),
                                "Stop command",
                            )
                            .h(px(34.))
                            .text_size(px(12.))
                            .font_weight(FontWeight::NORMAL)
                            .text_color(rgb(text_secondary()))
                            .on_click(cx.listener(
                                move |view, _, _, cx| {
                                    view.engine_command(WorkerCommand::KillJob { job_id: id }, cx)
                                },
                            )),
                        )
                    }),
            )
            .into_any_element()
    }

    pub(super) fn shell_jobs_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let jobs = self
            .turns
            .get(&self.session)
            .map(|turn| turn.shell_jobs.as_slice())
            .unwrap_or_default();
        let mut list = div()
            .id("shell-jobs-panel")
            .size_full()
            .overflow_y_scroll()
            .p_3()
            .flex()
            .flex_col()
            .gap_3();
        if jobs.is_empty() {
            return list
                .child(subtitle(
                    "Background commands appear here with their output.",
                ))
                .into_any_element();
        }
        for (index, job) in jobs.iter().rev().enumerate() {
            let id = job.id;
            let status = if job.running {
                "Running".to_owned()
            } else if let Some(error) = &job.error {
                error.clone()
            } else {
                format!(
                    "Exited {}",
                    job.exit_code
                        .map_or_else(|| "after a signal".into(), |code| code.to_string())
                )
            };
            let mut heading = div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().flex_1().min_w_0().child(job.command.clone()));
            if job.running {
                heading = heading.child(
                    focus_ring(gpui_kit::base::Button::new(("stop-shell-job", index)))
                        .accessibility_label("Stop shell job")
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .child("Stop")
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.engine_command(WorkerCommand::KillJob { job_id: id }, cx)
                        })),
                );
            }
            list = list.child(
                div()
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(well()))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(heading)
                    .child(subtitle(status))
                    .child(
                        secondary(("open-command-output", index), "View full output").on_click(
                            cx.listener(move |view, _, _, cx| view.open_command_output(id, cx)),
                        ),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(muted()))
                            .child(job.cwd.display().to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_family("monospace")
                            .child(job.output.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(muted()))
                            .child(format!("Full log: {}", job.log_path.display())),
                    ),
            );
        }
        list.into_any_element()
    }
}
