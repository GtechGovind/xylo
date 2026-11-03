use super::*;
use crate::host::tests::{next_event, state, stop};

async fn mark_scheduled(host: &HostState, session: Uuid) {
    let task = host
        .default_scheduler()
        .unwrap()
        .create("fixture".into(), "0 * * * * * *".into())
        .await
        .unwrap();
    host.scheduled.lock().await.insert(session, task.id);
}

#[tokio::test]
async fn handshake_waits_for_the_reply_to_hello_and_skips_unsolicited_ready() {
    let hello = Uuid::new_v4();
    let ready = |request_id| {
        serde_json::to_string(&Envelope::new(
            request_id,
            None,
            1,
            EngineEvent::Ready {
                version: "fixture".into(),
            },
        ))
        .unwrap()
    };
    let lines = format!("{}\n{}\n", ready(Uuid::nil()), ready(hello));
    let mut output = BufReader::new(lines.as_bytes());
    handshake(&mut output, hello).await.unwrap();
}

#[tokio::test]
async fn handshake_rejects_another_protocol_version_with_a_clear_message() {
    let hello = Uuid::new_v4();
    let line = format!(
        "{{\"version\":1,\"request_id\":\"{hello}\",\"session_id\":null,\"sequence\":1,\"payload\":{{\"event\":\"ready\",\"version\":\"old\"}}}}\n"
    );
    let error = handshake(&mut BufReader::new(line.as_bytes()), hello)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("different Xylo version"), "{error}");
    assert!(error.contains("protocol 1"), "{error}");
}

#[tokio::test]
async fn handshake_reports_startup_errors_and_early_exit() {
    let hello = Uuid::new_v4();
    let failure = serde_json::to_string(&Envelope::new(
        Uuid::nil(),
        None,
        1,
        EngineEvent::Error {
            message: "vault key rejected".into(),
            retryable: false,
            kind: ErrorKind::Internal,
        },
    ))
    .unwrap()
        + "\n";
    let error = handshake(&mut BufReader::new(failure.as_bytes()), hello)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("vault key rejected"), "{error}");
    let error = handshake(&mut BufReader::new(&b""[..]), hello)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("exited during startup"), "{error}");
}

#[test]
fn diagnostics_redact_tokens_keys_and_api_key_values() {
    let line = "POST /v1 authorization: Bearer abc.def-123 key=sk-proj-XYZ123 url?api_key=s3cr3t&x=1 \"api_key\": \"quoted\" risk-free";
    let redacted = redact_secrets(line);
    for secret in ["abc.def-123", "sk-proj-XYZ123", "s3cr3t", "quoted"] {
        assert!(!redacted.contains(secret), "{redacted}");
    }
    assert!(redacted.contains("Bearer [REDACTED]"), "{redacted}");
    assert!(redacted.contains("&x=1"), "{redacted}");
    assert!(redacted.contains("risk-free"), "{redacted}");
    assert_eq!(redact_secrets("plain line ✓"), "plain line ✓");
}

#[test]
fn host_sequences_are_monotonic_per_session() {
    let sequences = SessionSequences::default();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    assert_eq!(sequences.next(a), 1);
    assert_eq!(sequences.next(a), 2);
    assert_eq!(sequences.next(b), 1);
    let HostEvent::Engine(envelope) = sequences.engine(Uuid::new_v4(), a, EngineEvent::Cancelled)
    else {
        panic!("expected an engine event")
    };
    assert_eq!(envelope.sequence, 3);
    assert_eq!(envelope.session_id, Some(a));
}

#[tokio::test]
async fn admission_reports_preparing_with_a_real_sequence_before_dispatch() {
    let (_directory, host, events) = state();
    let (turn, draft) = crate::host::tests::draft::input(&host);
    let session = turn.session_id;
    let (mut incoming, _exit) = crate::host::tests::draft::mock_worker(&host, session, 4).await;
    host.worker_command_with_draft_and_receipt(
        Some(session),
        WorkerCommand::Run { turn },
        Some(draft),
        Some(Uuid::new_v4()),
    )
    .await
    .unwrap();
    let HostEvent::Engine(preparing) = events.recv().await.unwrap() else {
        panic!("Preparing must be the first event of an admission")
    };
    assert!(matches!(
        preparing.payload,
        EngineEvent::Phase {
            phase: TurnPhase::Preparing
        }
    ));
    assert!(preparing.sequence > 0);
    let run = incoming.recv().await.unwrap();
    assert_eq!(
        run.request_id, preparing.request_id,
        "Preparing and the run share one request"
    );
}

