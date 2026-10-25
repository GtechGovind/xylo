//! Real native controls and durable action/receipt/form storage, without models.
#![allow(dead_code, unused_imports)]
extern crate gpui_kit as gpui;
mod recovery_fixture;
#[path = "../src/state.rs"]
mod state;
#[path = "../src/terminal_view.rs"]
mod terminal_view;
#[path = "../src/terminal_workspace.rs"]
mod terminal_workspace;
#[path = "../src/ui.rs"]
mod ui;
mod workbench {
    include!("../src/workbench.rs");
    mod review_tests {
        use super::{
            AnyWindowHandle, App, AppContext, Bounds, ElementId, Entity, HostClient,
            TestAppContext, Uuid, WindowBounds, WindowOptions, Workbench, px, size,
        };
        use crate::recovery_fixture::{self, Delivery, Profile};
        use gpui_kit::test::{TestQueryExt, TestWindowExt};
        use std::{
            sync::{Arc, atomic::Ordering},
            time::{Duration, Instant},
        };
        use workbench_runtime::{
            ActionResolutionOutcome, ActionReviewFormWrite, ActionReviewPage, ActionReviewScope,
        };
        struct Fixture {
            profile: Profile,
            host: HostClient,
            delivery: Arc<Delivery>,
            window: AnyWindowHandle,
            view: Entity<Workbench>,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                self.delivery.release.store(true, Ordering::SeqCst);
            }
        }
        fn wait(
            cx: &mut TestAppContext,
            fixture: &Fixture,
            condition: impl Fn(&Workbench, &App) -> bool,
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
                    "Recovery UI timed out: {}",
                    cx.update(|cx| fixture.view.read(cx).validation_review_notice().to_owned())
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fn open(cx: &mut TestAppContext, count: usize) -> Fixture {
            cx.executor().allow_parking();
            let profile = recovery_fixture::seeded(count);
            let delivery = Arc::new(Delivery::default());
            let host = recovery_fixture::host(&profile, delivery.clone());
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
                    |window, cx| cx.new(|cx| Workbench::new(Ok(host), window, cx)),
                )
                .unwrap()
            });
            let fixture = Fixture {
                profile,
                host: client,
                delivery,
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
            wait(cx, &fixture, |view, _| view.validation_review_ready());
            fixture
        }
        fn click(cx: &mut TestAppContext, fixture: &Fixture, id: impl Into<ElementId>) {
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                window.click(id, cx);
            })
            .unwrap();
            cx.run_until_parked();
        }
        fn choose(cx: &mut TestAppContext, fixture: &Fixture, action: Uuid) {
            click(cx, fixture, "review-interrupted-actions");
            let index = cx.update(|cx| {
                fixture
                    .view
                    .read(cx)
                    .validation_review_items()
                    .iter()
                    .position(|item| item.id == action)
                    .unwrap()
            });
            click(cx, fixture, ("review-select-action", index));
            wait(cx, fixture, |view, _| {
                view.validation_review_ready()
                    && view
                        .validation_review_detail()
                        .is_some_and(|detail| detail.action.id == action)
            });
        }
        fn note(cx: &mut TestAppContext, fixture: &Fixture, text: &str) {
            cx.update_window(fixture.window, |_, window, cx| {
                fixture
                    .view
                    .update(cx, |view, cx| view.validation_review_focus(window, cx));
                window.render_frame(cx);
                window.press("cmd-a", cx);
                window.input(text, cx);
            })
            .unwrap();
            cx.run_until_parked();
        }
        fn confirm(cx: &mut TestAppContext, fixture: &Fixture, outcome: usize) {
            click(cx, fixture, ("review-observed-outcome", outcome));
            click(cx, fixture, "review-record-outcome");
            assert!(cx.has_pending_prompt());
            cx.simulate_prompt_answer("Record outcome");
        }
        fn shutdown(cx: &mut TestAppContext, fixture: &Fixture) {
            fixture.delivery.release.store(true, Ordering::SeqCst);
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
        fn observed_failure_preserves_partial_write_observation_and_artifact(
            cx: &mut TestAppContext,
        ) {
            let fixture = open(cx, 1);
            let action = fixture.profile.actions[0];
            choose(cx, &fixture, action);
            note(
                cx,
                &fixture,
                "I opened the project file and retained output. The update stopped after partial content.",
            );
            confirm(cx, &fixture, 1);
            wait(cx, &fixture, |view, _| {
                view.validation_review_form()
                    .is_some_and(|form| form.pending.is_none())
                    && view
                        .validation_review_detail()
                        .is_some_and(|detail| detail.resolution.is_some())
            });
            let stored = fixture.profile.store.action(action).unwrap().unwrap();
            assert_eq!(stored.state, "failed");
            assert_eq!(
                stored.result.as_ref().unwrap()["original_observation"]["status"],
                "outcome_unknown"
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
            assert_eq!(
                stored.result.unwrap()["operator_observation"]["native_execution_verified"],
                false
            );
            shutdown(cx, &fixture);
        }
        #[gpui_kit::test]
        fn lost_ack_restart_restores_exact_request_and_preserves_newer_note(
            cx: &mut TestAppContext,
        ) {
            let mut fixture = open(cx, 1);
            let action = fixture.profile.actions[0];
            choose(cx, &fixture, action);
            note(
                cx,
                &fixture,
                "Inspected the incomplete file before recording this observation.",
            );
            fixture.delivery.drop_next.store(true, Ordering::SeqCst);
            confirm(cx, &fixture, 1);
            wait(cx, &fixture, |_, _| {
                fixture.delivery.dropped.load(Ordering::SeqCst)
            });
            let pending = cx.update(|cx| {
                fixture
                    .view
                    .read(cx)
                    .validation_review_form()
                    .unwrap()
                    .pending
                    .unwrap()
            });
            note(
                cx,
                &fixture,
                "A newer note typed while the earlier acknowledgment was missing.",
            );
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    let session = view.session;
                    view.new_chat(cx);
                    assert_eq!(view.session, session);
                    view.request_quit(window, cx);
                    assert!(!view.quitting);
                })
            })
            .unwrap();
            click(cx, &fixture, "review-cancel-request");
            wait(cx, &fixture, |view, _| view.validation_review_ready());
            shutdown(cx, &fixture);
            fixture.delivery = Arc::new(Delivery::default());
            let host = recovery_fixture::host(&fixture.profile, fixture.delivery.clone());
            fixture.host = host.0.clone();
            cx.update_window(fixture.window, |_, window, cx| {
                fixture
                    .view
                    .update(cx, |view, cx| *view = Workbench::new(Ok(host), window, cx))
            })
            .unwrap();
            wait(cx, &fixture, |view, _| {
                view.settings_loaded
                    && view.workspace_ready
                    && view
                        .conversations
                        .iter()
                        .any(|conversation| conversation.id == fixture.profile.session)
            });
            cx.update_window(fixture.window, |_, window, cx| {
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
            wait(cx, &fixture, |view, cx| {
                view.validation_review_ready()
                    && view
                        .validation_review_form()
                        .is_some_and(|form| form.pending.is_none())
                    && view.validation_review_note(cx).contains("newer note")
            });
            let receipt = fixture
                .profile
                .store
                .action_resolution(pending.scope.clone(), pending.operation_id)
                .unwrap()
                .unwrap();
            assert_eq!(receipt.operation_id, pending.operation_id);
            assert_eq!(receipt.evidence, pending.evidence);
            assert!(cx.update(|cx| {
                fixture
                    .view
                    .read(cx)
                    .validation_review_note(cx)
                    .contains("newer note")
            }));
            shutdown(cx, &fixture);
        }
        #[gpui_kit::test]
        fn cancellation_before_commit_keeps_exact_request_until_explicit_same_outcome_save(
            cx: &mut TestAppContext,
        ) {
            let fixture = open(cx, 1);
            let action = fixture.profile.actions[0];
            choose(cx, &fixture, action);
            note(cx, &fixture, "I inspected the partial project file.");
            fixture.delivery.pause_pending.store(true, Ordering::SeqCst);
            confirm(cx, &fixture, 1);
            wait(cx, &fixture, |view, _| {
                fixture.delivery.locked.load(Ordering::SeqCst)
                    && view.validation_review_recording().is_some()
            });
            let pending = cx.update(|cx| {
                fixture
                    .view
                    .read(cx)
                    .validation_review_form()
                    .unwrap()
                    .pending
                    .unwrap()
            });
            note(
                cx,
                &fixture,
                "New typing while outcome storage waits for its write lock.",
            );
            click(cx, &fixture, "review-cancel-request");
            fixture.delivery.release.store(true, Ordering::SeqCst);
            wait(cx, &fixture, |view, _| view.validation_review_ready());
            click(cx, &fixture, "review-check-outcome");
            wait(cx, &fixture, |view, _| view.validation_review_ready());
            assert!(
                fixture
                    .profile
                    .store
                    .action_resolution(pending.scope.clone(), pending.operation_id)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                cx.update(|cx| fixture
                    .view
                    .read(cx)
                    .validation_review_form()
                    .unwrap()
                    .pending
                    .unwrap()),
                pending
            );
            click(cx, &fixture, "review-save-original-outcome");
            wait(cx, &fixture, |view, _| {
                view.validation_review_ready()
                    && view
                        .validation_review_form()
                        .is_some_and(|form| form.pending.is_none())
            });
            let receipt = fixture
                .profile
                .store
                .action_resolution(pending.scope.clone(), pending.operation_id)
                .unwrap()
                .unwrap();
            assert_eq!(receipt.evidence, pending.evidence);
            assert!(cx.update(|cx| {
                fixture
                    .view
                    .read(cx)
                    .validation_review_note(cx)
                    .starts_with("New typing")
            }));
            assert_eq!(
                std::fs::read_to_string(fixture.profile.workspace.join("partial-0.txt")).unwrap(),
                "partial content remains"
            );
            shutdown(cx, &fixture);
        }
        #[gpui_kit::test]
        fn pages_are_virtualized_and_wrong_scope_or_stale_replies_cannot_replace_actions(
            cx: &mut TestAppContext,
        ) {
            let fixture = open(cx, 55);
            click(cx, &fixture, "review-interrupted-actions");
            assert_eq!(
                cx.update(|cx| fixture.view.read(cx).validation_review_items().len()),
                50
            );
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                assert!(window.find_all_by_label("Update project files").len() < 50);
                fixture.view.update(cx, |view, cx| {
                    view.refresh_action_reviews(cx);
                    let id = view.validation_review_read().unwrap();
                    view.action_reviews_received(
                        id,
                        Ok(ActionReviewPage {
                            scope: ActionReviewScope {
                                session_id: Uuid::new_v4(),
                                workspace: fixture.profile.workspace.clone(),
                            },
                            items: vec![],
                            cursor: None,
                            has_more: false,
                        }),
                        cx,
                    );
                    assert_eq!(view.validation_review_items().len(), 50);
                });
            })
            .unwrap();
            click(cx, &fixture, "review-next-page");
            wait(cx, &fixture, |view, _| {
                view.validation_review_ready() && view.validation_review_items().len() == 5
            });
            let before = cx.update(|cx| fixture.view.read(cx).validation_review_items());
            cx.update_window(fixture.window, |_, _, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.action_reviews_received(
                        Uuid::new_v4(),
                        Err("malformed retired reply".into()),
                        cx,
                    )
                })
            })
            .unwrap();
            assert_eq!(
                cx.update(|cx| fixture.view.read(cx).validation_review_items()),
                before
            );
            click(cx, &fixture, "review-first-page");
            wait(cx, &fixture, |view, _| {
                view.validation_review_ready() && view.validation_review_items().len() == 50
            });
            shutdown(cx, &fixture);
        }
        #[gpui_kit::test]
        fn saved_note_conflict_requires_explicit_choice_and_keeps_current_typing(
            cx: &mut TestAppContext,
        ) {
            let fixture = open(cx, 1);
            choose(cx, &fixture, fixture.profile.actions[0]);
            let scope = ActionReviewScope {
                session_id: fixture.profile.session,
                workspace: fixture.profile.workspace.clone(),
            };
            let mut saved = fixture
                .profile
                .store
                .action_review_form(scope.clone())
                .unwrap();
            saved.form.evidence = "Saved in another window".into();
            fixture
                .profile
                .store
                .save_action_review_form(ActionReviewFormWrite {
                    scope: scope.clone(),
                    expected_revision: saved.revision,
                    form: saved.form,
                })
                .unwrap();
            note(cx, &fixture, "My current observation stays here.");
            wait(cx, &fixture, |view, _| {
                view.validation_review_notice().is_empty() && view.validation_review_conflict()
            });
            click(cx, &fixture, "review-use-saved");
            assert!(cx.has_pending_prompt());
            cx.simulate_prompt_answer("Keep my note");
            cx.run_until_parked();
            assert_eq!(
                cx.update(|cx| fixture.view.read(cx).validation_review_note(cx)),
                "My current observation stays here."
            );
            click(cx, &fixture, "review-keep-note");
            wait(cx, &fixture, |view, _| view.validation_review_ready());
            assert_eq!(
                fixture
                    .profile
                    .store
                    .action_review_form(scope)
                    .unwrap()
                    .form
                    .evidence,
                "My current observation stays here."
            );
            shutdown(cx, &fixture);
        }

        #[gpui_kit::test]
        fn executing_and_unqualified_actions_cannot_record_an_outcome(cx: &mut TestAppContext) {
            let mut fixture = open(cx, 1);
            let action = fixture.profile.actions[0];
            let lease = fixture.profile.store.lock_action_execution(action).unwrap();
            choose(cx, &fixture, action);
            assert!(cx.update(|cx| {
                fixture
                    .view
                    .read(cx)
                    .validation_review_detail()
                    .unwrap()
                    .action
                    .busy
            }));
            note(
                cx,
                &fixture,
                "I inspected the target while work was still active.",
            );
            click(cx, &fixture, ("review-observed-outcome", 1usize));
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                assert_eq!(window.find("review-record-outcome").disabled(), Some(true));
            })
            .unwrap();
            drop(lease);
            wait(cx, &fixture, |view, _| view.validation_review_ready());
            let native = recovery_fixture::seed_action(
                &mut fixture.profile,
                "plugin_run_installed",
                false,
                9,
            );
            click(cx, &fixture, "review-refresh");
            wait(cx, &fixture, |view, _| {
                view.validation_review_ready() && view.validation_review_items().len() == 2
            });
            let index = cx.update(|cx| {
                fixture
                    .view
                    .read(cx)
                    .validation_review_items()
                    .iter()
                    .position(|item| item.id == native)
                    .unwrap()
            });
            click(cx, &fixture, ("review-select-action", index));
            assert!(cx.has_pending_prompt());
            cx.simulate_prompt_answer("Discard note");
            wait(cx, &fixture, |view, _| {
                view.validation_review_ready()
                    && view
                        .validation_review_detail()
                        .is_some_and(|detail| detail.action.id == native)
            });
            let detail = cx.update(|cx| fixture.view.read(cx).validation_review_detail().unwrap());
            assert!(!detail.action.busy);
            let observation = cx.update(|cx| fixture.view.read(cx).validation_review_output(cx));
            assert!(!observation.contains(&native.to_string()));
            assert!(!observation.contains("fuel_remaining"));
            assert!(
                detail
                    .action
                    .blocked_reason
                    .unwrap()
                    .contains("service where it ran")
            );
            note(
                cx,
                &fixture,
                "This requires checking the connected service.",
            );
            click(cx, &fixture, ("review-observed-outcome", 1usize));
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                assert_eq!(window.find("review-record-outcome").disabled(), Some(true));
            })
            .unwrap();
            assert_eq!(
                fixture.profile.store.action(native).unwrap().unwrap().state,
                "outcome_unknown"
            );
            shutdown(cx, &fixture);
        }
        #[gpui_kit::test]
        fn malformed_ack_keeps_exact_pending_request_until_authoritative_lookup(
            cx: &mut TestAppContext,
        ) {
            let fixture = open(cx, 1);
            let action = fixture.profile.actions[0];
            choose(cx, &fixture, action);
            note(
                cx,
                &fixture,
                "I checked the actual target and retained files.",
            );
            fixture.delivery.drop_next.store(true, Ordering::SeqCst);
            confirm(cx, &fixture, 0);
            wait(cx, &fixture, |_, _| {
                fixture.delivery.dropped.load(Ordering::SeqCst)
            });
            let (id, pending) =
                cx.update(|cx| fixture.view.read(cx).validation_review_recording().unwrap());
            let mut receipt = fixture
                .profile
                .store
                .action_resolution(pending.scope.clone(), pending.operation_id)
                .unwrap()
                .unwrap();
            receipt.scope.session_id = Uuid::new_v4();
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.action_resolved(id, Ok(receipt), window, cx)
                })
            })
            .unwrap();
            assert_eq!(
                cx.update(|cx| fixture
                    .view
                    .read(cx)
                    .validation_review_form()
                    .unwrap()
                    .pending
                    .unwrap()),
                pending
            );
            assert!(cx.update(|cx| {
                fixture
                    .view
                    .read(cx)
                    .validation_review_notice()
                    .contains("did not match")
            }));
            click(cx, &fixture, "review-check-outcome");
            wait(cx, &fixture, |view, _| {
                view.validation_review_ready()
                    && view.validation_review_form().unwrap().pending.is_none()
            });
            let stored = fixture.profile.store.action(action).unwrap().unwrap();
            assert_eq!(stored.state, "succeeded");
            assert_eq!(
                stored.result.unwrap()["operator_observation"]["native_execution_verified"],
                false
            );
            assert_eq!(
                std::fs::read_to_string(fixture.profile.workspace.join("partial-0.txt")).unwrap(),
                "partial content remains"
            );
            shutdown(cx, &fixture);
        }

        #[gpui_kit::test]
        fn lost_note_save_ack_preserves_later_typing_and_explicit_exact_cas_retry(
            cx: &mut TestAppContext,
        ) {
            let fixture = open(cx, 1);
            choose(cx, &fixture, fixture.profile.actions[0]);
            fixture
                .delivery
                .drop_form_next
                .store(true, Ordering::SeqCst);
            note(
                cx,
                &fixture,
                "The first observation was persisted before its acknowledgment was lost.",
            );
            wait(cx, &fixture, |_, _| {
                fixture.delivery.form_dropped.load(Ordering::SeqCst)
            });
            note(
                cx,
                &fixture,
                "A newer note remains editable while the first acknowledgment is missing.",
            );
            cx.update_window(fixture.window, |_, _, cx| {
                fixture.view.update(cx, |view, cx| {
                    let session = view.session;
                    view.new_chat(cx);
                    assert_eq!(session, view.session);
                })
            })
            .unwrap();
            cx.executor().advance_clock(Duration::from_secs(31));
            cx.run_until_parked();
            click(cx, &fixture, "review-save-note");
            wait(cx, &fixture, |view, cx| {
                view.validation_review_ready()
                    && view.validation_review_note(cx).starts_with("A newer note")
            });
            let scope = ActionReviewScope {
                session_id: fixture.profile.session,
                workspace: fixture.profile.workspace.clone(),
            };
            assert_eq!(
                fixture
                    .profile
                    .store
                    .action_review_form(scope)
                    .unwrap()
                    .form
                    .evidence,
                "A newer note remains editable while the first acknowledgment is missing."
            );
            assert_eq!(
                fixture
                    .profile
                    .store
                    .action(fixture.profile.actions[0])
                    .unwrap()
                    .unwrap()
                    .state,
                "outcome_unknown"
            );
            shutdown(cx, &fixture);
        }
        #[gpui_kit::test]
        fn active_turn_ui_state_blocks_recording_and_keeps_the_composer_message(
            cx: &mut TestAppContext,
        ) {
            // UI-state guard only. Store/Host/worker admission is independently
            // covered by the backend integration tests, not inferred from this event.
            let fixture = open(cx, 1);
            choose(cx, &fixture, fixture.profile.actions[0]);
            note(
                cx,
                &fixture,
                "I inspected the target while later work was active.",
            );
            click(cx, &fixture, ("review-observed-outcome", 1usize));
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.turns
                        .entry(view.session)
                        .or_default()
                        .apply(workbench_core::EngineEvent::Started);
                    view.composer.update(cx, |input, cx| {
                        input.set_value("A message that must stay unsent", window, cx)
                    });
                    view.submit(window, cx);
                    assert_eq!(
                        view.composer.read(cx).value().as_str(),
                        "A message that must stay unsent"
                    );
                })
            })
            .unwrap();
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                assert_eq!(window.find("review-record-outcome").disabled(), Some(true));
            })
            .unwrap();
            assert!(!cx.has_pending_prompt());
            assert_eq!(
                fixture
                    .profile
                    .store
                    .action(fixture.profile.actions[0])
                    .unwrap()
                    .unwrap()
                    .state,
                "outcome_unknown"
            );
            shutdown(cx, &fixture);
        }
    }
}
