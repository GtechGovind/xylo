use super::Xylo;
use gpui_kit::{AppContext, TestAppContext, WindowOptions};
use uuid::Uuid;
use xylo_core::{EngineEvent, Envelope, ErrorKind, MessageStatus, Usage, WorkerCommand};
use xylo_runtime::host::HostEvent;

/// Runs `check` against a fresh, host-less chat view.
fn with_view(
    cx: &mut TestAppContext,
    check: impl FnOnce(&mut Xylo, &mut gpui_kit::Window, &mut gpui_kit::Context<Xylo>),
) {
    cx.update(gpui_kit::init);
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Isolated fixture")), window, cx))
        })
        .unwrap()
    });
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| check(view, window, cx))
    })
    .unwrap();
}
fn engine(
    view: &mut Xylo,
    session: Option<Uuid>,
    event: EngineEvent,
    window: &mut gpui_kit::Window,
    cx: &mut gpui_kit::Context<Xylo>,
) {
    view.event(
        HostEvent::Engine(Envelope::new(Uuid::new_v4(), session, 0, event)),
        window,
        cx,
    );
}

#[gpui_kit::test]
fn a_finished_reply_stays_in_place_then_moves_into_history_before_the_next_turn(
    cx: &mut TestAppContext,
) {
    with_view(cx, |view, window, cx| {
        let session = Some(view.session);
        let tool = Uuid::new_v4();
        engine(view, session, EngineEvent::Started, window, cx);
        engine(
            view,
            session,
            EngineEvent::Delta {
                text: "Checking.".into(),
            },
            window,
            cx,
        );
        engine(
            view,
            session,
            EngineEvent::ToolStarted {
                action_id: tool,
                name: "file_read".into(),
                arguments: serde_json::json!({"path": "a.rs"}),
                parent_id: None,
            },
            window,
            cx,
        );
        engine(
            view,
            session,
            EngineEvent::ToolFinished {
                action_id: tool,
                name: "file_read".into(),
                result: "{}".into(),
                success: true,
                parent_id: None,
            },
            window,
            cx,
        );
        engine(
            view,
            session,
            EngineEvent::Delta { text: "Ans".into() },
            window,
            cx,
        );
        engine(
            view,
            session,
            EngineEvent::Completed {
                text: "Answer.".into(),
                usage: Usage::default(),
            },
            window,
            cx,
        );
        assert!(
            view.messages.is_empty(),
            "The answer stays where it streamed"
        );
        assert_eq!(
            view.live_row_count(),
            5,
            "Work summary, text, tools, text, and the answer's action row"
        );
        engine(view, session, EngineEvent::Started, window, cx);
        assert_eq!(view.messages.len(), 1);
        assert_eq!(view.messages[0].content, "Answer.");
        assert_eq!(view.messages[0].status, MessageStatus::Complete);
        engine(
            view,
            session,
            EngineEvent::Delta {
                text: "partial".into(),
            },
            window,
            cx,
        );
        engine(view, session, EngineEvent::Cancelled, window, cx);
        assert_eq!(
            view.live_row_count(),
            3,
            "Summary, kept text and the Stopped marker"
        );
        engine(
            view,
            session,
            EngineEvent::Phase {
                phase: xylo_core::TurnPhase::Preparing,
            },
            window,
            cx,
        );
        assert_eq!(view.messages.len(), 2);
        assert_eq!(view.messages[1].content, "partial");
        assert_eq!(view.messages[1].status, MessageStatus::Interrupted);
        assert!(
            view.running(),
            "Admission opens the next turn's activity line"
        );
    });
}

#[gpui_kit::test]
fn engine_events_without_a_chat_are_dropped(cx: &mut TestAppContext) {
    with_view(cx, |view, window, cx| {
        engine(view, None, EngineEvent::Started, window, cx);
        engine(
            view,
            None,
            EngineEvent::Delta {
                text: "stray".into(),
            },
            window,
            cx,
        );
        assert!(view.turns.is_empty());
        assert!(!view.running());
    });
}

