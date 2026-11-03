//! Interrupted actions leave no durable records or native chat review workflow.
#![allow(dead_code, unused_imports)]
extern crate gpui_kit as gpui;
#[path = "../src/icons.rs"]
mod icons;
#[path = "../src/motion.rs"]
mod motion;
mod recovery_fixture;
#[path = "../src/state.rs"]
mod state;
#[path = "../src/terminal_view.rs"]
mod terminal_view;
#[path = "../src/terminal_workspace.rs"]
mod terminal_workspace;
#[path = "../src/ui.rs"]
mod ui;
mod xylo {
    include!("../src/xylo.rs");
    mod review_tests {
        use super::{
            AnyWindowHandle, App, AppContext, Bounds, ElementId, Entity, HostClient,
            TestAppContext, Uuid, WindowBounds, WindowOptions, Xylo, px, size,
        };
        use crate::recovery_fixture::{self, Profile};
        use gpui_kit::test::{TestQueryExt, TestWindowExt};
        use std::time::{Duration, Instant};
        struct Fixture {
            profile: Profile,
            host: HostClient,
            window: AnyWindowHandle,
            view: Entity<Xylo>,
        }
        fn wait(
            cx: &mut TestAppContext,
            fixture: &Fixture,
            condition: impl Fn(&Xylo, &App) -> bool,
        ) {
            let deadline = Instant::now() + Duration::from_secs(25);
            loop {
                cx.executor().advance_clock(Duration::from_millis(25));
                cx.run_until_parked();
                if cx.update(|cx| condition(fixture.view.read(cx), cx)) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "Chat did not become ready: {:?}",
                    cx.update(|cx| fixture.view.read(cx).error.clone())
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fn open(cx: &mut TestAppContext, count: usize) -> Fixture {
            cx.executor().allow_parking();
            let profile = recovery_fixture::seeded(count);
            let host = recovery_fixture::host(&profile);
            let client = host.0.clone();
            cx.update(gpui_kit::init);
            let (window, view) = cx.update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Default::default(),
                            size: size(px(1500.), px(2000.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| Xylo::new(Ok(host), window, cx)),
                )
                .unwrap()
            });
            let fixture = Fixture {
                profile,
                host: client,
                window,
                view,
            };
            wait(cx, &fixture, |view, _| {
                view.settings_loaded
                    && view.workspace_ready
                    && view
                        .conversations
                        .iter()
                        .any(|conversation| conversation.id == fixture.profile.session)
            });
            cx.update_window(window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.open_conversation(
                        fixture.profile.session,
                        fixture.profile.workspace.clone(),
                        window,
                        cx,
                    )
                })
            })
            .unwrap();
            wait(cx, &fixture, |view, _| {
                !view.history_loading() && !view.messages.is_empty()
            });
            fixture
        }
        #[gpui_kit::test]
        fn unsaved_draft_cache_does_not_lock_chat_navigation(cx: &mut TestAppContext) {
            let fixture = open(cx, 0);
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.host = None; // Exercise pending saves while the service is unavailable.
                    let mut drafts = Vec::new();
                    for _ in 0..140 {
                        view.validation_draft_surface(None, false, window, cx);
                        drafts.push(view.current_draft_key());
                        let previous = view.session;
                        view.new_chat(cx);
                        assert_ne!(
                            view.session, previous,
                            "A full cache must not trap the user in a chat"
                        );
                    }
                    for key in drafts {
                        assert!(
                            view.drafts.validation_dirty(&key),
                            "Unsent drafts must stay cached"
                        );
                    }
                });
            })
            .unwrap();
            shutdown(cx, &fixture);
        }

        #[gpui_kit::test]
        fn unsaved_memory_cache_does_not_lock_project_navigation(cx: &mut TestAppContext) {
            let fixture = open(cx, 0);
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.host = None;
                    for index in 0..70 {
                        view.workspace = Some(format!("/fixture/project-{index}").into());
                        view.memory_form_activate(window, cx);
                        view.memory_content.update(cx, |input, cx| {
                            input.set_value(format!("Note {index}"), window, cx)
                        });
                        assert!(
                            view.memory_form_before_switch(cx),
                            "Pending memory saves must not trap the user in a project"
                        );
                    }
                    view.workspace = Some("/fixture/project-0".into());
                    view.memory_form_activate(window, cx);
                    assert_eq!(view.memory_content.read(cx).value().as_str(), "Note 0");
                });
            })
            .unwrap();
            shutdown(cx, &fixture);
        }

        #[gpui_kit::test]
        fn full_access_keeps_the_unsaved_editor_choice(cx: &mut TestAppContext) {
            let fixture = open(cx, 0);
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.settings.permissions = xylo_core::PermissionMode::FullAccess;
                    view.turns
                        .entry(view.session)
                        .or_default()
                        .apply(xylo_core::EngineEvent::Started);
                    view.editor_dirty = true;
                    view.editor_path = Some(fixture.profile.workspace.join("draft.txt"));
                    view.request_quit(window, cx);
                });
            })
            .unwrap();
            assert!(cx.has_pending_prompt());
            assert_eq!(
                cx.pending_prompt().unwrap().0,
                "Save changes before quitting?",
                "Full access should go straight to the unsaved-edit choice, without a task-stop confirmation"
            );
            cx.simulate_prompt_answer("Cancel");
            cx.run_until_parked();
            assert!(cx.update(|cx| {
                let view = fixture.view.read(cx);
                view.editor_dirty && !view.quitting && !view.quit_prompt_open
            }));
            shutdown(cx, &fixture);
        }

        #[gpui_kit::test]
        fn typing_resumes_work_while_saves_are_pending(cx: &mut TestAppContext) {
            let fixture = open(cx, 0);
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.validation_draft_surface(None, false, window, cx);
                    assert!(!view.flush_drafts(super::DraftFlushAction::Quit, cx));
                    view.composer
                        .update(cx, |input, cx| input.focus(window, cx));
                });
                window.render_frame(cx);
                window.press(
                    if cfg!(target_os = "macos") {
                        "cmd-a"
                    } else {
                        "ctrl-a"
                    },
                    cx,
                );
                window.input("Keep working", cx);
            })
            .unwrap();
            cx.run_until_parked();
            cx.update(|cx| {
                let view = fixture.view.read(cx);
                assert_eq!(
                    view.composer.read(cx).value().as_str(),
                    "Keep working",
                    "The actual composer must remain editable while saving"
                );
                assert!(
                    !view.draft_flush_pending(),
                    "Typing must cancel delayed Quit"
                );
            });
            shutdown(cx, &fixture);
        }

        #[gpui_kit::test]
        fn full_access_runs_explicit_actions_without_an_extra_prompt(cx: &mut TestAppContext) {
            let fixture = open(cx, 0);
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.settings.permissions = xylo_core::PermissionMode::FullAccess;
                    view.error = None;
                    view.confirm_destructive(
                        "Remove item?",
                        "Remove the selected item.",
                        "Remove",
                        window,
                        cx,
                        |view, _, _| view.error = Some("Action ran".into()),
                    );
                });
            })
            .unwrap();
            cx.run_until_parked();
            assert!(
                !cx.has_pending_prompt(),
                "Full access must honor an explicit action without asking again"
            );
            assert_eq!(
                cx.update(|cx| fixture.view.read(cx).error.clone())
                    .as_deref(),
                Some("Action ran")
            );
            shutdown(cx, &fixture);
        }

        #[gpui_kit::test]
        fn guarded_modes_still_honor_cancel_on_explicit_actions(cx: &mut TestAppContext) {
            let fixture = open(cx, 0);
            for mode in [
                xylo_core::PermissionMode::AskForApproval,
                xylo_core::PermissionMode::ApproveForMe,
            ] {
                cx.update_window(fixture.window, |_, window, cx| {
                    fixture.view.update(cx, |view, cx| {
                        view.settings.permissions = mode;
                        view.error = None;
                        view.confirm_destructive(
                            "Remove item?",
                            "Remove the selected item.",
                            "Remove",
                            window,
                            cx,
                            |view, _, _| view.error = Some("Action ran".into()),
                        );
                    });
                })
                .unwrap();
                assert!(cx.has_pending_prompt());
                cx.simulate_prompt_answer("Cancel");
                cx.run_until_parked();
                assert!(cx.update(|cx| fixture.view.read(cx).error.is_none()));
            }
            shutdown(cx, &fixture);
        }

        #[gpui_kit::test]
        fn queued_send_resumes_work_while_background_saves_are_pending(cx: &mut TestAppContext) {
            let fixture = open(cx, 0);
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.validation_draft_surface(None, false, window, cx);
                    assert!(!view.flush_drafts(super::DraftFlushAction::Quit, cx));
                    let id = Uuid::new_v4();
                    view.turn_queue.items.insert(
                        view.session,
                        vec![xylo_runtime::turn_queue::QueuedTurn {
                            id,
                            paused: true,
                            reason: None,
                            turn: xylo_core::TurnRequest {
                                session_id: view.session,
                                turn_id: id,
                                prompt: "Resume queued work".into(),
                                workspace: fixture.profile.workspace.clone(),
                                provider: view.settings.provider.clone(),
                                permissions: view.settings.permissions,
                                attachments: vec![],
                            },
                        }],
                    );
                });
                window.render_frame(cx);
                window.click(("send-queued", 0usize), cx);
                assert!(
                    !fixture.view.read(cx).draft_flush_pending(),
                    "Queued Send must cancel the old Quit request before dispatching"
                );
            })
            .unwrap();
            shutdown(cx, &fixture);
        }

        #[gpui_kit::test]
        fn retry_resumes_work_while_background_saves_are_pending(cx: &mut TestAppContext) {
            let fixture = open(cx, 0);
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.validation_draft_surface(None, false, window, cx);
                    assert!(!view.flush_drafts(super::DraftFlushAction::Quit, cx));
                    view.retry_last_message(cx);
                    assert!(
                        !view.draft_flush_pending(),
                        "Retry expresses the same keep-working intent as Send"
                    );
                });
            })
            .unwrap();
            shutdown(cx, &fixture);
        }

        #[gpui_kit::test]
        fn send_resumes_work_while_background_saves_are_pending(cx: &mut TestAppContext) {
            let fixture = open(cx, 0);
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.validation_draft_surface(None, false, window, cx);
                    assert!(!view.flush_drafts(super::DraftFlushAction::Quit, cx));
                    assert!(view.draft_flush_pending());
                    view.error = None;
                    view.submit(window, cx);
                    assert!(
                        !view.draft_flush_pending(),
                        "Send should resume work without a separate Keep working click"
                    );
                    assert_eq!(view.page, super::Page::Providers);
                });
            })
            .unwrap();
            shutdown(cx, &fixture);
        }

        #[gpui_kit::test]
        fn interrupted_actions_do_not_take_over_chat(cx: &mut TestAppContext) {
            let fixture = open(cx, 1);
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                assert!(window.find("sidebar-new-chat").visible());
                assert!(
                    window.find_all("review-interrupted-actions").is_empty(),
                    "Old interrupted actions must not add a review banner to the chat"
                );
                assert!(window.find_all("action-review-panel").is_empty());
            })
            .unwrap();
            let stored = fixture
                .profile
                .store
                .action(fixture.profile.actions[0])
                .unwrap()
                .unwrap();
            assert_eq!(stored.state, "outcome_unknown");
            let reopened = xylo_services::Store::open(&fixture.profile.paths.database).unwrap();
            assert!(
                reopened
                    .action(fixture.profile.actions[0])
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                std::fs::read_to_string(fixture.profile.workspace.join("partial-0.txt")).unwrap(),
                "partial content remains"
            );
            assert!(
                fixture
                    .profile
                    .paths
                    .artifacts
                    .join("retained-0.txt")
                    .is_file()
            );
            shutdown(cx, &fixture);
        }
        fn shutdown(cx: &mut TestAppContext, fixture: &Fixture) {
            fixture.host.shutdown().unwrap();
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(fixture.profile.paths.data.join("host.lock"))
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                cx.run_until_parked();
                if lock.try_lock().is_ok() {
                    break;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        #[gpui_kit::test]
        fn interrupted_actions_allow_normal_submission_and_chat_switching(cx: &mut TestAppContext) {
            // Previous tool activity must not survive reopening or gate the chat.
            let fixture = open(cx, 55);
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.error = None;
                    view.composer.update(cx, |input, cx| {
                        input.set_value("Continue working on this project", window, cx)
                    });
                    view.submit(window, cx);
                    // This fixture has no selected model. Reaching ordinary provider
                    // validation proves that review state did not intercept Send.
                    assert_eq!(view.page, super::Page::Providers);
                    assert!(
                        view.error
                            .as_deref()
                            .is_some_and(|error| error.contains("Choose a model"))
                    );
                    assert_eq!(
                        view.composer.read(cx).value().as_str(),
                        "Continue working on this project"
                    );
                    view.new_chat(cx);
                    assert_ne!(view.session, fixture.profile.session);
                });
            })
            .unwrap();
            let reopened = xylo_services::Store::open(&fixture.profile.paths.database).unwrap();
            for action in &fixture.profile.actions {
                assert!(reopened.action(*action).unwrap().is_none());
                assert_eq!(
                    fixture
                        .profile
                        .store
                        .action(*action)
                        .unwrap()
                        .unwrap()
                        .state,
                    "outcome_unknown"
                );
            }
            shutdown(cx, &fixture);
        }
    }
}