#[tokio::test]
async fn admission_uses_current_mode_and_the_projects_standing_approvals() {
    let (_directory, host, _events) = state();
    let (mut turn, draft) = crate::host::tests::draft::input(&host);
    turn.permissions = PermissionMode::ReadOnly;
    let session = turn.session_id;
    let workspace = std::fs::canonicalize(&turn.workspace).unwrap();
    SettingsFiles::for_workspace(&workspace)
        .save_rule("Bash(cargo test:*)", PermissionDecision::Allow, false)
        .unwrap();
    host.settings.lock().await.permissions = PermissionMode::ApproveForMe;
    let (mut incoming, _exit) = crate::host::tests::draft::mock_worker(&host, session, 4).await;
    host.worker_command_with_draft_and_receipt(
        Some(session),
        WorkerCommand::Run { turn },
        Some(draft),
        None,
    )
    .await
    .unwrap();
    let WorkerCommand::RunAuthorized { turn, .. } = incoming.recv().await.unwrap().payload else {
        panic!("the run was not dispatched")
    };
    assert_eq!(turn.permissions, PermissionMode::ApproveForMe);
    assert_eq!(
        SettingsFiles::for_workspace(&workspace)
            .load()
            .unwrap()
            .rules[0]
            .pattern,
        "Bash(cargo test:*)"
    );
}

#[test]
fn only_unattended_scheduled_turns_lose_full_access() {
    for mode in PermissionMode::ALL {
        assert_eq!(effective_mode(mode, false), mode);
    }
    assert_eq!(
        effective_mode(PermissionMode::FullAccess, true),
        PermissionMode::ApproveForMe
    );
    for mode in [
        PermissionMode::ReadOnly,
        PermissionMode::AskForApproval,
        PermissionMode::ApproveForMe,
    ] {
        assert_eq!(effective_mode(mode, true), mode);
    }
}

#[tokio::test]
async fn scheduled_admission_never_runs_with_full_access() {
    let (_directory, host, _events) = state();
    host.settings.lock().await.permissions = PermissionMode::FullAccess;
    for scheduled in [false, true] {
        let (turn, draft) = crate::host::tests::draft::input(&host);
        let session = turn.session_id;
        if scheduled {
            mark_scheduled(&host, session).await;
        }
        let (mut incoming, _exit) = crate::host::tests::draft::mock_worker(&host, session, 4).await;
        host.worker_command_with_draft_and_receipt(
            Some(session),
            WorkerCommand::Run { turn },
            Some(draft),
            None,
        )
        .await
        .unwrap();
        let WorkerCommand::RunAuthorized { turn, .. } = incoming.recv().await.unwrap().payload
        else {
            panic!("the run was not dispatched")
        };
        let expected = if scheduled {
            PermissionMode::ApproveForMe
        } else {
            PermissionMode::FullAccess
        };
        assert_eq!(turn.permissions, expected);
    }
}

#[tokio::test]
async fn full_access_forwarded_to_a_scheduled_worker_becomes_approve_for_me() {
    let (_directory, host, _events) = state();
    let interactive = Uuid::new_v4();
    let scheduled = Uuid::new_v4();
    mark_scheduled(&host, scheduled).await;
    let (mut interactive_commands, _a) =
        crate::host::tests::draft::mock_worker(&host, interactive, 4).await;
    let (mut scheduled_commands, _b) =
        crate::host::tests::draft::mock_worker(&host, scheduled, 4).await;
    host.forward_permissions(PermissionMode::FullAccess).await;
    assert!(matches!(
        interactive_commands.recv().await.unwrap().payload,
        WorkerCommand::SetPermissions {
            mode: PermissionMode::FullAccess
        }
    ));
    assert!(matches!(
        scheduled_commands.recv().await.unwrap().payload,
        WorkerCommand::SetPermissions {
            mode: PermissionMode::ApproveForMe
        }
    ));
}