#[gpui_kit::test]
fn retry_runs_the_failed_turn_again_under_its_own_id(cx: &mut TestAppContext) {
    with_view(cx, |view, window, cx| {
        let (probe, worker) = std::sync::mpsc::sync_channel(4);
        view.validation_worker_commands = Some(probe);
        view.workspace = Some(std::path::PathBuf::from("/fixture"));
        view.settings.provider.model = "fixture-model".into();
        assert!(view.settings.provider.validate().is_ok());
        let session = view.session;
        let failed = xylo_core::TurnRequest {
            session_id: session,
            turn_id: Uuid::new_v4(),
            prompt: "Fix the build".into(),
            workspace: std::path::PathBuf::from("/fixture"),
            provider: view.settings.provider.clone(),
            permissions: view.settings.permissions,
            attachments: Vec::new(),
            approved_rules: Vec::new(),
        };
        let mut user = crate::state::message(session, "user", failed.prompt.clone());
        user.id = Uuid::new_v5(&failed.turn_id, b"user-message");
        view.messages.push(user);
        view.turns.entry(session).or_default().request = Some(failed.clone());
        engine(view, Some(session), EngineEvent::Started, window, cx);
        engine(
            view,
            Some(session),
            EngineEvent::Delta {
                text: "Half an answer".into(),
            },
            window,
            cx,
        );
        engine(
            view,
            Some(session),
            EngineEvent::Error {
                message: "Rate limited".into(),
                retryable: true,
                kind: ErrorKind::RateLimited,
            },
            window,
            cx,
        );
        assert_eq!(
            view.live_row_count(),
            3,
            "Summary, kept text and the error row"
        );
        view.retry_last_message(cx);
        let (target, command) = worker.try_recv().unwrap();
        assert_eq!(target, session);
        let WorkerCommand::Run { turn } = command else {
            panic!("Retry must run the turn");
        };
        assert_eq!(turn.turn_id, failed.turn_id, "Retry resumes the same turn");
        assert_eq!(turn.prompt, failed.prompt);
        assert_eq!(view.messages.len(), 1, "The prompt is not repeated");
        view.retry_last_message(cx);
        assert!(worker.try_recv().is_err(), "One retry at a time");
        engine(view, Some(session), EngineEvent::Started, window, cx);
        assert_eq!(
            view.messages.len(),
            1,
            "The resumed turn replaces its partial reply"
        );
        assert!(view.running());
        assert!(view.turns[&session].failure().is_none());
    });
}

