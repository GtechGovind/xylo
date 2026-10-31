//! The quit flow. Quitting asks first when tasks are running or waiting for
//! approval, then saves unsaved editors, drafts, memory and document edits.
//! Saving can stall (the service stopped, a save failed or conflicts); the
//! waiting banner then names what is unsaved and offers Quit anyway, so the
//! user can always leave. Once shutdown starts the window shows that tasks are
//! being stopped until the host reports it has finished.
use super::*;

/// What still has to be saved before Xylo closes, in product words.
fn unsaved_kinds(drafts: bool, memory: bool, documents: bool, recovery: bool) -> Vec<&'static str> {
    [
        (drafts, "message drafts"),
        (memory, "memory edits"),
        (documents, "document edits"),
        (recovery, "action review notes"),
    ]
    .into_iter()
    .filter_map(|(pending, label)| pending.then_some(label))
    .collect()
}

/// The prompt detail for quitting with work in progress.
fn running_tasks_detail(count: usize) -> String {
    if count == 1 {
        "1 task is still running and will be stopped.".into()
    } else {
        format!("{count} tasks are still running and will be stopped.")
    }
}

impl Xylo {
    /// Chats with a running turn or an action waiting for approval.
    fn active_task_count(&self) -> usize {
        self.turns
            .values()
            .filter(|turn| turn.running || turn.pending_approvals().next().is_some())
            .count()
    }

