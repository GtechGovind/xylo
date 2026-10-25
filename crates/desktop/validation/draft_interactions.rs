//! Real SQLite/host draft restoration, correlated native composer interactions,
//! and teardown flush. No inference, accounts, updater install or devices.
#![allow(dead_code, unused_imports)]
extern crate gpui_kit as gpui;
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
    #[cfg(test)]
    mod draft_native_tests {
        use super::{DraftFlushAction, Page, Workbench};
        use gpui_kit::{
            AppContext, Bounds, TestAppContext, WindowBounds, WindowOptions, px, size,
            test::TestWindowExt,
        };
        use std::time::{Duration, Instant};
        use uuid::Uuid;
        use workbench_runtime::{
            AppPaths,
            drafts::{DraftKey, DraftSave, DraftSnapshot, DraftWrite},
            host::HostClient,
        };
        use workbench_services::Store;
        fn wait(
            cx: &mut TestAppContext,
            view: &gpui_kit::Entity<Workbench>,
            description: &str,
            condition: impl Fn(&Workbench, &gpui_kit::App) -> bool,
        ) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                cx.run_until_parked();
                if cx.update(|cx| condition(view.read(cx), cx)) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "Timed out waiting for {description}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fn open(
            cx: &mut TestAppContext,
            host: anyhow::Result<(
                HostClient,
                async_channel::Receiver<workbench_runtime::host::HostEvent>,
            )>,
        ) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<Workbench>) {
            cx.update(gpui_kit::init);
            cx.update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Default::default(),
                            size: size(px(1280.), px(900.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| Workbench::new(host, window, cx)),
                )
                .unwrap()
            })
        }
        fn save(store: &Store, key: &DraftKey, text: &str) -> DraftSnapshot {
            let request = DraftWrite {
                key: key.clone(),
                expected_revision: store.draft(key).unwrap().revision,
                mutation_id: Uuid::new_v4(),
                text: text.into(),
                attachments: Vec::new(),
            };
            match store.save_draft(&request).unwrap() {
                DraftSave::Saved(snapshot) => snapshot,
                _ => panic!("Unexpected conflict"),
            }
        }
        #[gpui_kit::test]
        fn terminal_assistance_preserves_the_original_draft_and_requires_normal_send(
            cx: &mut TestAppContext,
        ) {
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let paths = AppPaths::at(directory.path().join("profile")).unwrap();
            let store = Store::open(&paths.database).unwrap();
            let (window, view) = open(cx, HostClient::spawn_at(paths));
            wait(cx, &view, "initial draft", |view, _| {
                view.drafts
                    .validation_revision(&DraftKey {
                        session_id: view.session,
                        workspace: view.workspace.clone(),
                    })
                    .is_some()
            });
            let old = cx.update(|cx| {
                let view = view.read(cx);
                DraftKey {
                    session_id: view.session,
                    workspace: view.workspace.clone(),
                }
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.composer.update(cx, |input, cx| {
                        input.set_value("Keep my existing unsent message", window, cx)
                    });
                    view.draft_changed(cx);
                    view.terminal_assistance_chat(
                        "Explain this selected terminal output".into(),
                        window,
                        cx,
                    );
                    assert_ne!(view.session, old.session_id);
                    assert_eq!(
                        view.composer.read(cx).value().as_ref(),
                        "Explain this selected terminal output"
                    );
                    assert!(
                        view.turns.is_empty(),
                        "Opening assistance must not submit a turn or execute a tool"
                    );
                })
            })
            .unwrap();
            let new = cx.update(|cx| {
                let view = view.read(cx);
                DraftKey {
                    session_id: view.session,
                    workspace: view.workspace.clone(),
                }
            });
            wait(cx, &view, "both saved drafts", |view, _| {
                view.drafts.validation_revision(&new).is_some()
                    && !view.drafts.validation_dirty(&old)
                    && !view.drafts.validation_dirty(&new)
            });
            assert_eq!(
                store.draft(&old).unwrap().text,
                "Keep my existing unsent message"
            );
            assert_eq!(
                store.draft(&new).unwrap().text,
                "Explain this selected terminal output"
            );
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    let session = view.session;
                    view.terminal_assistance_chat("   ".into(), window, cx);
                    assert_eq!(
                        view.session, session,
                        "An empty question must not open another chat"
                    );
                })
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn restarted_database_drafts_restore_in_native_chat_picker_and_cross_chat_switches(
            cx: &mut TestAppContext,
        ) {
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let base = directory.path().canonicalize().unwrap();
            let workspace = base.join("project");
            std::fs::create_dir(&workspace).unwrap();
            let paths = AppPaths::at(base.join("profile")).unwrap();
            let first = DraftKey {
                session_id: Uuid::new_v4(),
                workspace: Some(workspace.clone()),
            };
            let second = DraftKey {
                session_id: Uuid::new_v4(),
                workspace: Some(workspace.clone()),
            };
            let unscoped = DraftKey {
                session_id: Uuid::new_v4(),
                workspace: None,
            };
            {
                let store = Store::open(&paths.database).unwrap();
                save(&store, &first, "First saved αβ 🦀 message");
                save(&store, &second, "Second saved message");
                save(&store, &unscoped, "Written before choosing a project");
                assert!(store.conversations(10).unwrap().is_empty());
            }
            let store = Store::open(&paths.database).unwrap();
            let host = HostClient::spawn_at(paths).unwrap();
            let client = host.0.clone();
            let (window, view) = open(cx, Ok(host));
            wait(cx, &view, "initial host", |view, _| view.settings_loaded);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.set_workspace(workspace.clone(), window, cx);
                    view.open_saved_drafts(cx);
                });
            })
            .unwrap();
            wait(cx, &view, "saved draft picker", |view, _| {
                view.drafts.validation_summaries().len() == 2
                    && view.drafts.validation_list_request().is_none()
            });
            cx.update_window(window, |_, window, cx| {
                let index = view
                    .read(cx)
                    .drafts
                    .validation_summaries()
                    .iter()
                    .position(|draft| draft.key == first)
                    .unwrap();
                window.render_frame(cx);
                window.click(("open-saved-draft", index), cx);
            })
            .unwrap();
            wait(cx, &view, "first restored composer", |view, cx| {
                view.session == first.session_id
                    && view.composer.read(cx).value().as_ref() == "First saved αβ 🦀 message"
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.composer.update(cx, |input, cx| {
                        input.set_value("First edited αβ 🦀 message", window, cx)
                    });
                    view.draft_changed(cx);
                    view.open_conversation(second.session_id, workspace.clone(), window, cx);
                });
            })
            .unwrap();
            wait(cx, &view, "second chat restore", |view, cx| {
                view.session == second.session_id
                    && view.composer.read(cx).value().as_ref() == "Second saved message"
            });
            wait(cx, &view, "previous chat save ack", |view, _| {
                !view.drafts.validation_dirty(&first)
            });
            assert_eq!(
                store.draft(&first).unwrap().text,
                "First edited αβ 🦀 message"
            );
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.draft_loaded(
                        Uuid::new_v4(),
                        Ok(DraftSnapshot {
                            key: first.clone(),
                            text: "Stale reply must not appear".into(),
                            attachments: Vec::new(),
                            revision: 1,
                            updated_at: 7,
                        }),
                        window,
                        cx,
                    );
                    assert_eq!(
                        view.composer.read(cx).value().as_ref(),
                        "Second saved message"
                    );
                    view.open_conversation(first.session_id, workspace.clone(), window, cx);
                });
            })
            .unwrap();
            wait(cx, &view, "edited draft restored on return", |view, cx| {
                view.session == first.session_id
                    && view.composer.read(cx).value().as_ref() == "First edited αβ 🦀 message"
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| view.open_saved_drafts(cx));
                window.render_frame(cx);
            })
            .unwrap();
            wait(cx, &view, "project draft list reload", |view, _| {
                view.drafts.validation_summaries().len() == 2
                    && view.drafts.validation_list_request().is_none()
            });
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.click("drafts-without-project", cx);
            })
            .unwrap();
            wait(cx, &view, "draft before choosing a project", |view, _| {
                view.drafts.validation_summaries().len() == 1
                    && view.drafts.validation_summaries()[0].key == unscoped
            });
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.click(("open-saved-draft", 0usize), cx);
            })
            .unwrap();
            wait(cx, &view, "original unscoped draft restore", |view, cx| {
                view.workspace.is_none()
                    && view.session == unscoped.session_id
                    && view.composer.read(cx).value().as_ref()
                        == "Written before choosing a project"
            });
            client.shutdown().unwrap();
        }
        #[gpui_kit::test]
        fn quit_waits_for_latest_durable_draft_ack_and_cancellation_keeps_working(
            cx: &mut TestAppContext,
        ) {
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let base = directory.path().canonicalize().unwrap();
            let workspace = base.join("project");
            std::fs::create_dir(&workspace).unwrap();
            let paths = AppPaths::at(base.join("profile")).unwrap();
            let store = Store::open(&paths.database).unwrap();
            let host = HostClient::spawn_at(paths).unwrap();
            let (window, view) = open(cx, Ok(host));
            wait(cx, &view, "host settings", |view, _| view.settings_loaded);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.set_workspace(workspace.clone(), window, cx)
                });
            })
            .unwrap();
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.composer.update(cx, |input, cx| {
                        input.set_value("Save this before closing αβ 🦀", window, cx)
                    });
                    view.draft_changed(cx);
                    assert!(!view.flush_drafts(DraftFlushAction::Quit, cx));
                    assert!(!view.quitting, "No teardown before durable acknowledgement");
                    assert!(view.draft_flush_pending());
                    view.cancel_draft_flush(cx);
                });
            })
            .unwrap();
            let key = cx.update(|cx| DraftKey {
                session_id: view.read(cx).session,
                workspace: view.read(cx).workspace.clone(),
            });
            wait(cx, &view, "cancelled-close persistence", |view, _| {
                !view.drafts.validation_dirty(&key)
            });
            cx.update(|cx| {
                assert!(!view.read(cx).quitting);
                assert_eq!(
                    view.read(cx).composer.read(cx).value().as_ref(),
                    "Save this before closing αβ 🦀"
                );
            });
            assert_eq!(
                store.draft(&key).unwrap().text,
                "Save this before closing αβ 🦀"
            );
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.composer.update(cx, |input, cx| {
                        input.set_value("Newest text typed immediately before Quit", window, cx)
                    });
                    view.draft_changed(cx);
                    view.request_quit(window, cx);
                    assert!(!view.quitting);
                });
            })
            .unwrap();
            wait(cx, &view, "quit after latest commit", |view, _| {
                view.quitting
            });
            assert_eq!(
                store.draft(&key).unwrap().text,
                "Newest text typed immediately before Quit"
            );
        }
        #[gpui_kit::test]
        fn in_flight_save_cannot_replace_newer_native_typing(cx: &mut TestAppContext) {
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let paths = AppPaths::at(directory.path().canonicalize().unwrap()).unwrap();
            let store = Store::open(&paths.database).unwrap();
            let host = HostClient::spawn_at(paths).unwrap();
            let client = host.0.clone();
            let (window, view) = open(cx, Ok(host));
            wait(cx, &view, "initial settings", |view, _| {
                view.settings_loaded
            });
            let key = cx.update(|cx| DraftKey {
                session_id: view.read(cx).session,
                workspace: None,
            });
            wait(cx, &view, "initial draft revision", |view, _| {
                view.drafts.validation_revision(&key).is_some()
            });
            let oversized = "x".repeat(workbench_runtime::drafts::MAX_DRAFT_BYTES + 1);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.composer.update(cx, |input, cx| {
                        input.set_value("First queued value", window, cx)
                    });
                    view.draft_changed(cx);
                    assert!(view.draft_before_switch(cx));
                    assert!(view.drafts.validation_save(&key).is_some());
                    view.composer.update(cx, |input, cx| {
                        input.set_value(oversized.clone(), window, cx)
                    });
                });
            })
            .unwrap();
            wait(
                cx,
                &view,
                "earlier write acknowledged while oversized input remains",
                |view, _| {
                    view.drafts.validation_save(&key).is_none()
                        && view.drafts.validation_revision(&key) == Some(1)
                },
            );
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    assert_eq!(view.composer.read(cx).value().len(), oversized.len());
                    assert!(
                        !view.draft_before_switch(cx),
                        "Oversized unsaved input must prevent a destructive switch"
                    );
                    view.composer.update(cx, |input, cx| {
                        input.set_value("Newest native input αβ 🦀", window, cx)
                    });
                    view.draft_changed(cx);
                });
            })
            .unwrap();
            cx.executor().advance_clock(Duration::from_millis(400));
            wait(cx, &view, "latest coalesced save", |view, _| {
                !view.drafts.validation_dirty(&key)
                    && store.draft(&key).unwrap().text == "Newest native input αβ 🦀"
            });
            cx.update(|cx| {
                assert_eq!(
                    view.read(cx).composer.read(cx).value().as_ref(),
                    "Newest native input αβ 🦀"
                )
            });
            assert_eq!(store.draft(&key).unwrap().revision, 2);
            client.shutdown().unwrap();
        }
        #[gpui_kit::test]
        fn failed_durable_save_blocks_quit_without_discarding_visible_text(
            cx: &mut TestAppContext,
        ) {
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let base = directory.path().canonicalize().unwrap();
            let paths = AppPaths::at(base.join("profile")).unwrap();
            let store = Store::open(&paths.database).unwrap();
            let key = DraftKey {
                session_id: Uuid::new_v4(),
                workspace: None,
            };
            save(&store, &key, "Previously durable text");
            let connection = rusqlite::Connection::open(&paths.database).unwrap();
            connection.execute_batch("CREATE TRIGGER reject_draft_write BEFORE UPDATE ON composer_drafts BEGIN SELECT RAISE(ABORT,'fixture unavailable storage'); END;").unwrap();
            drop(connection);
            let host = HostClient::spawn_at(paths).unwrap();
            let client = host.0.clone();
            let (window, view) = open(cx, Ok(host));
            wait(cx, &view, "host settings", |view, _| view.settings_loaded);
            cx.update_window(window, |_, _, cx| {
                view.update(cx, |view, cx| {
                    assert!(view.draft_before_switch(cx));
                    view.session = key.session_id;
                    view.draft_switch_deferred(cx);
                });
            })
            .unwrap();
            wait(cx, &view, "original saved draft", |view, cx| {
                view.composer.read(cx).value().as_ref() == "Previously durable text"
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.composer.update(cx, |input, cx| {
                        input.set_value("Keep these unsaved edits", window, cx)
                    });
                    view.draft_changed(cx);
                    view.request_quit(window, cx);
                });
            })
            .unwrap();
            wait(cx, &view, "failed flush acknowledgement", |view, _| {
                view.drafts.validation_dirty(&key) && view.drafts.validation_save(&key).is_none()
            });
            cx.update_window(window, |_, window, cx| {
                assert!(!view.read(cx).quitting);
                assert_eq!(
                    view.read(cx).composer.read(cx).value().as_ref(),
                    "Keep these unsaved edits"
                );
                window.render_frame(cx);
                assert!(window.try_find("draft-cancel-close").is_some());
                window.click("draft-cancel-close", cx);
                assert!(!view.read(cx).draft_flush_pending());
            })
            .unwrap();
            assert_eq!(store.draft(&key).unwrap().text, "Previously durable text");
            client.shutdown().unwrap();
        }

        type GuidanceFixture = (
            tempfile::TempDir,
            Store,
            HostClient,
            gpui_kit::AnyWindowHandle,
            gpui_kit::Entity<Workbench>,
            DraftKey,
            workbench_core::SteeringRequest,
            workbench_runtime::steering::SteeringOutcome,
        );
        fn guidance_fixture(cx: &mut TestAppContext) -> GuidanceFixture {
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let base = directory.path().canonicalize().unwrap();
            let workspace = base.join("project");
            std::fs::create_dir(&workspace).unwrap();
            let paths = AppPaths::at(base.join("profile")).unwrap();
            let store = Store::open(&paths.database).unwrap();
            let host = HostClient::spawn_at(paths).unwrap();
            let client = host.0.clone();
            let (window, view) = open(cx, Ok(host));
            wait(cx, &view, "settings", |view, _| view.settings_loaded);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.set_workspace(workspace.clone(), window, cx)
                })
            })
            .unwrap();
            let key = cx.update(|cx| DraftKey {
                session_id: view.read(cx).session,
                workspace: Some(workspace.clone()),
            });
            wait(cx, &view, "initial draft", |view, _| {
                view.drafts.validation_revision(&key).is_some()
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.composer.update(cx, |input, cx| {
                        input.set_value("Use the verified option αβ 🦀", window, cx)
                    });
                    view.draft_changed(cx);
                    view.draft_before_switch(cx);
                })
            })
            .unwrap();
            wait(cx, &view, "guidance save", |view, _| {
                !view.drafts.validation_dirty(&key)
            });
            let snapshot = store.draft(&key).unwrap();
            let turn = Uuid::new_v4();
            store
                .upsert_conversation(&workbench_core::Conversation {
                    id: key.session_id,
                    title: "Task".into(),
                    workspace,
                    updated_at: 1,
                })
                .unwrap();
            store
                .save_checkpoint(
                    key.session_id,
                    turn,
                    &serde_json::json!([{"role":"user","content":"Original task"}]),
                )
                .unwrap();
            let guidance = workbench_core::SteeringRequest {
                id: Uuid::new_v4(),
                session_id: key.session_id,
                turn_id: turn,
                text: snapshot.text.clone(),
            };
            let queued = store
                .queue_steering(
                    &guidance,
                    &workbench_runtime::drafts::DraftSubmission {
                        key: key.clone(),
                        expected_revision: snapshot.revision,
                        text: snapshot.text,
                        attachments: Vec::new(),
                    },
                )
                .unwrap();
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.drafts.validation_capture_guidance(&key, guidance.id);
                    view.steering_result(guidance.id, Ok(queued.clone()), window, cx);
                    assert!(view.drafts.validation_guidance_pending(&key));
                    assert_eq!(view.composer.read(cx).value().as_ref(), guidance.text);
                })
            })
            .unwrap();
            (
                directory, store, client, window, view, key, guidance, queued,
            )
        }
        #[gpui_kit::test]
        fn durable_guidance_receipt_clears_only_exact_native_draft_and_duplicate_keeps_new_input(
            cx: &mut TestAppContext,
        ) {
            let (_directory, store, client, window, view, key, guidance, _) = guidance_fixture(cx);
            let checkpoint = store.checkpoint(key.session_id).unwrap().unwrap();
            let outcome = store
                .apply_steering(key.session_id, guidance.turn_id, &checkpoint.messages)
                .unwrap()
                .applied
                .remove(0);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    assert!(!view.running());
                    view.steering_result(guidance.id, Ok(outcome.clone()), window, cx);
                    assert!(view.composer.read(cx).value().is_empty());
                    assert!(!view.running(), "receipt must not restart a finished task");
                    view.composer.update(cx, |input, cx| {
                        input.set_value("New input after applied receipt", window, cx)
                    });
                    view.steering_result(guidance.id, Ok(outcome.clone()), window, cx);
                    assert_eq!(
                        view.composer.read(cx).value().as_ref(),
                        "New input after applied receipt"
                    );
                    view.draft_changed(cx);
                    view.draft_before_switch(cx);
                })
            })
            .unwrap();
            wait(cx, &view, "new input saved", |view, _| {
                !view.drafts.validation_dirty(&key)
                    && store.draft(&key).unwrap().text == "New input after applied receipt"
            });
            assert_eq!(
                store
                    .messages(key.session_id, 20)
                    .unwrap()
                    .iter()
                    .filter(|message| message.id == Uuid::new_v5(&guidance.id, b"steering-message"))
                    .count(),
                1
            );
            client.shutdown().unwrap();
        }
        #[gpui_kit::test]
        fn applied_clear_racing_newer_native_save_rebases_only_its_known_empty_revision(
            cx: &mut TestAppContext,
        ) {
            for receipt_first in [true, false] {
                let (_directory, store, client, window, view, key, guidance, _) =
                    guidance_fixture(cx);
                let checkpoint = store.checkpoint(key.session_id).unwrap().unwrap();
                let outcome = store
                    .apply_steering(key.session_id, guidance.turn_id, &checkpoint.messages)
                    .unwrap()
                    .applied
                    .remove(0);
                cx.update_window(window, |_, window, cx| {
                    view.update(cx, |view, cx| {
                        view.composer.update(cx, |input, cx| {
                            input.set_value("Newer native typing must survive αβ 🦀", window, cx)
                        });
                        view.draft_changed(cx);
                        assert!(view.draft_before_switch(cx));
                        assert!(view.drafts.validation_save(&key).is_some());
                        if receipt_first {
                            view.steering_result(guidance.id, Ok(outcome.clone()), window, cx);
                        }
                    })
                })
                .unwrap();
                if !receipt_first {
                    wait(
                        cx,
                        &view,
                        "known clear conflict before receipt",
                        |view, _| {
                            view.drafts.validation_save(&key).is_none()
                                && view.drafts.validation_dirty(&key)
                        },
                    );
                    cx.update_window(window, |_, window, cx| {
                        view.update(cx, |view, cx| {
                            view.steering_result(guidance.id, Ok(outcome.clone()), window, cx)
                        })
                    })
                    .unwrap();
                }
                wait(cx, &view, "newer typing rebased durably", |view, _| {
                    !view.drafts.validation_dirty(&key)
                        && store.draft(&key).unwrap().text
                            == "Newer native typing must survive αβ 🦀"
                });
                cx.update(|cx| {
                    assert_eq!(
                        view.read(cx).composer.read(cx).value().as_ref(),
                        "Newer native typing must survive αβ 🦀"
                    )
                });
                client.shutdown().unwrap();
            }
        }
        #[gpui_kit::test]
        fn cancelled_guidance_and_failed_ack_retain_native_draft_for_explicit_retry(
            cx: &mut TestAppContext,
        ) {
            let (_directory, store, client, window, view, key, guidance, queued) =
                guidance_fixture(cx);
            let cancelled = store
                .finish_pending_steering(
                    key.session_id,
                    Some(guidance.turn_id),
                    "fixture cancellation",
                )
                .unwrap()
                .remove(0);
            let original_revision = queued.draft.revision;
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.steering_result(guidance.id, Ok(cancelled), window, cx);
                    assert_eq!(view.composer.read(cx).value().as_ref(), guidance.text);
                })
            })
            .unwrap();
            wait(cx, &view, "fresh explicit-retry revision", |view, _| {
                !view.drafts.validation_dirty(&key)
                    && store.draft(&key).unwrap().revision > original_revision
            });
            let retry = Uuid::new_v4();
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.drafts.validation_capture_guidance(&key, retry);
                    view.steering_result(retry, Err("fixture stopped pipe".into()), window, cx);
                    assert_eq!(view.composer.read(cx).value().as_ref(), guidance.text);
                    assert!(!view.drafts.validation_guidance_pending(&key));
                })
            })
            .unwrap();
            assert_eq!(store.draft(&key).unwrap().text, guidance.text);
            client.shutdown().unwrap();
        }

        #[gpui_kit::test]
        fn applied_receipt_cannot_treat_an_unrelated_saved_draft_as_its_own_clear(
            cx: &mut TestAppContext,
        ) {
            let (_directory, store, client, window, view, key, guidance, _) = guidance_fixture(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.composer.update(cx, |input, cx| {
                        input.set_value("My newer typing", window, cx)
                    });
                    view.draft_changed(cx);
                })
            })
            .unwrap();
            let external = save(&store, &key, "Another window's saved draft");
            let checkpoint = store.checkpoint(key.session_id).unwrap().unwrap();
            let applied = store
                .apply_steering(key.session_id, guidance.turn_id, &checkpoint.messages)
                .unwrap()
                .applied
                .remove(0);
            assert_eq!(applied.cleared_revision, None);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.steering_result(guidance.id, Ok(applied), window, cx);
                    assert_eq!(view.composer.read(cx).value().as_ref(), "My newer typing");
                });
                window.render_frame(cx);
                assert!(window.try_find("draft-keep-current").is_some());
                assert!(window.try_find("draft-use-saved").is_some());
            })
            .unwrap();
            cx.executor().advance_clock(Duration::from_millis(500));
            cx.run_until_parked();
            assert_eq!(store.draft(&key).unwrap(), external);
            client.shutdown().unwrap();
        }
    }
}