#[tokio::test]
async fn a_new_vault_key_retires_idle_workers_now_and_busy_ones_after_their_turn() {
    let (_directory, host, _events) = state();
    let idle = Uuid::new_v4();
    let busy = Uuid::new_v4();
    let (_idle_commands, _idle_exit) = crate::host::tests::draft::mock_worker(&host, idle, 2).await;
    let (_busy_commands, _busy_exit) = crate::host::tests::draft::mock_worker(&host, busy, 2).await;
    host.workers.lock().await[&busy]
        .active
        .store(true, Ordering::Release);
    host.retire_workers_for_new_key().await;
    let workers = host.workers.lock().await;
    assert!(
        workers[&idle].stop.is_cancelled(),
        "idle worker retired at once"
    );
    assert!(
        !workers[&busy].stop.is_cancelled(),
        "a running turn is not interrupted"
    );
    assert!(
        workers[&busy].retire_after_run.load(Ordering::Acquire),
        "the busy worker retires when its turn ends"
    );
}

#[tokio::test]
async fn permission_changes_reach_every_live_worker() {
    let (_directory, host, events) = state();
    let session = Uuid::new_v4();
    let (mut incoming, _exit) = crate::host::tests::draft::mock_worker(&host, session, 4).await;
    let mut settings = host.settings.lock().await.clone();
    settings.theme = "dark".into();
    host.handle(HostCommand::SaveSettings(settings.clone()))
        .await
        .unwrap();
    assert!(
        incoming.try_recv().is_err(),
        "an unchanged mode sends nothing"
    );
    settings.permissions = PermissionMode::ReadOnly;
    host.handle(HostCommand::SaveSettings(settings))
        .await
        .unwrap();
    assert!(matches!(
        incoming.recv().await.unwrap().payload,
        WorkerCommand::SetPermissions {
            mode: PermissionMode::ReadOnly
        }
    ));
    assert_eq!(
        host.store.settings().unwrap().permissions,
        PermissionMode::ReadOnly
    );
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, HostEvent::Error(_)));
    }
}

#[tokio::test]
async fn always_allow_saves_the_rule_for_the_chats_project_once() {
    let (_directory, host, _events) = state();
    let session = Uuid::new_v4();
    let action_id = Uuid::new_v4();
    let workspace = std::fs::canonicalize(&host.paths.data).unwrap();
    let (mut incoming, _exit) = crate::host::tests::draft::mock_worker(&host, session, 4).await;
    {
        let workers = host.workers.lock().await;
        let worker = &workers[&session];
        *worker.workspace.lock() = Some(workspace.clone());
        worker.approvals.lock().insert(
            action_id,
            PendingApproval {
                name: "run_command".into(),
                rule_key: Some("Bash(cargo test:*)".into()),
                parent_id: None,
                decision: None,
            },
        );
    }
    let approve = WorkerCommand::Approve {
        action_id,
        allow: true,
        remember: true,
    };
    host.handle(HostCommand::Worker(Some(session), approve.clone()))
        .await
        .unwrap();
    assert!(matches!(
        incoming.recv().await.unwrap().payload,
        WorkerCommand::Approve { remember: true, .. }
    ));
    let files = SettingsFiles::for_workspace(&workspace);
    let rules = files.load().unwrap().rules;
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].pattern, "Bash(cargo test:*)");
    files.remove_rule(&rules[0]).unwrap();
    host.handle(HostCommand::Worker(Some(session), approve))
        .await
        .unwrap();
    assert!(
        files.load().unwrap().rules.is_empty(),
        "a duplicate decision does not save again"
    );
}

#[tokio::test]
async fn a_decision_for_a_reaped_task_settles_its_card() {
    let (_directory, host, events) = state();
    let session = Uuid::new_v4();
    let action_id = Uuid::new_v4();
    host.handle(HostCommand::Worker(
        Some(session),
        WorkerCommand::Approve {
            action_id,
            allow: true,
            remember: false,
        },
    ))
    .await
    .unwrap();
    let HostEvent::Engine(envelope) = events.recv().await.unwrap() else {
        panic!("expected a settled card")
    };
    assert_eq!(envelope.session_id, Some(session));
    assert!(matches!(
        envelope.payload,
        EngineEvent::ToolFinished { action_id: id, success: false, .. } if id == action_id
    ));
}

#[tokio::test]
async fn host_internal_commands_are_refused_from_the_ui() {
    let (_directory, host, _events) = state();
    let session = Uuid::new_v4();
    let (mut incoming, _exit) = crate::host::tests::draft::mock_worker(&host, session, 4).await;
    for command in [
        WorkerCommand::SetPermissions {
            mode: PermissionMode::FullAccess,
        },
        WorkerCommand::Unlock {
            key: WorkerCredential(None),
        },
        WorkerCommand::Hello,
    ] {
        assert!(
            host.handle(HostCommand::Worker(Some(session), command))
                .await
                .is_err()
        );
    }
    assert!(incoming.try_recv().is_err());
}