#[gpui_kit::test]
fn a_skill_row_has_no_expandable_body_and_never_renders_raw_json(cx: &mut TestAppContext) {
    use gpui_kit::test::TestWindowExt;
    cx.update(gpui_kit::init);
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Isolated fixture")), window, cx))
        })
        .unwrap()
    });
    let skill = Uuid::new_v4();
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            let session = Some(view.session);
            engine(view, session, EngineEvent::Started, window, cx);
            engine(
                view,
                session,
                EngineEvent::ToolStarted {
                    action_id: skill,
                    name: "extension_skill".into(),
                    arguments: serde_json::json!({"id": "code-review"}),
                    parent_id: None,
                },
                window,
                cx,
            );
            engine(
                view,
                session,
                EngineEvent::ToolFinished {
                    action_id: skill,
                    name: "extension_skill".into(),
                    result: serde_json::json!({
                        "skill": "code-review",
                        "instructions": "# Code review\nCheck every hunk.",
                    })
                    .to_string(),
                    success: true,
                    parent_id: None,
                },
                window,
                cx,
            );
        });
        window.render_frame(cx);
        assert!(window.try_find(("tool-detail", 0usize)).is_some());
        window.click(("tool-detail", 0usize), cx);
        window.render_frame(cx);
        let view = view.read(cx);
        assert!(
            !view.expanded_tools.contains(&skill),
            "A skill row does not expand"
        );
        let card = view.turns[&view.session].tool(skill).unwrap();
        assert_eq!(card.presentation.verb, "Using skill");
        assert_eq!(card.presentation.target, "Code review");
        assert!(
            window.try_find(("tool-delegate", 0usize)).is_none(),
            "The skill's instructions never render"
        );
        assert!(
            window.try_find(("tool-copy-input", 0usize)).is_none(),
            "A row without a body has no detail to open"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_finished_turn_folds_its_work_under_one_line_and_keeps_the_answer(cx: &mut TestAppContext) {
    use gpui_kit::test::TestWindowExt;
    cx.update(gpui_kit::init);
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Isolated fixture")), window, cx))
        })
        .unwrap()
    });
    let read = Uuid::new_v4();
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.error = None;
            let session = Some(view.session);
            engine(view, session, EngineEvent::Started, window, cx);
            engine(
                view,
                session,
                EngineEvent::ToolStarted {
                    action_id: read,
                    name: "file_read".into(),
                    arguments: serde_json::json!({"path": "src/main.rs"}),
                    parent_id: None,
                },
                window,
                cx,
            );
        });
        window.render_frame(cx);
        assert!(window.try_find("turn-activity").is_some());
        assert!(
            window.try_find("turn-summary").is_none(),
            "A running turn shows its steps, not a summary"
        );
        assert!(window.try_find(("tool-detail", 0usize)).is_some());
        view.update(cx, |view, cx| {
            let session = Some(view.session);
            engine(
                view,
                session,
                EngineEvent::ToolFinished {
                    action_id: read,
                    name: "file_read".into(),
                    result: "{}".into(),
                    success: true,
                    parent_id: None,
                },
                window,
                cx,
            );
            engine(
                view,
                session,
                EngineEvent::Completed {
                    text: "It starts in main.".into(),
                    usage: Usage::default(),
                },
                window,
                cx,
            );
        });
        window.render_frame(cx);
        assert!(window.try_find("turn-activity").is_none());
        assert!(window.try_find("turn-summary").is_some());
        assert!(
            window.try_find(("tool-detail", 0usize)).is_none(),
            "Finished work folds away"
        );
        let answer_row = 1 + view.read(cx).messages.len() + 3;
        assert!(
            window.try_find(("copy-message", answer_row)).is_some(),
            "The latest answer keeps its action row"
        );
        window.click("turn-summary", cx);
        window.render_frame(cx);
        assert!(window.try_find(("tool-detail", 0usize)).is_some());
    })
    .unwrap();
}

/// Opens a host-less chat window for tests that drive frames and timers.
fn open_chat(cx: &mut TestAppContext) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<Xylo>) {
    cx.update(gpui_kit::init);
    cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Isolated fixture")), window, cx))
        })
        .unwrap()
    })
}

fn approval(view: &mut Xylo) -> Uuid {
    let action = Uuid::new_v4();
    let turn = view.turns.entry(view.session).or_default();
    turn.apply(EngineEvent::Started);
    turn.apply(EngineEvent::ApprovalRequired {
        action_id: action,
        name: "shell_command".into(),
        arguments: serde_json::json!({"command": "cargo test"}),
        rule: None,
        rule_key: None,
        reason: None,
        parent_id: None,
    });
    view.invalidate_live_history();
    action
}

#[gpui_kit::test]
fn the_elapsed_time_advances_while_an_approval_waits(cx: &mut TestAppContext) {
    use gpui_kit::test::TestWindowExt;
    let (window, view) = open_chat(cx);
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.error = None;
            approval(view);
            view.sync_history_list(cx);
            cx.notify();
        });
        window.render_frame(cx);
    })
    .unwrap();
    let renders = std::rc::Rc::new(std::cell::Cell::new(0));
    let counted = renders.clone();
    let _observer = cx.update(|cx| cx.observe(&view, move |_, _| counted.set(counted.get() + 1)));
    let over_three_seconds = |cx: &mut TestAppContext| {
        let before = renders.get();
        for _ in 0..3 {
            cx.executor()
                .advance_clock(std::time::Duration::from_secs(1));
            cx.run_until_parked();
        }
        renders.get() - before
    };
    let waiting = over_three_seconds(cx);
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            let session = view.session;
            view.turns
                .get_mut(&session)
                .unwrap()
                .apply(EngineEvent::Cancelled);
            cx.notify();
        })
    });
    cx.run_until_parked();
    // The stop itself settles within a tick; after that nothing ticks.
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();
    let ended = over_three_seconds(cx);
    assert!(
        waiting >= 3,
        "Nothing animates while an approval waits, so a timer advances the clock each second: {waiting}"
    );
    assert_eq!(ended, 0, "The clock stops with the turn");
}

