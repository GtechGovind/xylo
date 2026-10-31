//! The Environments settings page: the saved list, and one sub-page per
//! environment (or a new one) with its setup form, connection, container,
//! command and recovery controls. Connection labels come from confirmed
//! probes/actions, not from rendering or the presence of a saved profile.
use super::*;
use crate::motion;
use settings_ui::{Feedback, SelectOption, Tone};

impl Xylo {
    pub(in super::super) fn environments_page(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.environments.editing {
            return self.environment_detail_page(cx);
        }
        let busy = self.environments.busy();
        let empty = self.environments.loaded && self.environments.records.is_empty();
        let mut page = settings_ui::settings_page("environments-page", "Environments").description(
            "Let the agent run commands on a remote machine or in a container instead of this Mac.",
        );
        if !empty {
            page = page.action(
                settings_ui::primary_button("environment-new", "New environment")
                    .disabled(busy)
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.environment_new(Kind::Ssh, window, cx)
                    })),
            );
        }
        page = page.children(self.environment_status_notices(cx));
        if !self.environments.loaded {
            page = page.child(settings_ui::loading_list(
                3,
                self.environments.waiting_since,
            ));
        } else if empty {
            page = page
                .child(settings_ui::empty_state(
                    AppIcon::Environment,
                    "No environments yet",
                    "Pick SSH, Docker or Podman below to set one up.",
                    None,
                ))
                .child(self.environment_use_cases(cx));
        } else {
            page = page.child(settings_ui::section(
                "Saved environments",
                None,
                self.environment_list(cx),
            ));
        }
        page.children(self.environment_operations(cx))
            .into_any_element()
    }
    /// The page-wide request notice and the Cancel control for a pending
    /// request; both pages show them so a long request can always be stopped.
    fn environment_status_notices(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut notices = Vec::new();
        if !self.environments.notice.is_empty() {
            notices.push(
                notice(BadgeTone::Neutral, self.environments.notice.clone()).into_any_element(),
            );
        }
        let watched = self.environments.waiting_for(|kind| {
            matches!(
                kind,
                RequestKind::Status(_)
                    | RequestKind::Receipt {
                        action: EnvironmentAction::Connect,
                        ..
                    }
            )
        });
        if watched
            && let Some(slow) = self
                .environments
                .waiting_since
                .and_then(settings_ui::slow_caption)
        {
            notices.push(slow.into_any_element());
        }
        if !self.environments.pending.is_empty() {
            notices.push(
                div()
                    .child(
                        settings_ui::secondary_button("environment-cancel", "Cancel request")
                            .on_click(cx.listener(|view, _, _, cx| view.environment_cancel(cx))),
                    )
                    .into_any_element(),
            );
        }
        notices
    }
    /// The three kinds of environment, offered while none is saved.
    fn environment_use_cases(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.environments.busy();
        let items = [
            (Kind::Ssh, "Remote server (SSH)", AppIcon::Environment),
            (Kind::Docker, "Docker container", AppIcon::Container),
            (Kind::Podman, "Podman container", AppIcon::Container),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (kind, title, glyph))| {
            settings_ui::list_item(
                ("environment-use-case", index),
                Some(icon_tile(glyph).into_any_element()),
                title,
                Some(kind.helper().into()),
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                if !busy {
                    view.environment_new(kind, window, cx)
                }
            }))
            .into_any_element()
        });
        settings_ui::section(
            "Get started",
            Some("Choose where the agent's commands should run.".into()),
            settings_ui::list_container(items),
        )
        .into_any_element()
    }
    fn environment_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.environments.busy();
        let items = self
            .environments
            .records
            .iter()
            .enumerate()
            .map(|(index, record)| {
                let id = record.profile.id;
                let (pill, subtitle) = self.environment_test_summary(record);
                let mut item = settings_ui::list_item(
                    ("environment-select", index),
                    Some(
                        settings_ui::monogram(&record.profile.name, &id.to_string())
                            .into_any_element(),
                    ),
                    record.profile.name.clone(),
                    Some(subtitle),
                )
                .on_click(
                    cx.listener(move |view, _, window, cx| view.environment_open(id, window, cx)),
                );
                if let Some(at) = self.environments.tests.get(&id).map(|(_, _, at)| *at) {
                    item = item.hover_detail(format!("Tested {}", tested_age(at)));
                }
                item.trailing(pill)
                    .trailing(
                        settings_ui::busy(
                            settings_ui::text_button(("environment-test", index), "Test")
                                .disabled(busy),
                            self.environments.testing.is_some()
                                && self.environments.selected == Some(id),
                        )
                        .on_click(cx.listener(
                            move |view, _, window, cx| view.environment_test(id, window, cx),
                        )),
                    )
                    .into_any_element()
            });
        settings_ui::list_container(items).into_any_element()
    }
    /// The status pill and subtitle of a saved environment's list item: the
    /// target, or the last test failure in its place. The pill fades when its
    /// wording changes.
    fn environment_test_summary(&self, record: &EnvironmentRecord) -> (AnyElement, SharedString) {
        let id = record.profile.id;
        let target: SharedString = environment_target(&record.profile.target).into();
        let testing = self.environments.testing.is_some() && self.environments.selected == Some(id);
        let ((label, tone), subtitle) = match self.environments.tests.get(&id) {
            _ if testing => (("Checking…", Tone::Neutral), target),
            Some((true, _, _)) => (("Test passed", Tone::Success), target),
            Some((false, message, _)) => (("Test failed", Tone::Danger), message.clone().into()),
            None => (("Not tested", Tone::Neutral), target),
        };
        let pill = motion::fade_on_change(
            format!("environment-test-pill-{id}"),
            &label,
            settings_ui::pill(label, tone),
        );
        (pill.into_any_element(), subtitle)
    }
    /// One environment's page: a new environment's setup, or a saved one's
    /// connection, container, command and setup sections.
    fn environment_detail_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.environments.busy();
        let form = &self.environments.form;
        let record = self.environments.selected_record();
        let name = form
            .original
            .as_ref()
            .map(|record| record.profile.name.clone())
            .unwrap_or_else(|| "New environment".into());
        let trail = settings_ui::breadcrumb(
            [settings_ui::crumb("environment-back", "Environments")
                .disabled(busy)
                .on_click(
                    cx.listener(|view, _, window, cx| view.environment_back_to_list(window, cx)),
                )],
            name.clone(),
        );
        let mut page = settings_ui::settings_page("environments-page", name)
            .description(form.kind.helper())
            .breadcrumb(trail)
            .children(self.environment_status_notices(cx));
        if let Some(record) = record {
            let index = self
                .environments
                .records
                .iter()
                .position(|saved| saved.profile.id == record.profile.id)
                .unwrap_or_default();
            page = page
                .child(self.environment_connection(record, cx))
                .child(self.environment_command(record, cx))
                .children(self.environment_saved_results_section(cx))
                .children(self.environment_output(cx))
                .children(self.environment_operations(cx))
                .child(settings_ui::section(
                    "Setup",
                    None,
                    self.environment_form(cx),
                ))
                .child(self.environment_danger_zone(index, record, cx));
        } else {
            page = page
                .child(self.environment_form(cx))
                .children(self.environment_operations(cx));
        }
        page.into_any_element()
    }
    fn environment_connection(
        &self,
        record: &EnvironmentRecord,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = record.profile.id;
        let busy = self.environments.busy();
        let form = &self.environments.form;
        let blocked = busy || form.dirty || form.conflict || self.environments.unresolved(id);
        let status = self
            .environments
            .statuses
            .get(&id)
            .filter(|status| status.environment.revision == record.revision)
            .map(|status| match status.state {
                EnvironmentState::Connected => ("Connected", Tone::Success),
                EnvironmentState::Disconnected => ("Disconnected", Tone::Neutral),
                EnvironmentState::Stopped => ("Stopped", Tone::Neutral),
                EnvironmentState::Missing => ("Not found", Tone::Danger),
                EnvironmentState::OutcomeUnknown => ("Result needs checking", Tone::Warning),
                EnvironmentState::Unavailable => ("Unavailable", Tone::Danger),
            })
            .unwrap_or(("Status needs checking", Tone::Neutral));
        let test = self.environments.tests.get(&id).map(|(ok, message, at)| {
            if *ok {
                Feedback::Saved(format!("Test passed {}.", tested_age(*at)).into())
            } else {
                Feedback::Error(format!("Test failed {}: {message}", tested_age(*at)).into())
            }
        });
        let mut rows = vec![
            settings_ui::setting_row(
                "Status",
                Some(environment_target(&record.profile.target).into()),
                Some(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(motion::fade_on_change(
                            format!("environment-status-{id}"),
                            &status.0,
                            settings_ui::pill(status.0, status.1),
                        ))
                        .child(
                            settings_ui::busy(
                                settings_ui::text_button("environment-status", "Check")
                                    .disabled(busy),
                                self.environments
                                    .waiting_for(|kind| matches!(kind, RequestKind::Status(_))),
                            )
                            .on_click(cx.listener(|view, _, _, cx| view.environment_status(cx))),
                        )
                        .into_any_element(),
                ),
            )
            .feedback(test)
            .into_any_element(),
            settings_ui::setting_row(
                "Connection",
                Some("Test the saved setup, or keep a connection open for the agent.".into()),
                Some(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            settings_ui::busy(
                                settings_ui::text_button("environment-disconnect", "Disconnect")
                                    .disabled(blocked),
                                self.environments
                                    .waiting_for_action(EnvironmentAction::Disconnect),
                            )
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.environment_action(EnvironmentAction::Disconnect, cx);
                            })),
                        )
                        .child(
                            settings_ui::busy(
                                settings_ui::secondary_button("environment-connect", "Connect")
                                    .disabled(blocked),
                                self.environments.testing.is_none()
                                    && self
                                        .environments
                                        .waiting_for_action(EnvironmentAction::Connect),
                            )
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.environment_action(EnvironmentAction::Connect, cx);
                            })),
                        )
                        .into_any_element(),
                ),
            )
            .into_any_element(),
        ];
        if matches!(
            record.profile.target,
            EnvironmentTarget::Container {
                container: ContainerTarget::Managed { .. },
                ..
            }
        ) {
            let exists = record.container_id.is_some();
            rows.push(
                settings_ui::setting_row(
                    "Container",
                    Some(
                        if exists {
                            "Xylo created this environment's container."
                        } else {
                            "Create the container from the saved image before connecting."
                        }
                        .into(),
                    ),
                    Some(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                settings_ui::busy(
                                    settings_ui::text_button("environment-create", "Create")
                                        .disabled(blocked || exists),
                                    self.environments
                                        .waiting_for_action(EnvironmentAction::Create),
                                )
                                .on_click(cx.listener(
                                    |view, _, _, cx| {
                                        view.environment_action(EnvironmentAction::Create, cx);
                                    },
                                )),
                            )
                            .child(
                                settings_ui::busy(
                                    settings_ui::text_button("environment-start", "Start")
                                        .disabled(blocked || !exists),
                                    self.environments
                                        .waiting_for_action(EnvironmentAction::Start),
                                )
                                .on_click(cx.listener(
                                    |view, _, _, cx| {
                                        view.environment_action(EnvironmentAction::Start, cx);
                                    },
                                )),
                            )
                            .child(
                                settings_ui::busy(
                                    settings_ui::text_button("environment-stop", "Stop")
                                        .disabled(blocked || !exists),
                                    self.environments
                                        .waiting_for_action(EnvironmentAction::Stop),
                                )
                                .on_click(cx.listener(
                                    |view, _, _, cx| {
                                        view.environment_action(EnvironmentAction::Stop, cx);
                                    },
                                )),
                            )
                            .into_any_element(),
                    ),
                )
                .into_any_element(),
            );
        }
        settings_ui::section("Connection", None, settings_ui::rows(rows)).into_any_element()
    }
    fn environment_command(
        &self,
        record: &EnvironmentRecord,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.environments.busy();
        let form = &self.environments.form;
        let blocked =
            busy || form.dirty || form.conflict || self.environments.unresolved(record.profile.id);
        settings_ui::section(
            "Run a command",
            Some("Runs in this environment, not on this Mac, once it is connected.".into()),
            settings_ui::form_column([
                settings_ui::field(
                    "Command",
                    Input::new(&self.environments.command).aria_label("Command"),
                    None,
                )
                .into_any_element(),
                settings_ui::form_actions([
                    settings_ui::busy(
                        settings_ui::text_button(
                            "environment-saved-results",
                            "Saved command results",
                        )
                        .disabled(busy),
                        self.environments
                            .waiting_for(|kind| matches!(kind, RequestKind::History(_))),
                    )
                    .on_click(cx.listener(|view, _, _, cx| view.environment_saved_results(cx)))
                    .into_any_element(),
                    settings_ui::busy(
                        settings_ui::secondary_button("environment-run", "Run command")
                            .disabled(blocked || !self.environments.connected(record)),
                        self.environments
                            .waiting_for_action(EnvironmentAction::Execute),
                    )
                    .on_click(cx.listener(|view, _, _, cx| view.environment_execute(cx)))
                    .into_any_element(),
                ])
                .into_any_element(),
            ]),
        )
        .into_any_element()
    }
    fn environment_saved_results_section(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.environments.history_for.is_none()
            || self.environments.history_for != self.environments.selected
        {
            return None;
        }
        let loading = self
            .environments
            .pending
            .values()
            .any(|request| matches!(request.kind, RequestKind::History(_)));
        let body = if loading {
            settings_ui::loading_list(2, self.environments.waiting_since).into_any_element()
        } else if !self.environments.history_loaded {
            settings_ui::inline_error(
                "Saved results could not be loaded. Choose Saved command results to try again.",
            )
            .into_any_element()
        } else if self.environments.history.is_empty() {
            settings_ui::caption("No saved command results for this environment yet.")
                .into_any_element()
        } else {
            let history = self.environments.history.clone();
            let height = history.len().min(5) as f32 * settings_ui::LIST_ITEM_HEIGHT;
            settings_ui::list_frame()
                .child(
                    uniform_list(
                        "environment-saved-results-list",
                        history.len(),
                        cx.processor(move |_, range: std::ops::Range<usize>, _window, cx| {
                            range
                                .map(|index| {
                                    let receipt = &history[index];
                                    let id = receipt.operation_id;
                                    let label = receipt
                                        .command_label
                                        .clone()
                                        .unwrap_or_else(|| "Saved command".into());
                                    let time = receipt
                                        .created_at_ms
                                        .map(crate::xylo::memory::memory_age)
                                        .unwrap_or_else(|| "Saved earlier".into());
                                    settings_ui::list_item(
                                        ("environment-open-saved", index),
                                        None,
                                        label,
                                        Some(
                                            format!("{} · {time}", operation_state(receipt.state))
                                                .into(),
                                        ),
                                    )
                                    .divider(index > 0)
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        if !view.environments.busy() {
                                            view.environment_open_saved_result(id, cx)
                                        }
                                    }))
                                    .into_any_element()
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .w_full()
                    .h(px(height)),
                )
                .into_any_element()
        };
        Some(settings_ui::section("Saved command results", None, body).into_any_element())
    }
    fn environment_output(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let output = self.environments.output_details.as_ref()?;
        let mut body = div().flex().flex_col().gap(px(12.));
        if output.truncated {
            body = body.child(div().id("environment-output-shortened").child(
                settings_ui::caption(
                    "The preview shows only the beginning of this command's output.",
                ),
            ));
        }
        if !self.environments.output.read(cx).value().is_empty() {
            body = body.child(
                Textarea::new(&self.environments.output)
                    .readonly(true)
                    .aria_label("Environment command output preview"),
            );
        }
        let files = output
            .artifacts
            .iter()
            .enumerate()
            .map(|(index, artifact)| {
                let stream = match artifact.stream {
                    EnvironmentOutputStream::Stdout => "Standard output",
                    EnvironmentOutputStream::Stderr => "Standard error",
                };
                let path = artifact.path.clone();
                settings_ui::setting_row(
                    stream,
                    Some(environment_capture_description(artifact).into()),
                    Some(
                        settings_ui::text_button(("environment-open-output", index), "Open")
                            .accessibility_label(format!("Open {}", stream.to_lowercase()))
                            .on_click(cx.listener(move |_, _, _, cx| cx.open_with_system(&path)))
                            .into_any_element(),
                    ),
                )
                .into_any_element()
            });
        body = if output.artifacts.is_empty() {
            body.child(settings_ui::caption(
                "No separate output files are available for this result.",
            ))
        } else {
            body.child(settings_ui::rows(files))
        };
        Some(
            div()
                .id("environment-output-heading")
                .child(settings_ui::section(
                    "Output preview",
                    Some(format!("From {}", self.environments.output_environment).into()),
                    body,
                ))
                .into_any_element(),
        )
    }
    /// Actions whose outcome is unknown, with the controls to check them or
    /// record what the person observed.
    fn environment_operations(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.environments.operations.is_empty() {
            return None;
        }
        let busy = self.environments.busy();
        let operations = self
            .environments
            .operations
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let height = operations.len().min(4) as f32 * settings_ui::LIST_ITEM_HEIGHT;
        let mut body = div().flex().flex_col().gap(px(12.)).child(
            settings_ui::list_frame().child(
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
            ),
        );
        if self.environments.resolving.is_some() {
            body =
                body.child(settings_ui::form_column([
                    settings_ui::field(
                        "What you observed",
                        Textarea::new(&self.environments.resolution_note)
                            .aria_label("Observed environment result"),
                        None,
                    )
                    .into_any_element(),
                    settings_ui::form_actions([
                        settings_ui::text_button("environment-resolve-cancel", "Cancel")
                            .disabled(busy)
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.environments.resolving = None;
                                cx.notify();
                            }))
                            .into_any_element(),
                        settings_ui::secondary_button(
                            "environment-resolve-failed",
                            "Action did not complete",
                        )
                        .disabled(busy)
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.environment_resolve(false, window, cx)
                        }))
                        .into_any_element(),
                        settings_ui::secondary_button(
                            "environment-resolve-succeeded",
                            "Action completed",
                        )
                        .disabled(busy)
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.environment_resolve(true, window, cx)
                        }))
                        .into_any_element(),
                    ])
                    .into_any_element(),
                ]));
        }
        Some(
            settings_ui::section(
                "Actions needing a result check",
                Some(
                    "Xylo couldn't confirm how these ended. Check each one before trying again."
                        .into(),
                ),
                body,
            )
            .into_any_element(),
        )
    }
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
        settings_ui::list_item(
            ("environment-operation", index),
            None,
            name.to_owned(),
            Some(action_progress(operation.action).into()),
        )
        .divider(index > 0)
        .trailing(
            settings_ui::text_button(("environment-check-operation", index), "Check result")
                .disabled(self.environments.busy())
                .on_click(
                    cx.listener(move |view, _, _, cx| view.environment_check_operation(id, cx)),
                ),
        )
        .trailing(
            settings_ui::text_button(("environment-record-operation", index), "Record result")
                .disabled(
                    self.environments.busy()
                        || operation.state != EnvironmentOperationState::OutcomeUnknown
                        || !self.environments.checked_operations.contains(&id),
                )
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.environments.resolving = Some(id);
                    cx.notify();
                })),
        )
        .into_any_element()
    }
    fn environment_danger_zone(
        &self,
        index: usize,
        record: &EnvironmentRecord,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.environments.busy();
        let form = &self.environments.form;
        let blocked =
            busy || form.dirty || form.conflict || self.environments.unresolved(record.profile.id);
        let removable = !(record.connected || record.container_id.is_some());
        let mut rows = Vec::new();
        if matches!(
            record.profile.target,
            EnvironmentTarget::Container {
                container: ContainerTarget::Managed { .. },
                ..
            }
        ) {
            rows.push(
                settings_ui::setting_row(
                    "Remove container",
                    Some(
                        "Removes the container Xylo created. Its image and named volumes stay."
                            .into(),
                    ),
                    Some(
                        settings_ui::destructive_button(
                            "environment-remove-container",
                            "Remove container",
                        )
                        .disabled(blocked || record.container_id.is_none() || record.connected)
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.environment_confirm_remove(true, window, cx)
                        }))
                        .into_any_element(),
                    ),
                )
                .inset()
                .into_any_element(),
            );
        }
        rows.push(
            settings_ui::setting_row(
                "Remove environment",
                Some(
                    if removable {
                        "Removes the saved profile. Remote files and existing containers stay."
                    } else {
                        "Disconnect and remove any Xylo container before removing this environment."
                    }
                    .into(),
                ),
                Some(
                    settings_ui::destructive_button(("environment-remove-row", index), "Remove")
                        .disabled(blocked || !removable)
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.environment_confirm_remove(false, window, cx)
                        }))
                        .into_any_element(),
                ),
            )
            .inset()
            .into_any_element(),
        );
        settings_ui::danger_zone(rows).into_any_element()
    }
    /// The setup form: kind, name, target, authentication and client.
    fn environment_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.environments.busy();
        let form = &self.environments.form;
        let weak = cx.entity().downgrade();
        let kinds = [Kind::Ssh, Kind::Docker, Kind::Podman]
            .into_iter()
            .map(|kind| {
                let view = weak.clone();
                SelectOption::new(kind.label(), form.kind == kind, move |_, cx| {
                    let _ = view.update(cx, |view, cx| {
                        if view.environments.busy() || view.environments.form.kind == kind {
                            return;
                        }
                        view.environments.form.kind = kind;
                        view.environments.form.client = None;
                        view.environment_form_changed(cx);
                    });
                })
            })
            .collect();
        let mut fields: Vec<AnyElement> = vec![
            settings_ui::field(
                "Type",
                div().child(settings_ui::segmented("environment-kind", kinds)),
                None,
            )
            .into_any_element(),
            text_field("Name", &form.name, None),
        ];
        let mut rows: Vec<AnyElement> = Vec::new();
        match form.kind {
            Kind::Ssh => {
                fields.push(self.environment_ssh_import(cx));
                fields.push(text_field("Host", &form.host, None));
                fields.push(
                    div()
                        .flex()
                        .gap(px(12.))
                        .child(div().flex_1().child(text_field("User", &form.user, None)))
                        .child(
                            div()
                                .w(px(120.))
                                .child(text_field("Port", &form.port, None)),
                        )
                        .into_any_element(),
                );
                let key_file = form.identity.is_some();
                let agent = weak.clone();
                let key = weak.clone();
                fields.push(
                    settings_ui::field(
                        "Authentication",
                        div().child(settings_ui::segmented(
                            "environment-auth",
                            vec![
                                SelectOption::new(
                                    "SSH agent (recommended)",
                                    !key_file,
                                    move |_, cx| {
                                        let _ = agent.update(cx, |view, cx| {
                                            view.environments.form.identity = None;
                                            view.environment_form_changed(cx);
                                        });
                                    },
                                ),
                                SelectOption::new("Key file…", key_file, move |window, cx| {
                                    let _ = key.update(cx, |view, cx| {
                                        view.environment_pick(Asset::Identity, window, cx)
                                    });
                                }),
                            ],
                        )),
                        Some("Xylo never stores passwords.".into()),
                    )
                    .into_any_element(),
                );
                if let Some(identity) = &form.identity {
                    rows.push(
                        settings_ui::setting_row(
                            "Key file",
                            Some(tilde(identity).into()),
                            Some(
                                settings_ui::text_button("environment-identity-change", "Change")
                                    .disabled(busy)
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.environment_pick(Asset::Identity, window, cx)
                                    }))
                                    .into_any_element(),
                            ),
                        )
                        .into_any_element(),
                    );
                }
                let trusted = form
                    .known_hosts
                    .as_ref()
                    .map(|path| tilde(path))
                    .unwrap_or_else(|| "None chosen".into());
                let untrusted = self.environment_host_trusted(cx) == Some(false);
                rows.push(
                    settings_ui::setting_row(
                        "Trusted hosts",
                        Some(trusted.into()),
                        Some(
                            settings_ui::text_button("environment-known-hosts", "Change")
                                .disabled(busy)
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.environment_pick(Asset::KnownHosts, window, cx)
                                }))
                                .into_any_element(),
                        ),
                    )
                    .feedback(untrusted.then(|| {
                        Feedback::Error(
                            "This host isn't trusted yet. Connect once with ssh in Terminal to verify its key, then test again."
                                .into(),
                        )
                    }))
                    .into_any_element(),
                );
            }
            Kind::Docker | Kind::Podman => {
                let modes = [(false, "Running container"), (true, "New from image")]
                    .into_iter()
                    .map(|(managed, label)| {
                        let view = weak.clone();
                        SelectOption::new(label, form.managed == managed, move |_, cx| {
                            let _ = view.update(cx, |view, cx| {
                                view.environments.form.managed = managed;
                                view.environment_form_changed(cx);
                            });
                        })
                    })
                    .collect();
                fields.push(
                    settings_ui::field(
                        "Container",
                        div().child(settings_ui::segmented("environment-container-mode", modes)),
                        None,
                    )
                    .into_any_element(),
                );
                if form.managed {
                    fields.push(text_field(
                        "Image",
                        &form.image,
                        Some("For example ubuntu:24.04. Create the container after saving."),
                    ));
                    fields.push(text_field(
                        "Startup command (optional)",
                        &form.startup,
                        None,
                    ));
                } else {
                    fields.push(text_field(
                        "Container name or ID",
                        &form.target,
                        Some("The name or ID shown by docker ps (or podman ps)."),
                    ));
                }
            }
        }
        rows.push(self.environment_client_row(cx));
        fields.push(settings_ui::rows(rows).into_any_element());
        if form.conflict {
            fields.push(
                notice(
                    BadgeTone::Warning,
                    "This environment changed elsewhere. Reload it before saving or testing.",
                )
                .into_any_element(),
            );
        }
        let blocked = busy || form.conflict || self.environments.unresolved(form.id);
        let mut actions = Vec::new();
        if form.dirty {
            actions.push(
                div()
                    .flex_1()
                    .child(settings_ui::caption("Changes have not been saved."))
                    .into_any_element(),
            );
        }
        if form.conflict {
            actions.push(
                settings_ui::text_button("environment-reload-saved", "Reload saved setup")
                    .disabled(busy)
                    .on_click(cx.listener(|view, _, window, cx| {
                        let id = view.environments.selected;
                        view.environment_select(id, window, cx);
                    }))
                    .into_any_element(),
            );
        }
        actions.push(
            settings_ui::busy(
                settings_ui::secondary_button("environment-test-form", "Test connection")
                    .disabled(blocked),
                self.environments.testing.is_some() || self.environments.test_after_save.is_some(),
            )
            .on_click(cx.listener(|view, _, window, cx| view.environment_test_form(window, cx)))
            .into_any_element(),
        );
        actions.push(
            settings_ui::busy(
                settings_ui::primary_button("environment-save", "Save environment")
                    .disabled(blocked),
                self.environments.test_after_save.is_none()
                    && self
                        .environments
                        .waiting_for_action(EnvironmentAction::Save),
            )
            .on_click(cx.listener(|view, _, _, cx| view.environment_save(cx)))
            .into_any_element(),
        );
        fields.push(settings_ui::form_actions(actions).into_any_element());
        fields.push(
            settings_ui::caption(
                "The agent asks before running commands here unless you choose Full access.",
            )
            .into_any_element(),
        );
        settings_ui::form_column(fields).into_any_element()
    }
    /// The client program row: the detected or chosen program, or how to get
    /// one when none was found.
    fn environment_client_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.environments.busy();
        let form = &self.environments.form;
        let detected = self
            .environments
            .clients
            .iter()
            .find(|client| client.runtime == form.kind.runtime() && client.available)
            .map(|client| &client.executable);
        let client = form.client.as_ref().or(detected);
        let missing = client.is_none() && self.environments.clients_loaded;
        settings_ui::setting_row(
            format!("{} program", form.kind.label()),
            Some(
                match client {
                    Some(path) => path.display().to_string(),
                    None if missing => "Not found".into(),
                    None => "Looking for the program…".into(),
                }
                .into(),
            ),
            Some(
                settings_ui::text_button(
                    "environment-client",
                    if client.is_some() {
                        "Change"
                    } else {
                        "Choose program…"
                    },
                )
                .disabled(busy)
                .on_click(cx.listener(|view, _, window, cx| {
                    view.environment_pick(Asset::Client, window, cx)
                }))
                .into_any_element(),
            ),
        )
        .feedback(missing.then(|| Feedback::Error(form.kind.install_hint().into())))
        .into_any_element()
    }
    /// Hosts from `~/.ssh/config`, loaded when asked, to prefill the form.
    fn environment_ssh_import(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.environments.busy();
        let Some(hosts) = &self.environments.ssh_hosts else {
            return settings_ui::setting_row(
                "Import from SSH config",
                Some("Fill in the host, user, port and key from ~/.ssh/config.".into()),
                Some(
                    settings_ui::busy(
                        settings_ui::secondary_button("environment-import-ssh", "Import…")
                            .disabled(busy),
                        self.environments.ssh_loading,
                    )
                    .on_click(cx.listener(|view, _, _, cx| view.environment_load_ssh_hosts(cx)))
                    .into_any_element(),
                ),
            )
            .into_any_element();
        };
        if hosts.is_empty() {
            return settings_ui::caption("No hosts found in ~/.ssh/config.").into_any_element();
        }
        let items = hosts.iter().enumerate().map(|(index, host)| {
            let chosen = host.clone();
            let detail = format!(
                "{}{}{}",
                host.user
                    .as_ref()
                    .map(|user| format!("{user}@"))
                    .unwrap_or_default(),
                host.address(),
                host.port.map(|port| format!(":{port}")).unwrap_or_default()
            );
            settings_ui::list_item(
                ("environment-ssh-host", index),
                Some(settings_ui::monogram(&host.alias, &host.alias).into_any_element()),
                host.alias.clone(),
                Some(detail.into()),
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                if !view.environments.busy() {
                    view.environment_import_ssh_host(chosen.clone(), window, cx)
                }
            }))
            .into_any_element()
        });
        settings_ui::field(
            "Import from SSH config",
            settings_ui::list_container(items),
            Some("Choose a host to fill in the form.".into()),
        )
        .into_any_element()
    }
}
/// The leading tile of a use-case item: a glyph on the muted surface.
// Candidate primitive: an icon counterpart to `settings_ui::monogram`.
fn icon_tile(glyph: AppIcon) -> Div {
    div()
        .size(px(settings_ui::MONOGRAM_SIZE))
        .flex_shrink_0()
        .rounded(px(7.))
        .bg(rgb(well()))
        .flex()
        .items_center()
        .justify_center()
        .child(icons::icon(glyph).text_color(rgb(icons::rest_color())))
}
/// A labelled single-line input of the setup form.
fn text_field(
    label: &'static str,
    input: &Entity<InputState>,
    help: Option<&'static str>,
) -> AnyElement {
    settings_ui::field(
        label,
        Input::new(input).aria_label(label),
        help.map(SharedString::from),
    )
    .into_any_element()
}
/// "SSH · user@host:22" / "Docker · web-1": the kind and where it points.
fn environment_target(target: &EnvironmentTarget) -> String {
    match target {
        EnvironmentTarget::Ssh {
            host, user, port, ..
        } => format!("SSH · {user}@{host}:{port}"),
        EnvironmentTarget::Container {
            runtime, container, ..
        } => {
            let runtime = if *runtime == ContainerRuntime::Docker {
                "Docker"
            } else {
                "Podman"
            };
            match container {
                ContainerTarget::Existing { target } => format!("{runtime} · {target}"),
                ContainerTarget::Managed { image, .. } => {
                    format!("{runtime} · New container from {image}")
                }
            }
        }
    }
}
fn operation_state(state: EnvironmentOperationState) -> &'static str {
    match state {
        EnvironmentOperationState::Succeeded => "Completed",
        EnvironmentOperationState::Failed => "Failed",
        EnvironmentOperationState::Cancelled => "Cancelled",
        EnvironmentOperationState::Prepared | EnvironmentOperationState::Running => "In progress",
        EnvironmentOperationState::OutcomeUnknown => "Result needs checking",
    }
}
/// "5 min ago" style age of a test.
fn tested_age(at: i64) -> String {
    let seconds = now_ms().saturating_sub(at).max(0) / 1000;
    match seconds {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=86399 => format!("{} h ago", seconds / 3600),
        _ => format!("{} days ago", seconds / 86400),
    }
}
/// A path with the home directory shown as `~`.
fn tilde(path: &Path) -> String {
    match ssh_config::home().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
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

#[cfg(test)]
#[path = "environments_view/tests.rs"]
mod tests;
