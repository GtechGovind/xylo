//! Native setup and lifecycle controls. Connection labels come from confirmed
//! probes/actions, not from rendering or the presence of a saved profile.
use super::*;
impl Xylo {
    fn environment_operation_row(
        &self,
        index: usize,
        operation: &EnvironmentReceipt,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = operation.operation_id;
        let name = self
            .environments
            .records
            .iter()
            .find(|record| record.profile.id == operation.environment_id)
            .map(|record| record.profile.name.as_str())
            .unwrap_or("Environment");
        div()
            .h(px(76.))
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().truncate().text_sm().child(format!(
                "{} · {}",
                name,
                action_progress(operation.action)
            )))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        button(("environment-check-operation", index), "Check result")
                            .disabled(self.environments.busy())
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.environment_check_operation(id, cx)
                            })),
                    )
                    .child(
                        button(
                            ("environment-record-operation", index),
                            "Record observed result",
                        )
                        .disabled(
                            self.environments.busy()
                                || operation.state != EnvironmentOperationState::OutcomeUnknown
                                || !self.environments.checked_operations.contains(&id),
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.environments.resolving = Some(id);
                            cx.notify();
                        })),
                    ),
            )
            .into_any_element()
    }
    pub(in super::super) fn environments_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.environments.busy();
        let form = &self.environments.form;
        let mut page = div()
            .w_full()
            .max_w(px(760.))
            .flex()
            .flex_col()
            .gap_5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().text_size(px(12.)).text_color(rgb(muted())).child("SSH and container connections"))
                    .child(
                        primary("environment-new", "New environment")
                            .disabled(busy)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.environment_select(None, window, cx)
                            })),
                    )
                    .child(
                        button("environment-refresh", "Refresh profiles")
                            .disabled(busy)
                            .on_click(cx.listener(|view, _, _, cx| view.refresh_environments(cx))),
                    ),
            );
        if !self.environments.notice.is_empty() {
            page = page.child(subtitle(self.environments.notice.clone()));
        }
        if !self.environments.pending.is_empty() {
            page = page.child(
                button("environment-cancel", "Cancel request")
                    .on_click(cx.listener(|view, _, _, cx| view.environment_cancel(cx))),
            );
        }
        let rows = self.environments.records.clone();
        let mut saved = div().w_full().flex().flex_col().gap_2()
            .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).child("Connections"));
        if !self.environments.loaded {
            saved = saved.child(subtitle("Loading saved profiles…"));
        } else if rows.is_empty() {
            saved = saved.child(subtitle("No environments saved yet."));
        } else {
            saved = saved.child(
                uniform_list(
                    "environment-list",
                    rows.len(),
                    cx.processor(move |view, range: std::ops::Range<usize>, _window, cx| {
                        range
                            .map(|index| {
                                let record = rows[index].clone();
                                let id = record.profile.id;
                                let kind = match record.profile.target {
                                    EnvironmentTarget::Ssh { .. } => "SSH",
                                    EnvironmentTarget::Container {
                                        runtime: ContainerRuntime::Docker,
                                        ..
                                    } => "Docker",
                                    EnvironmentTarget::Container {
                                        runtime: ContainerRuntime::Podman,
                                        ..
                                    } => "Podman",
                                };
                                button(
                                    ("environment-select", index),
                                    format!("{} · {kind}", record.profile.name),
                                )
                                .w_full()
                                .justify_start()
                                .disabled(view.environments.busy())
                                .when(view.environments.selected == Some(id), |button| {
                                    button.bg(rgb(lavender()))
                                })
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.environment_select(Some(id), window, cx)
                                }))
                                .into_any_element()
                            })
                            .collect()
                    }),
                )
                .h(px(160.)),
            );
        }
        page = page.child(saved);
        let mut setup = div().w_full().max_w(px(560.)).flex().flex_col().gap_3()
            .pt_4().border_t_1().border_color(rgb(border()))
            .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child(
            if form.original.is_some() {
                "Edit environment"
            } else {
                "New environment"
            },
        ));
        setup = setup.child(environment_field("Name", &form.name));
        setup = setup.child(
            div().flex().gap_2().children(
                [Kind::Ssh, Kind::Docker, Kind::Podman]
                    .into_iter()
                    .enumerate()
                    .map(|(index, kind)| {
                        button(("environment-kind", index), kind.label())
                            .disabled(busy)
                            .when(form.kind == kind, |button| button.bg(rgb(lavender())))
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.environments.form.kind = kind;
                                view.environments.form.client = None;
                                view.environment_form_changed(cx);
                            }))
                    }),
            ),
        );
        let client = form.client.as_ref().or_else(|| {
            self.environments
                .clients
                .iter()
                .find(|client| client.runtime == form.kind.runtime() && client.available)
                .map(|client| &client.executable)
        });
        setup = setup.child(
            div()
                .flex()
                .gap_2()
                .items_center()
                .child(subtitle(if client.is_some() {
                    format!("{} client available", form.kind.label())
                } else {
                    format!("{} client not found", form.kind.label())
                }))
                .child(
                    button("environment-client", "Choose client application")
                        .disabled(busy)
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.environment_pick(Asset::Client, window, cx)
                        })),
                ),
        );
        match form.kind {
            Kind::Ssh => {
                setup = setup.child(environment_field("Host", &form.host)).child(
                    div()
                        .flex()
                        .gap_3()
                        .child(div().flex_1().child(environment_field("User", &form.user)))
                        .child(
                            div()
                                .w(px(100.))
                                .child(environment_field("Port", &form.port)),
                        ),
                );
                let known = form
                    .known_hosts
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Not selected".into());
                let identity = form
                    .identity
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Use SSH agent".into());
                setup = setup
                    .child(div().flex().gap_2().items_center()
                        .child(subtitle(format!("Verified hosts: {known}")))
                        .child(button("environment-known-hosts", "Choose verified hosts file").disabled(busy)
                            .on_click(cx.listener(|view, _, window, cx| view.environment_pick(Asset::KnownHosts, window, cx)))))
                    .child(div().flex().gap_2().items_center()
                        .child(subtitle(format!("Identity: {identity}")))
                        .child(button("environment-identity", "Choose identity file").disabled(busy)
                            .on_click(cx.listener(|view, _, window, cx| view.environment_pick(Asset::Identity, window, cx))))
                        .child(button("environment-use-agent", "Use SSH agent").disabled(busy || form.identity.is_none())
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.environments.form.identity = None;
                                view.environment_form_changed(cx);
                            }))))
                    .child(subtitle("Use a host already verified in your SSH hosts file. Connections use your SSH agent or selected identity; no password is stored here."));
            }
            Kind::Docker | Kind::Podman => {
                setup = setup.child(
                    div().flex().gap_2().children(
                        [(false, "Existing container"), (true, "Xylo container")]
                            .into_iter()
                            .enumerate()
                            .map(|(index, (managed, label))| {
                                button(("environment-container-mode", index), label)
                                    .disabled(busy)
                                    .when(form.managed == managed, |button| {
                                        button.bg(rgb(lavender()))
                                    })
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        view.environments.form.managed = managed;
                                        view.environment_form_changed(cx);
                                    }))
                            }),
                    ),
                );
                if form.managed {
                    setup = setup
                        .child(environment_field("Image", &form.image))
                        .child(environment_field(
                            "Startup command (optional)",
                            &form.startup,
                        ))
                        .child(subtitle(
                            "Save the profile, then create its container with an explicit action.",
                        ));
                } else {
                    setup = setup.child(environment_field("Container name or ID", &form.target));
                }
            }
        }
        if form.conflict {
            setup = setup
                .child(subtitle(
                    "This saved setup changed elsewhere. Reload it before saving or connecting.",
                ))
                .child(
                    button("environment-reload-saved", "Reload saved setup")
                        .disabled(busy)
                        .on_click(cx.listener(|view, _, window, cx| {
                            let id = view.environments.selected;
                            view.environment_select(id, window, cx);
                        })),
                );
        }
        setup = setup.child(
            primary("environment-save", "Save profile")
                .disabled(busy || form.conflict || self.environments.unresolved(form.id))
                .on_click(cx.listener(|view, _, _, cx| view.environment_save(cx))),
        );
        if form.dirty {
            setup = setup.child(subtitle("Setup changes have not been saved."));
        }
        page = page.child(setup);
        if let Some(record) = self.environments.selected_record() {
            let id = record.profile.id;
            let blocked = busy || form.dirty || form.conflict || self.environments.unresolved(id);
            let status = self
                .environments
                .statuses
                .get(&id)
                .filter(|status| status.environment.revision == record.revision);
            let status_label = status
                .map(|status| match status.state {
                    EnvironmentState::Connected => "Connected",
                    EnvironmentState::Disconnected => "Disconnected",
                    EnvironmentState::Stopped => "Stopped",
                    EnvironmentState::Missing => "Not found",
                    EnvironmentState::OutcomeUnknown => "Result needs checking",
                    EnvironmentState::Unavailable => "Unavailable",
                })
                .unwrap_or("Status needs checking");
            let mut controls = card()
                .max_w(px(860.))
                .gap_3()
                .child(div().text_lg().child(record.profile.name.clone()))
                .child(subtitle(status_label))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(
                            primary("environment-connect", "Connect")
                                .disabled(blocked)
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.environment_action(EnvironmentAction::Connect, cx)
                                })),
                        )
                        .child(
                            button("environment-disconnect", "Disconnect")
                                .disabled(blocked)
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.environment_action(EnvironmentAction::Disconnect, cx)
                                })),
                        )
                        .child(
                            button("environment-status", "Check saved connection")
                                .disabled(busy)
                                .on_click(
                                    cx.listener(|view, _, _, cx| view.environment_status(cx)),
                                ),
                        ),
                );
            if matches!(
                record.profile.target,
                EnvironmentTarget::Container {
                    container: ContainerTarget::Managed { .. },
                    ..
                }
            ) {
                controls = controls.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(
                            button("environment-create", "Create container")
                                .disabled(blocked || record.container_id.is_some())
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.environment_action(EnvironmentAction::Create, cx)
                                })),
                        )
                        .child(
                            button("environment-start", "Start container")
                                .disabled(blocked || record.container_id.is_none())
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.environment_action(EnvironmentAction::Start, cx)
                                })),
                        )
                        .child(
                            button("environment-stop", "Stop container")
                                .disabled(blocked || record.container_id.is_none())
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.environment_action(EnvironmentAction::Stop, cx)
                                })),
                        )
                        .child(
                            button("environment-remove-container", "Remove container")
                                .disabled(
                                    blocked || record.container_id.is_none() || record.connected,
                                )
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.environment_confirm_remove(true, window, cx)
                                })),
                        ),
                );
            }
            controls = controls
                .child(environment_field("Command", &self.environments.command))
                .child(
                    primary("environment-run", "Run command")
                        .disabled(blocked || !self.environments.connected(record))
                        .on_click(cx.listener(|view, _, _, cx| view.environment_execute(cx))),
                )
                .child(
                    button("environment-saved-results", "Saved command results")
                        .disabled(busy)
                        .on_click(cx.listener(|view, _, _, cx| view.environment_saved_results(cx))),
                )
                .child(
                    button("environment-remove", "Remove profile")
                        .disabled(blocked || record.connected || record.container_id.is_some())
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.environment_confirm_remove(false, window, cx)
                        })),
                );
            if record.connected || record.container_id.is_some() {
                controls = controls.child(subtitle(
                    "Disconnect and remove any Xylo container before removing its profile.",
                ));
            }
            page = page.child(controls);
        }
        if self.environments.history_for.is_some()
            && self.environments.history_for == self.environments.selected
        {
            let mut saved = card()
                .max_w(px(860.))
                .gap_2()
                .child(div().text_lg().child("Saved command results"));
            let loading = self
                .environments
                .pending
                .values()
                .any(|request| matches!(request.kind, RequestKind::History(_)));
            if loading {
                saved = saved.child(subtitle("Loading saved results…"));
            } else if !self.environments.history_loaded {
                saved = saved.child(subtitle(
                    "Saved results could not be loaded. Choose Saved command results to try again.",
                ));
            } else if self.environments.history.is_empty() {
                saved = saved.child(subtitle(
                    "No saved command results are available for this environment.",
                ));
            } else {
                let history = self.environments.history.clone();
                let height = history.len().min(5) as f32 * 70.;
                saved = saved.child(
                    uniform_list(
                        "environment-saved-results-list",
                        history.len(),
                        cx.processor(move |view, range: std::ops::Range<usize>, _window, cx| {
                            range
                                .map(|index| {
                                    let receipt = &history[index];
                                    let id = receipt.operation_id;
                                    let label =
                                        receipt.command_label.as_deref().unwrap_or("Saved command");
                                    let state = match receipt.state {
                                        EnvironmentOperationState::Succeeded => "Completed",
                                        EnvironmentOperationState::Failed => "Failed",
                                        EnvironmentOperationState::Cancelled => "Cancelled",
                                        EnvironmentOperationState::Prepared
                                        | EnvironmentOperationState::Running => "In progress",
                                        EnvironmentOperationState::OutcomeUnknown => {
                                            "Result needs checking"
                                        }
                                    };
                                    let time = receipt
                                        .created_at_ms
                                        .map(crate::xylo::memory::memory_age)
                                        .unwrap_or_else(|| "Saved earlier".into());
                                    button(("environment-open-saved", index), label.to_owned())
                                        .w_full()
                                        .h(px(70.))
                                        .justify_start()
                                        .items_start()
                                        .flex_col()
                                        .child(subtitle(format!("{state} · {time}")))
                                        .disabled(view.environments.busy())
                                        .on_click(cx.listener(move |view, _, _, cx| {
                                            view.environment_open_saved_result(id, cx)
                                        }))
                                        .into_any_element()
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .w_full()
                    .h(px(height)),
                );
            }
            page = page.child(saved);
        }
        if !self.environments.operations.is_empty() {
            let mut actions = card().max_w(px(860.)).gap_2().child(
                div()
                    .text_lg()
                    .child("Previous actions needing a result check"),
            );
            let operations = self
                .environments
                .operations
                .values()
                .cloned()
                .collect::<Vec<_>>();
            let height = (operations.len().min(4) as f32) * 76.;
            actions = actions.child(
                uniform_list(
                    "environment-operations",
                    operations.len(),
                    cx.processor(move |view, range: std::ops::Range<usize>, _window, cx| {
                        range
                            .map(|index| {
                                view.environment_operation_row(index, &operations[index], cx)
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .h(px(height))
                .w_full(),
            );
            if self.environments.resolving.is_some() {
                actions = actions
                    .child(
                        Textarea::new(&self.environments.resolution_note)
                            .aria_label("Observed environment result"),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                button("environment-resolve-succeeded", "Action completed")
                                    .disabled(busy)
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.environment_resolve(true, window, cx)
                                    })),
                            )
                            .child(
                                button("environment-resolve-failed", "Action did not complete")
                                    .disabled(busy)
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.environment_resolve(false, window, cx)
                                    })),
                            )
                            .child(
                                button("environment-resolve-cancel", "Cancel")
                                    .disabled(busy)
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.environments.resolving = None;
                                        cx.notify();
                                    })),
                            ),
                    );
            }
            page = page.child(actions);
        }
        if let Some(output) = &self.environments.output_details {
            let mut saved = card()
                .max_w(px(860.))
                .gap_2()
                .child(
                    div()
                        .text_lg()
                        .id("environment-output-heading")
                        .child("Output preview"),
                )
                .child(subtitle(format!(
                    "From {}",
                    self.environments.output_environment
                )));
            if output.truncated {
                saved = saved.child(
                    subtitle("The preview shows only the beginning of this command's output.")
                        .id("environment-output-shortened"),
                );
            }
            if !self.environments.output.read(cx).value().is_empty() {
                saved = saved.child(
                    Textarea::new(&self.environments.output)
                        .readonly(true)
                        .aria_label("Environment command output preview"),
                );
            }
            if output.artifacts.is_empty() {
                saved = saved.child(subtitle(
                    "No separate output files are available for this result.",
                ));
            }
            for (index, artifact) in output.artifacts.iter().enumerate() {
                let stream = match artifact.stream {
                    EnvironmentOutputStream::Stdout => "standard output",
                    EnvironmentOutputStream::Stderr => "standard error",
                };
                let path = artifact.path.clone();
                saved = saved.child(
                    div()
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap_1()
                        .child(
                            button(("environment-open-output", index), format!("Open {stream}"))
                                .on_click(
                                    cx.listener(move |_, _, _, cx| cx.open_with_system(&path)),
                                ),
                        )
                        .child(subtitle(environment_capture_description(artifact))),
                );
            }
            page = page.child(saved);
        }
        div().id("environments-page").size_full().overflow_y_scroll()
            .px(px(45.)).py(px(38.)).flex().justify_center().child(page).into_any_element()
    }
}
fn environment_capture_description(artifact: &EnvironmentOutputArtifact) -> String {
    if artifact.complete {
        return format!("Complete saved stream · {} bytes", artifact.stored_bytes);
    }
    let reason = match artifact.reason {
        EnvironmentCaptureReason::Eof => "Only part of this stream was saved",
        EnvironmentCaptureReason::Cancelled => "Capture stopped after cancellation",
        EnvironmentCaptureReason::TimedOut => "Capture stopped at the time limit",
        EnvironmentCaptureReason::ByteLimit => "Saved output reached its size limit",
        EnvironmentCaptureReason::ReadError => "Capture stopped after a read problem",
        EnvironmentCaptureReason::WriteError => "Capture stopped after a save problem",
        EnvironmentCaptureReason::Interrupted => "Recovered partial output",
    };
    format!("{reason} · {} bytes saved", artifact.stored_bytes)
}
fn environment_field(label: &'static str, input: &Entity<InputState>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(subtitle(label))
        .child(Input::new(input).aria_label(label))
}