#[cfg(unix)]
mod process {
    use super::*;

    const ACTION: &str = "6a3c1e9e-0d1f-4c4b-9a55-111111111111";
    /// A worker stand-in speaking the protocol over sh and sed. Modes:
    /// `ignore` never answers Cancel; `honor` acknowledges and cancels;
    /// `refuse` answers every Approve with an error; `delayed` waits a second
    /// before asking for approval; `titled` completes at once and then sends
    /// a generated title, as a worker does.
    const FIXTURE: &str = r#"
mode="$1"
run=""
emit() { printf '{"version":%s,"request_id":"%s","session_id":null,"sequence":0,"payload":%s}\n' "$VERSION" "$1" "$2"; }
while IFS= read -r line; do
  rid=$(printf '%s' "$line" | sed -n 's/^{"version":[0-9]*,"request_id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"command":"hello"'*) emit "$rid" '{"event":"ready","version":"fixture"}' ;;
    *'"command":"run_authorized"'*)
      run="$rid"
      emit "$run" '{"event":"started"}'
      if [ "$mode" = titled ]; then
        emit "$run" '{"event":"completed","text":"Done","usage":{"input_tokens":1,"output_tokens":1,"elapsed_ms":1}}'
        emit "$run" '{"event":"title_updated","title":"Fixture title"}'
        continue
      fi
      [ "$mode" = delayed ] && sleep 1
      emit "$run" "{\"event\":\"approval_required\",\"action_id\":\"$ACTION\",\"name\":\"file_write\",\"arguments\":{},\"rule\":\"Edits in this project\",\"rule_key\":\"write:project\"}" ;;
    *'"command":"answer"'*)
      if [ "$answered" != yes ]; then
        answered=yes
        emit "$rid" '{"event":"error","message":"Invalid answer; retry","retryable":false,"kind":"internal"}'
        emit "$run" "{\"event\":\"plan_required\",\"action_id\":\"$ACTION\",\"plan\":\"Review again\"}"
      else
        emit "$rid" '{"event":"status","text":"Answer accepted"}'
      fi ;;
    *'"command":"approve"'*)
      [ "$mode" = refuse ] && emit "$rid" '{"event":"error","message":"Approval is no longer pending","retryable":false,"kind":"internal"}' ;;
    *'"command":"cancel"'*)
      if [ "$mode" = honor ]; then
        emit "$rid" '{"event":"status","text":"Cancellation requested"}'
        emit "$run" '{"event":"cancelled"}'
      fi ;;
  esac