#[gpui_kit::test]
fn the_allow_shortcut_needs_the_card_in_front_of_the_person(cx: &mut TestAppContext) {
    let (window, view) = open_chat(cx);
    let (probe, worker) = std::sync::mpsc::sync_channel(4);
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.error = None;
            view.validation_worker_commands = Some(probe);
            approval(view);
            view.sync_history_list(cx);
            view.settings_open = true;
            view.page = super::super::Page::Providers;
            view.composer
                .update(cx, |input, cx| input.focus(window, cx));
            view.allow_pending(window, cx);
        });
        assert!(
            worker.try_recv().is_err(),
            "Hidden behind settings, nothing is approved"
        );
        view.update(cx, |view, cx| {
            view.settings_open = false;
            view.page = super::super::Page::Chat;
            view.allow_pending(window, cx);
        });
        let (_, command) = worker.try_recv().expect("The visible card is approved");
        assert!(matches!(
            command,
            WorkerCommand::Approve { allow: true, .. }
        ));
    })
    .unwrap();
}

#[gpui_kit::test]
fn the_composer_prompt_follows_the_page(cx: &mut TestAppContext) {
    let (window, view) = open_chat(cx);
    let placeholder = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            view.read(cx)
                .composer
                .read(cx)
                .presentation()
                .placeholder()
                .to_string()
        })
    };
    cx.update_window(window, |_, _, cx| {
        view.update(cx, |view, cx| {
            view.messages
                .push(crate::state::message(view.session, "user", "hi".into()));
            view.refresh_composer_placeholder(cx);
        })
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(placeholder(cx), super::REPLY_PLACEHOLDER);
    cx.update_window(window, |_, _, cx| {
        view.update(cx, |view, cx| view.new_chat(cx))
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(placeholder(cx), super::HOME_PLACEHOLDER);
}

#[gpui_kit::test]
fn only_the_answer_being_read_offers_stop(cx: &mut TestAppContext) {
    let (_, view) = open_chat(cx);
    cx.update(|cx| {
        view.update(cx, |view, _| {
            view.reading_aloud = Some(super::text_key("First answer"));
            view.speech.playing = true;
            assert!(view.reading_answer("First answer"));
            assert!(!view.reading_answer("Second answer"));
            view.speech.playing = false;
            assert!(
                !view.reading_answer("First answer"),
                "Finished playback no longer reads as in progress"
            );
        })
    });
}

#[test]
fn the_answer_gap_holds_its_action_row() {
    assert!(super::ANSWER_GAP >= f32::from(crate::icons::HIT_AREA));
}

#[gpui_kit::test]
fn speech_work_in_flight_can_be_cancelled_from_the_composer(cx: &mut TestAppContext) {
    use gpui_kit::test::TestWindowExt;
    let (window, view) = open_chat(cx);
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.error = None;
            view.speech
                .validation_request(Uuid::new_v4(), "speech_synthesize");
            view.speech.notice = "Creating speech on this computer…".into();
            view.reading_aloud = Some(super::text_key("An answer"));
            cx.notify();
        });
        window.render_frame(cx);
        assert!(window.try_find("composer-speech-status").is_some());
        window.click("cancel-speech", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("composer-speech-status").is_none(),
            "Cancelling clears the pending request and its strip"
        );
        let view = view.read(cx);
        assert!(!view.speech.busy());
        assert_eq!(view.reading_aloud, None, "Read aloud is offered again");
    })
    .unwrap();
}