    pub(crate) fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.updates.installing
            || self.quitting
            || self.quit_prompt_open
            || self.quit_after_save.is_some()
        {
            return;
        }
        if self.recovery.mutation_busy() {
            self.error = Some("Wait for the observed outcome request, or stop waiting and check its result, before closing.".into());
            cx.notify();
            return;
        }
        if !self.management_check_close(DraftFlushAction::Quit, window, cx) {
            return;
        }
        let active = self.active_task_count();
        if active > 0 && !self.quit_tasks_confirmed {
            self.confirm_quit_with_tasks(active, window, cx);
            return;
        }
        self.quit_tasks_confirmed = false;
        if !self.editor_dirty {
            self.begin_shutdown(cx);
            return;
        }
        window.activate_window();
        cx.activate(true);
        self.quit_prompt_open = true;
        let filename = self
            .editor_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "your file".into());
        let detail = format!("Your changes to {filename} haven't been saved.");
        let answer = window.prompt(
            PromptLevel::Warning,
            "Save changes before quitting?",
            Some(&detail),
            &[
                PromptButton::ok("Save"),
                PromptButton::new("Discard"),
                PromptButton::cancel("Cancel"),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let answer = answer.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.quit_prompt_open = false;
                match answer {
                    Ok(0) => this.request_editor_save(true, window, cx),
                    Ok(1) => this.begin_shutdown(cx),
                    _ => cx.notify(),
                }
            });
        })
        .detach();
    }

    fn confirm_quit_with_tasks(
        &mut self,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.activate_window();
        cx.activate(true);
        self.quit_prompt_open = true;
        let detail = running_tasks_detail(count);
        let answer = window.prompt(
            PromptLevel::Warning,
            "Quit Xylo?",
            Some(&detail),
            &[
                PromptButton::ok("Quit anyway"),
                PromptButton::cancel("Cancel"),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let answer = answer.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.quit_prompt_open = false;
                if matches!(answer, Ok(0)) {
                    this.quit_tasks_confirmed = true;
                    this.request_quit(window, cx);
                } else {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Saves pending work, then shuts down. Flush owners call this again when
    /// their save completes; a stalled save leaves the waiting banner up.
    pub(super) fn begin_shutdown(&mut self, cx: &mut Context<Self>) {
        if self.management_busy() || self.management_dirty(cx) {
            self.error = Some(
                "Finish or discard your plugin and environment setup before closing Xylo.".into(),
            );
            cx.notify();
            return;
        }
        if !self.flush_drafts(DraftFlushAction::Quit, cx)
            || !self.memory_forms_flush(DraftFlushAction::Quit, cx)
            || !self.document_drafts_flush(DraftFlushAction::Quit, cx)
            || !self.recovery_flush(DraftFlushAction::Quit, cx)
        {
            self.quit_waiting_for_saves = true;
            cx.notify();
            return;
        }
        self.shutdown_now(cx);
    }

    /// Leaves without the pending saves the user chose to give up.
    fn quit_anyway(&mut self, cx: &mut Context<Self>) {
        self.cancel_setup_close_intents(cx);
        self.shutdown_now(cx);
    }

    fn keep_working(&mut self, cx: &mut Context<Self>) {
        self.quit_waiting_for_saves = false;
        self.cancel_setup_close_intents(cx);
    }

    fn shutdown_now(&mut self, cx: &mut Context<Self>) {
        self.quit_waiting_for_saves = false;
        for (_, (terminal, _)) in std::mem::take(&mut self.terminal_cache) {
            terminal.update(cx, |terminal, cx| terminal.close_all(cx));
        }
        self.stop_speech(cx);
        self.cancel_project_search(cx);
        self.stop_asset_poll();
        self.detach_editor_assist();
        if self.quitting {
            return;
        }
        self.quitting = true;
        if !self.host_running() || self.host.as_ref().is_none_or(|h| h.shutdown().is_err()) {
            cx.quit();
            return;
        }
        // The host answers with `Stopped` once workers have stopped; the
        // deadline bounds a host that never answers.
        let deadline = cx
            .background_executor()
            .timer(std::time::Duration::from_secs(30));
        cx.spawn(async move |_, cx| {
            deadline.await;
            cx.update(|cx| cx.quit());
        })
        .detach();
        cx.notify();
    }

    fn saves_pending(&self) -> bool {
        self.draft_flush_pending()
            || self.memory_forms_flush_pending()
            || self.document_drafts_flush_pending()
            || self.recovery_flush_pending()
    }

    /// Banner while saves run before quitting or installing an update. A quit
    /// whose saves were all cancelled elsewhere stops waiting here.
    pub(super) fn pending_saves_banner(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.saves_pending() {
            self.quit_waiting_for_saves = false;
            return None;
        }
        let memory = self.memory_forms_flush_pending();
        let documents = self.document_drafts_flush_pending();
        let unsaved = unsaved_kinds(
            self.draft_flush_pending(),
            memory,
            documents,
            self.recovery_flush_pending(),
        );
        let quitting = self.quit_waiting_for_saves;
        let headline = if !self.host_running() {
            "Xylo's background service stopped, so this work can't be saved."
        } else if quitting {
            "Saving your work before closing…"
        } else {
            "Saving your work before installing the update…"
        };
        Some(
            div()
                .px_5()
                .py_2()
                .flex_shrink_0()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_3()
                .child(
                    div().flex().flex_col().child(subtitle(headline)).child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(muted()))
                            .child(format!("Not saved yet: {}", unsaved.join(", "))),
                    ),
                )
                .child(
                    button("pending-save-keep-working", "Keep working")
                        .on_click(cx.listener(|view, _, _, cx| view.keep_working(cx))),
                )
                .when(memory, |row| {
                    row.child(
                        button("pending-save-memory", "Review Memory").on_click(
                            cx.listener(|view, _, _, cx| view.navigate(Page::Memory, cx)),
                        ),
                    )
                })
                .when(documents, |row| {
                    row.child(
                        button("pending-save-documents", "Review documents").on_click(
                            cx.listener(|view, _, _, cx| view.navigate(Page::Artifacts, cx)),
                        ),
                    )
                })
                .when(quitting, |row| {
                    row.child(
                        secondary("quit-anyway", "Quit anyway")
                            .text_color(rgb(danger()))
                            .on_click(cx.listener(|view, _, _, cx| view.quit_anyway(cx))),
                    )
                })
                .into_any_element(),
        )
    }

    /// Full-window state while running tasks are being stopped.
    pub(super) fn quitting_screen(&self) -> AnyElement {
        div()
            .size_full()
            .bg(rgb(bg()))
            .text_color(rgb(text()))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .child(crate::motion::spinner(icons::LARGE).color(rgb(muted()).into()))
            .child(empty_state(
                "Finishing your tasks…",
                "Xylo closes as soon as running tasks have stopped.",
            ))
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "quit/tests.rs"]
mod tests;
