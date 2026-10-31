//! Product view for uncertain effects; no journal IDs or state dumps are shown.
use super::*;
impl Xylo {
    pub(in crate::xylo) fn recovery_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        let pending = self
            .recovery
            .entry()
            .is_some_and(|entry| entry.form.pending.is_some());
        let blocking = self.recovery.blocking_items().next().is_some() || pending;
        let dismissed = self.recovery.dismissed_count();
        if !blocking && dismissed == 0 {
            return div().into_any_element();
        }
        let message = if blocking {
            "An interrupted action needs review before this chat continues. Some changes may remain.".to_owned()
        } else if dismissed == 1 {
            "Continuing without a recorded outcome for an interrupted action. Its changes may remain.".to_owned()
        } else {
            format!(
                "Continuing without a recorded outcome for {dismissed} interrupted actions. Their changes may remain."
            )
        };
        let dismissible = self.recovery.dismissible_items().next().is_some();
        div()
            .w_full()
            .px_6()
            .py_2()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_3()
            .child(div().flex_1().min_w_0().child(notice(
                if blocking {
                    BadgeTone::Neutral
                } else {
                    BadgeTone::Warning
                },
                message,
            )))
            .child(
                button("review-interrupted-actions", "Review interrupted action").on_click(
                    cx.listener(|view, _, _, cx| {
                        view.recovery.open = true;
                        cx.notify();
                    }),
                ),
            )
            .when(blocking && dismissible, |row| {
                row.child(self.recovery_dismiss_button("recovery-banner-dismiss", cx))
            })
            .into_any_element()
    }
    fn recovery_dismiss_button(&self, id: &'static str, cx: &mut Context<Self>) -> AnyElement {
        button(id, "Dismiss and continue")
            .disabled(self.recovery.mutation_busy())
            .on_click(cx.listener(|view, _, window, cx| view.dismiss_stuck_actions(window, cx)))
            .into_any_element()
    }
    pub(in crate::xylo) fn recovery_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.recovery.mutation_busy();
        let mut page = div()
            .id("action-review-panel")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .child(
                        icons::icon_button(
                            "review-back-chat",
                            AppIcon::Back,
                            "Back to conversation",
                        )
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.recovery.open = false;
                            cx.notify();
                        })),
                    )
                    .child(div().flex_1().min_w_0().child(heading(
                        "Review interrupted action",
                        "Inspect the actual target, then record the outcome you observed.",
                    )))
                    .child(
                        icons::icon_button("review-refresh", AppIcon::Refresh, "Refresh actions")
                            .disabled(busy || self.recovery.read_busy())
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.recovery_load_form(window, cx);
                                view.refresh_action_reviews(cx);
                            })),
                    ),
            );
        if !self.recovery.notice.is_empty() {
            page = page.child(notice(BadgeTone::Neutral, self.recovery.notice.clone()));
        }
        if self.recovery.dismissible_items().next().is_some() {
            page = page.child(self.recovery_dismiss_button("review-dismiss-stuck", cx));
        }
        if self.recovery.read_busy() || self.recovery.recording.is_some() {
            page = page.child(
                button("review-cancel-request", "Stop waiting for this request").on_click(
                    cx.listener(|view, _, _, cx| {
                        view.cancel_recovery_flush(cx);
                        view.cancel_review_request(cx);
                    }),
                ),
            );
        }
        let rows = self.recovery.items.clone();
        let mut actions = card()
            .w(px(280.))
            .flex_shrink_0()
            .gap_2()
            .child(sidebar_label("Needs review").px(px(0.)));
        if !self.recovery.loaded {
            actions = actions.child(subtitle("Loading this chat's interrupted actions…"));
        } else if rows.is_empty() {
            actions = actions.child(subtitle("No interrupted actions remain on this page."));
        } else {
            let height = rows.len().min(5) as f32 * 58.;
            actions = actions.child(
                uniform_list(
                    "action-review-list",
                    rows.len(),
                    cx.processor(move |view, range: std::ops::Range<usize>, _window, cx| {
                        range
                            .map(|index| {
                                let action = rows[index].clone();
                                let id = action.id;
                                button(("review-select-action", index), review_label(&action.name))
                                    .h(px(58.))
                                    .flex_col()
                                    .items_start()
                                    .child(subtitle(crate::xylo::memory::memory_age(
                                        action.updated_at,
                                    )))
                                    .w_full()
                                    .justify_start()
                                    .text_sm()
                                    .disabled(view.recovery.mutation_busy())
                                    .when(
                                        view.recovery
                                            .entry()
                                            .is_some_and(|entry| entry.form.selected == Some(id)),
                                        |button| button.bg(rgb(lavender())),
                                    )
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        view.review_select(id, window, cx)
                                    }))
                                    .into_any_element()
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .h(px(height))
                .w_full(),
            );
        }
        if self.recovery.has_more {
            actions = actions.child(
                button("review-next-page", "Earlier actions")
                    .disabled(busy || self.recovery.read_busy())
                    .on_click(cx.listener(|view, _, _, cx| view.more_action_reviews(cx))),
            );
        }
        if self.recovery.requested_cursor.is_some() {
            actions = actions.child(
                button("review-first-page", "Back to newest actions")
                    .disabled(busy || self.recovery.read_busy())
                    .on_click(cx.listener(|view, _, _, cx| view.refresh_action_reviews(cx))),
            );
        }
        let mut detail = card().flex_1().min_w(px(320.)).gap_3().items_start();
        if let Some(saved) = self.recovery.detail.as_ref() {
            detail = detail
                .child(div().text_lg().child(review_label(&saved.action.name)))
                .child(subtitle(
                    "An interruption or cancellation does not show whether its effects completed.",
                ));
            if saved.action.busy {
                detail = detail.child(subtitle("This action is still running. Wait until it has stopped before recording an outcome. If it stays running, you can dismiss it and continue."));
            } else if saved.action.blocked_reason.is_some() {
                detail = detail.child(subtitle("Verify this action in the service where it ran. Xylo cannot confirm that it has stopped, so recording an outcome here is unavailable. You can dismiss it and continue."));
            }
            if let Some(target) = review_target(saved) {
                detail = detail.child(div().text_sm().child(target));
            }
            if let Some(path) = review_target_path(saved) {
                detail = detail.child(button("review-open-target", "Open target file").on_click(
                    cx.listener(move |view, _, _, cx| {
                        view.request_editor_file(path.clone(), None, false, cx)
                    }),
                ));
            }
            detail = detail.child(subtitle("Retained observation")).child(
                Textarea::new(&self.recovery.output)
                    .w_full()
                    .readonly(true)
                    .aria_label("Retained action observation"),
            );
            if saved
                .result
                .as_ref()
                .is_some_and(|payload| payload.truncated)
            {
                detail = detail.child(subtitle("The saved observation is larger than this preview. The original is retained; inspect the target and listed files."));
            }
            for (index, path) in saved.artifacts.iter().take(64).enumerate() {
                let path = path.clone();
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                detail = detail.child(button(("review-artifact", index), name).on_click(
                    cx.listener(move |view, _, _, cx| {
                        view.request_editor_file(path.clone(), None, false, cx)
                    }),
                ));
            }
            if saved.artifacts_truncated {
                detail = detail.child(subtitle(
                    "Additional output files are retained with the original action.",
                ));
            }
            if let Some(receipt) = &saved.resolution {
                detail = detail.child(subtitle(match receipt.outcome {
                    ActionResolutionOutcome::Completed => "You recorded that the action completed. Execution output remains unverified.",
                    ActionResolutionOutcome::DidNotComplete => "You recorded that the action did not complete. Partial effects may remain.",
                })).child(div().text_sm().child(receipt.evidence.clone()));
            }
        } else {
            detail = detail.child(subtitle(
                "Choose an interrupted action to inspect its target and saved observation.",
            ));
        }
        if let Some(entry) = self.recovery.entry() {
            if let Some(error) = &entry.error {
                detail = detail.child(notice(BadgeTone::Danger, error.clone()));
            }
            if entry.conflict.is_some() {
                detail = detail
                    .child(subtitle(
                        "A saved review differs from this note. Your current note stays here.",
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                button("review-use-saved", "Use saved review")
                                    .disabled(busy || entry.form.pending.is_some())
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.choose_saved_recovery_form(true, window, cx)
                                    })),
                            )
                            .child(
                                button("review-keep-note", "Keep my note")
                                    .disabled(busy || entry.form.pending.is_some())
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.choose_saved_recovery_form(false, window, cx)
                                    })),
                            ),
                    );
            }
            if let Some(pending) = &entry.form.pending {
                detail = detail.child(subtitle("Your observed outcome is saved as a pending request. Check its recorded result before making another observation."))
                    .child(div().flex().flex_wrap().gap_2()
                        .child(button("review-check-outcome", "Check recorded outcome").disabled(busy || self.recovery.read_busy())
                            .on_click(cx.listener(|view, _, _, cx| view.check_observed_outcome(cx))))
                        .child(button("review-save-original-outcome", "Save this observed outcome")
                            .disabled(busy || entry.dirty || entry.save.is_some() || entry.conflict.is_some() || self.running() || self.recovery.detail.as_ref().is_none_or(|detail| detail.action.busy || detail.action.blocked_reason.is_some() || detail.action.state != "outcome_unknown"))
                            .on_click(cx.listener(|view, _, _, cx| view.dispatch_observed_outcome(cx)))))
                    .child(subtitle(match pending.outcome { ActionResolutionOutcome::Completed => "Pending observation: completed.",
                        ActionResolutionOutcome::DidNotComplete => "Pending observation: did not complete; partial effects may remain." }));
            }
            if entry.form.selected.is_some() {
                detail = detail
                    .child(subtitle(
                        "What did you observe after inspecting the target?",
                    ))
                    .child(
                        div().flex().flex_wrap().gap_2().children(
                            [
                                (ActionResolutionOutcome::Completed, "Completed"),
                                (ActionResolutionOutcome::DidNotComplete, "Did not complete"),
                            ]
                            .into_iter()
                            .enumerate()
                            .map(|(index, (outcome, label))| {
                                button(("review-observed-outcome", index), label)
                                    .disabled(
                                        busy || entry.form.pending.is_some()
                                            || entry.revision.is_none(),
                                    )
                                    .when(entry.form.outcome == Some(outcome), |button| {
                                        button.bg(rgb(lavender()))
                                    })
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        let Some(scope) = view.recovery.scope.clone() else {
                                            return;
                                        };
                                        let entry =
                                            view.recovery.entries.get_mut(&key(&scope)).unwrap();
                                        if entry.form.outcome == Some(outcome) {
                                            return;
                                        }
                                        entry.form.outcome = Some(outcome);
                                        entry.dirty = true;
                                        entry.generation = entry.generation.saturating_add(1);
                                        view.recovery_cancel_close_intents(cx);
                                        view.persist_recovery_forms(cx);
                                    }))
                            }),
                        ),
                    )
                    .child(
                        Textarea::new(&self.recovery.note)
                            .w_full()
                            .disabled(entry.revision.is_none())
                            .aria_label("Observed outcome evidence note"),
                    )
                    .child(subtitle(if entry.dirty || entry.save.is_some() {
                        "Saving your review note…"
                    } else {
                        "Your review note is saved on this computer."
                    }))
                    .child(
                        button("review-save-note", "Save review note")
                            .disabled(busy || entry.save.is_some())
                            .on_click(
                                cx.listener(|view, _, _, cx| view.persist_recovery_forms(cx)),
                            ),
                    );
                if entry.form.pending.is_none()
                    && self
                        .recovery
                        .detail
                        .as_ref()
                        .is_some_and(|detail| detail.resolution.is_none())
                {
                    detail = detail.child(
                        primary("review-record-outcome", "Record observed outcome")
                            .disabled(
                                busy || self.running()
                                    || entry.conflict.is_some()
                                    || entry.form.evidence.trim().is_empty()
                                    || entry.form.evidence.len() > 4096
                                    || entry.form.outcome.is_none()
                                    || self.recovery.detail.as_ref().is_some_and(|detail| {
                                        detail.action.busy
                                            || detail.action.blocked_reason.is_some()
                                            || detail.action.state != "outcome_unknown"
                                    }),
                            )
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.record_observed_outcome(window, cx)
                            })),
                    );
                }
            }
        }
        page.child(
            div()
                .flex()
                .flex_wrap()
                .gap_4()
                .items_start()
                .child(actions)
                .child(detail),
        )
        .into_any_element()
    }
}
fn review_target(detail: &ActionReviewDetail) -> Option<String> {
    let value = detail.arguments.json.as_ref()?;
    if let Some(path) = value.get("path").and_then(serde_json::Value::as_str) {
        return Some(format!("Target file: {}", truncate(path, 1024)));
    }
    if let Some(command) = value.get("command") {
        let command = command.as_str().map(str::to_owned).or_else(|| {
            command
                .as_array()
                .map(|items| shell_words::join(items.iter().filter_map(serde_json::Value::as_str)))
        })?;
        return Some(format!("Command: {}", truncate(&command, 2048)));
    }
    if let Some(url) = value.get("url").and_then(serde_json::Value::as_str) {
        let host = url
            .split_once("://")
            .map_or(url, |(_, rest)| rest)
            .split(['/', '?', '#'])
            .next()
            .unwrap_or("")
            .rsplit('@')
            .next()
            .unwrap_or("");
        return Some(format!("Connected site: {}", truncate(host, 256)));
    }
    None
}
fn review_target_path(detail: &ActionReviewDetail) -> Option<PathBuf> {
    let path = PathBuf::from(detail.arguments.json.as_ref()?.get("path")?.as_str()?);
    Some(if path.is_absolute() {
        path
    } else {
        detail.scope.workspace.join(path)
    })
}