done
"#;

    fn fixture(mode: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(FIXTURE)
            .arg("fixture")
            .arg(mode)
            .env("VERSION", PROTOCOL_VERSION.to_string())
            .env("ACTION", ACTION);
        command
    }

    /// Starts a fixture worker and a running turn in it.
    async fn running(mode: &str) -> (tempfile::TempDir, Arc<HostState>, Receiver<HostEvent>, Uuid) {
        let (directory, host, events) = state();
        let (turn, draft) = crate::host::tests::draft::input(&host);
        let session = turn.session_id;
        let worker = host.start_worker(session, fixture(mode)).await.unwrap();
        host.workers.lock().await.insert(session, worker);
        host.worker_command_with_draft_and_receipt(
            Some(session),
            WorkerCommand::Run { turn },
            Some(draft),
            None,
        )
        .await
        .unwrap();
        wait_for(&events, |event| {
            matches!(
                event,
                HostEvent::Engine(Envelope {
                    payload: EngineEvent::ApprovalRequired { .. },
                    ..
                })
            )
        })
        .await;
        (directory, host, events, session)
    }

    async fn wait_for(
        events: &Receiver<HostEvent>,
        mut wanted: impl FnMut(&HostEvent) -> bool,
    ) -> HostEvent {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = next_event(events).await;
                if wanted(&event) {
                    return event;
                }
            }
        })
        .await
        .expect("expected host event did not arrive")
    }

    #[tokio::test]
    async fn interaction_plan_acceptance_publishes_mode_then_plan_is_delivered() {
        let (_directory, host, events, session) = running("honor").await;
        host.settings.lock().await.permissions = PermissionMode::Plan;
        host.control_worker(
            Some(session),
            WorkerCommand::SetPermissions {
                mode: PermissionMode::Plan,
            },
        )
        .await
        .unwrap();
        let id = Uuid::new_v4();
        // The fixture rejects the first answer so the same pending request is retried.
        host.control_worker(
            Some(session),
            WorkerCommand::Answer {
                action_id: id,
                answer: xylo_core::InteractionAnswer::Reject,
            },
        )
        .await
        .unwrap();
        wait_for(&events, |event| {
            matches!(event, HostEvent::InteractionReply { result: Err(_), .. })
        })
        .await;
        wait_for(&events, |event| {
            matches!(event, HostEvent::PlanModes { .. })
        })
        .await;
        host.control_worker(
            Some(session),
            WorkerCommand::Answer {
                action_id: id,
                answer: xylo_core::InteractionAnswer::ApprovePlan {
                    mode: PermissionMode::AskForApproval,
                },
            },
        )
        .await
        .unwrap();
        let event = wait_for(&events, |event| {
            matches!(
                event,
                HostEvent::Permissions {
                    mode: PermissionMode::AskForApproval,
                    ..
                }
            )
        })
        .await;
        let HostEvent::Permissions {
            revision: accepted, ..
        } = event
        else {
            unreachable!()
        };
        wait_for(&events, |event| {
            matches!(event, HostEvent::InteractionReply { result: Ok(()), .. })
        })
        .await;
        assert_eq!(host.settings.lock().await.permissions, PermissionMode::Plan);
        host.control_worker(
            Some(session),
            WorkerCommand::SetPermissions {
                mode: PermissionMode::Plan,
            },
        )
        .await
        .unwrap();
        let event = wait_for(&events, |event| {
            matches!(
                event,
                HostEvent::Permissions {
                    mode: PermissionMode::Plan,
                    ..
                }
            )
        })
        .await;
        assert!(matches!(event, HostEvent::Permissions { revision, .. } if revision > accepted));
        assert_eq!(
            *host.workers.lock().await[&session].mode.lock(),
            PermissionMode::Plan
        );
        stop(&host, session).await;
    }

    #[tokio::test]
    async fn interaction_answer_reply_retry_keeps_host_mode_consistent() {
        let (_directory, host, events, session) = running("honor").await;
        let workspace = host.workers.lock().await[&session]
            .workspace
            .lock()
            .clone()
            .unwrap();
        host.settings.lock().await.permissions = PermissionMode::AskForApproval;
        host.start_loop(session, workspace, "/loop check".into())
            .await
            .unwrap();
        let task = host
            .default_scheduler()
            .unwrap()
            .list()
            .await
            .unwrap()
            .remove(0);
        host.scheduled.lock().await.insert(session, task.id);
        let id = Uuid::new_v4();
        for (current, accepted, expected) in [
            (
                PermissionMode::FullAccess,
                false,
                PermissionMode::AskForApproval,
            ),
            (
                PermissionMode::FullAccess,
                true,
                PermissionMode::AskForApproval,
            ),
            (PermissionMode::ReadOnly, true, PermissionMode::ReadOnly),
        ] {
            host.settings.lock().await.permissions = current;
            host.control_worker(
                Some(session),
                WorkerCommand::Answer {
                    action_id: id,
                    answer: xylo_core::InteractionAnswer::ApprovePlan {
                        mode: PermissionMode::FullAccess,
                    },
                },
            )
            .await
            .unwrap();
            let event = wait_for(&events, |event| matches!(event, HostEvent::InteractionReply { action_id, .. } if *action_id == id)).await;
            assert!(
                matches!(event, HostEvent::InteractionReply { result, .. } if result.is_ok() == accepted)
            );
            if accepted {
                assert_eq!(*host.workers.lock().await[&session].mode.lock(), expected);
            } else {
                let event = wait_for(&events, |event| {
                    matches!(event, HostEvent::PlanModes { .. })
                })
                .await;
                let HostEvent::PlanModes {
                    modes, scheduled, ..
                } = event
                else {
                    unreachable!()
                };
                assert!(scheduled);
                assert!(modes.contains(&PermissionMode::AskForApproval));
                assert!(modes.contains(&PermissionMode::ReadOnly));
                assert!(!modes.contains(&PermissionMode::FullAccess));
                assert!(!modes.contains(&PermissionMode::ApproveForMe));
            }
        }
        stop(&host, session).await;
    }

    #[tokio::test]
    async fn a_scheduled_approval_wait_names_the_effective_mode() {
        let (_directory, host, events) = state();
        host.settings.lock().await.permissions = PermissionMode::FullAccess;
        let (turn, draft) = crate::host::tests::draft::input(&host);
        let session = turn.session_id;
        mark_scheduled(&host, session).await;
        let worker = host.start_worker(session, fixture("ignore")).await.unwrap();
        host.workers.lock().await.insert(session, worker);
        host.worker_command_with_draft_and_receipt(
            Some(session),
            WorkerCommand::Run { turn },
            Some(draft),
            None,
        )
        .await
        .unwrap();
        let notice = wait_for(&events, |event| matches!(event, HostEvent::Status(_))).await;
        let HostEvent::Status(text) = notice else {
            unreachable!()
        };
        assert!(text.contains("waiting for your approval"), "{text}");
        assert!(text.contains("Approve for me"), "{text}");
    }

    #[tokio::test]
    async fn a_blocked_ui_channel_never_holds_the_schedule_lock() {
        let (_directory, host, events) = state();
        let (turn, draft) = crate::host::tests::draft::input(&host);
        let session = turn.session_id;
        mark_scheduled(&host, session).await;
        let worker = host
            .start_worker(session, fixture("delayed"))
            .await
            .unwrap();
        host.workers.lock().await.insert(session, worker);
        host.worker_command_with_draft_and_receipt(
            Some(session),
            WorkerCommand::Run { turn },
            Some(draft),
            None,
        )
        .await
        .unwrap();
        // Fill the UI channel so the reader blocks sending the approval notice.
        while host
            .events
            .try_send(HostEvent::Status("filler".into()))
            .is_ok()
        {}
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), host.scheduled.lock())
                .await
                .is_ok(),
            "the reader waits on the UI while holding the schedule lock"
        );
        let notice = wait_for(&events, |event| {
            matches!(event, HostEvent::Status(text) if text.contains("waiting for your approval"))
        })
        .await;
        assert!(matches!(notice, HostEvent::Status(_)));
    }

    #[tokio::test]
    async fn a_stop_the_worker_ignores_kills_it_and_reports_a_cancellation() {
        let (_directory, host, events, session) = running("ignore").await;
        let exited = host.workers.lock().await[&session].exited.clone();
        stop(&host, session).await;
        let cancelled = wait_for(&events, |event| {
            matches!(
                event,
                HostEvent::Engine(Envelope {
                    payload: EngineEvent::Cancelled | EngineEvent::Error { .. },
                    ..
                })
            )
        })
        .await;
        assert!(
            matches!(&cancelled, HostEvent::Engine(Envelope { payload: EngineEvent::Cancelled, session_id, .. }) if *session_id == Some(session)),
            "a forced stop is a cancellation, not a crash"
        );
        let notice = wait_for(&events, |event| matches!(event, HostEvent::Status(_))).await;
        assert!(matches!(notice, HostEvent::Status(text) if text == FORCED_STOP_NOTICE));
        assert!(*exited.borrow());
        let workers = host.workers.lock().await;
        assert!(!workers[&session].active.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn an_honored_stop_forwards_the_acknowledgement_and_keeps_the_worker() {
        let (_directory, host, events, session) = running("honor").await;
        stop(&host, session).await;
        let ack = wait_for(&events, |event| {
            matches!(
                event,
                HostEvent::Engine(Envelope {
                    payload: EngineEvent::Status { .. },
                    ..
                })
            )
        })
        .await;
        let HostEvent::Engine(ack) = ack else {
            unreachable!()
        };
        assert!(ack.sequence > 0);
        wait_for(&events, |event| {
            matches!(
                event,
                HostEvent::Engine(Envelope {
                    payload: EngineEvent::Cancelled,
                    ..
                })
            )
        })
        .await;
        tokio::time::sleep(STOP_GRACE * 2).await;
        let workers = host.workers.lock().await;
        let worker = &workers[&session];
        assert!(!worker.dead(), "a cooperative stop keeps the warm worker");
        assert!(!worker.forced_stop.load(Ordering::Acquire));
        assert!(!worker.active.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn retiring_an_idle_worker_leaves_its_queue_running() {
        let (_directory, host, _events) = state();
        let (turn, draft) = crate::host::tests::draft::input(&host);
        let session = turn.session_id;
        host.store.queue_turn(&turn, &draft).unwrap();
        let worker = host.start_worker(session, fixture("ignore")).await.unwrap();
        let mut exited = worker.exited.clone();
        host.workers.lock().await.insert(session, worker.clone());
        worker.retire(session);
        tokio::time::timeout(Duration::from_secs(10), exited.wait_for(|exited| *exited))
            .await
            .unwrap()
            .unwrap();
        // A wrongly spawned settlement would pause the item right after reaping.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let item = host
            .store
            .queued_turn(session, turn.turn_id)
            .unwrap()
            .unwrap();
        assert!(!item.paused, "nothing ran, so nothing failed");
    }

    #[tokio::test]
    async fn a_title_after_completion_is_applied() {
        let (_directory, host, events) = state();
        let (turn, draft) = crate::host::tests::draft::input(&host);
        let session = turn.session_id;
        host.store
            .upsert_conversation(&xylo_core::Conversation {
                id: session,
                title: "Fixture title".into(),
                workspace: host.paths.data.clone(),
                updated_at: xylo_core::now_ms(),
                title_source: xylo_core::TitleSource::Generated,
                pinned: false,
                archived: false,
            })
            .unwrap();
        let worker = host.start_worker(session, fixture("titled")).await.unwrap();
        host.workers.lock().await.insert(session, worker);
        host.worker_command_with_draft_and_receipt(
            Some(session),
            WorkerCommand::Run { turn },
            Some(draft),
            None,
        )
        .await
        .unwrap();
        wait_for(&events, |event| {
            matches!(
                event,
                HostEvent::Engine(Envelope {
                    payload: EngineEvent::Completed { .. },
                    ..
                })
            )
        })
        .await;
        let renamed = wait_for(&events, |event| {
            matches!(event, HostEvent::ConversationRenamed(conversation) if conversation.id == session)
        })
        .await;
        let HostEvent::ConversationRenamed(conversation) = renamed else {
            unreachable!()
        };
        assert_eq!(conversation.title, "Fixture title");
    }

    #[tokio::test]
    async fn a_failed_first_decision_settles_the_card_by_action() {
        let (_directory, host, events, session) = running("refuse").await;
        let action_id: Uuid = ACTION.parse().unwrap();
        host.handle(HostCommand::Worker(
            Some(session),
            WorkerCommand::Approve {
                action_id,
                allow: true,
                remember: false,
            },
        ))
        .await
        .unwrap();
        let settled = wait_for(&events, |event| {
            matches!(
                event,
                HostEvent::Engine(Envelope {
                    payload: EngineEvent::ToolFinished { .. },
                    ..
                })
            )
        })
        .await;
        assert!(matches!(
            settled,
            HostEvent::Engine(Envelope {
                payload: EngineEvent::ToolFinished { action_id: id, success: false, ref result, .. },
                ..
            }) if id == action_id && result.contains("no longer pending")
        ));
    }

    #[tokio::test]
    async fn spawning_a_worker_of_another_protocol_fails_fast() {
        let (_directory, host, _events) = state();
        let mut command = fixture("ignore");
        command.env("VERSION", "1");
        let error = host
            .start_worker(Uuid::new_v4(), command)
            .await
            .err()
            .expect("an old worker must be refused")
            .to_string();
        assert!(error.contains("different Xylo version"), "{error}");
    }
}

#[test]
fn a_worker_with_a_live_shell_job_is_not_retired_as_idle() {
    let (sender, _receiver) = mpsc::channel(8);
    let (_exited, receiver) = tokio::sync::watch::channel(false);
    let worker = WorkerHandle::new(sender, receiver);
    let id = Uuid::new_v4();
    worker.running_jobs.lock().insert(Uuid::new_v4());
    let mut workers = HashMap::from([(id, worker.clone())]);
    retire_idle_workers(
        &mut workers,
        None,
        Instant::now() + IDLE_WORKER_TTL + Duration::from_secs(1),
    );
    assert!(!worker.stop.is_cancelled());
    worker.running_jobs.lock().clear();
    retire_idle_workers(
        &mut workers,
        None,
        Instant::now() + IDLE_WORKER_TTL + Duration::from_secs(1),
    );
    assert!(worker.stop.is_cancelled());
}

#[tokio::test]
async fn interaction_late_plan_receipt_cannot_replace_newer_permissions_or_turn() {
    let (_directory, host, _events) = state();
    let session = Uuid::new_v4();
    let (mut incoming, _exit) = crate::host::tests::draft::mock_worker(&host, session, 4).await;
    let worker = host.workers.lock().await[&session].clone();
    worker.record_mode(PermissionMode::AskForApproval);
    for newer in [PermissionMode::ReadOnly, PermissionMode::Plan] {
        let request = Uuid::new_v4();
        worker
            .send_control(
                session,
                request,
                WorkerCommand::Answer {
                    action_id: Uuid::new_v4(),
                    answer: xylo_core::InteractionAnswer::ApprovePlan {
                        mode: PermissionMode::FullAccess,
                    },
                },
            )
            .await
            .unwrap();
        incoming.recv().await.unwrap();
        let Control::Answer {
            mode: Some(mode),
            revision,
            ..
        } = worker.take_control(request).unwrap()
        else {
            panic!("Missing answer correlation")
        };
        worker.record_mode(newer);
        worker.acknowledge_plan_mode(mode, revision);
        assert_eq!(*worker.mode.lock(), newer);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn worker_diagnostics_keep_warning_and_error_severity_at_info_and_redact_credentials() {
    use tracing::instrument::WithSubscriber;
    let root = tempfile::tempdir().unwrap();
    let file = std::fs::File::create(root.path().join("capture")).unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(file)
        .finish();
    let mut child = tokio::process::Command::new("sh").arg("-c").arg("printf '%s\\n' '2026-11-03T00:00:00Z WARN xylo_engine: provider status=403 Bearer fake-secret' 'ERROR xylo_worker: failed api_key=fake-key' 'INFO xylo_worker: ready' 'DEBUG xylo_engine: detail' >&2").stderr(Stdio::piped()).spawn().unwrap();
    log_diagnostics(child.stderr.take().unwrap(), Uuid::nil())
        .with_subscriber(subscriber)
        .await;
    child.wait().await.unwrap();
    let output = std::fs::read_to_string(root.path().join("capture")).unwrap();
    assert!(
        output
            .lines()
            .any(|line| line.contains(" WARN ") && line.contains("provider status=403")),
        "{output}"
    );
    assert!(
        output
            .lines()
            .any(|line| line.contains(" ERROR ") && line.contains("failed")),
        "{output}"
    );
    assert!(output.contains("ready"), "{output}");
    assert!(
        !output.contains("detail")
            && !output.contains("fake-secret")
            && !output.contains("fake-key"),
        "{output}"
    );
}

#[test]
fn diagnostics_redact_auth_headers_and_token_fields() {
    let output = redact_secrets(
        "Authorization: Basic dXNlcjpwYXNz access_token=access-secret refresh_token=refresh-secret client_secret=client-secret x-api-key: header-secret password=pass-secret",
    );
    for secret in [
        "dXNlcjpwYXNz",
        "access-secret",
        "refresh-secret",
        "client-secret",
        "header-secret",
        "pass-secret",
    ] {
        assert!(!output.contains(secret), "{output}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn oversized_diagnostics_are_discarded_without_leaking_split_credentials() {
    use tracing::instrument::WithSubscriber;
    let root = tempfile::tempdir().unwrap();
    let file = std::fs::File::create(root.path().join("capture")).unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(file)
        .finish();
    let message = format!(
        "WARN {}Bearer {}\\nWARN retained-warning\\n",
        "x".repeat(16 * 1024 - 12),
        "secret-suffix".repeat(300)
    );
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg("printf '%b' \"$1\" >&2")
        .arg("fixture")
        .arg(message)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    log_diagnostics(child.stderr.take().unwrap(), Uuid::nil())
        .with_subscriber(subscriber)
        .await;
    child.wait().await.unwrap();
    let output = std::fs::read_to_string(root.path().join("capture")).unwrap();
    assert!(!output.contains("secret-suffix"), "{output}");
    assert!(output.contains("retained-warning"), "{output}");
    assert!(
        output.len() < 1024,
        "oversized diagnostics must not be retained"
    );
}

#[test]
fn diagnostics_redact_entire_quoted_credentials_including_whitespace() {
    let output = redact_secrets(r#"api_key="secret with spaces" token='more secret' status=401"#);
    assert!(
        !output.contains("secret") && !output.contains("spaces"),
        "{output}"
    );
    assert!(output.contains("status=401"), "{output}");
}
