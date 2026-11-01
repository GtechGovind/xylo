//! Live shell jobs in the work panel, with a bounded output tail and Stop.
use super::*;

impl Xylo {
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
