//! Product memory buttons against the real host/SQLite, including a rejected
//! durable write. Native dialogs, model inference and external accounts stay idle.
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
mod xylo {
    include!("../src/xylo.rs");
    mod memory_tests {
        use super::{AppSettings, HostClient, MemoryRecord, Page, Uuid, Xylo};
        use gpui_kit::test::{TestQueryExt, TestWindowExt};
        use gpui_kit::{
            App, AppContext, Bounds, Entity, TestAppContext, WindowBounds, WindowOptions, px, size,
        };
        use std::time::{Duration, Instant};
        fn wait(
            cx: &mut TestAppContext,
            view: &Entity<Xylo>,
            condition: impl Fn(&Xylo, &App) -> bool,
        ) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                // Drive the native receiver's bounded-backlog yield timer.
                cx.executor().advance_clock(std::time::Duration::from_millis(2));
                cx.run_until_parked();
                if cx.update(|cx| condition(view.read(cx), cx)) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "Timed out waiting for native memory state"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        #[gpui_kit::test]
        fn native_memory_browses_corrects_and_preserves_input_after_rejected_save(
            cx: &mut TestAppContext,
        ) {
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let base = directory.path().canonicalize().unwrap();
            let project = base.join("project");
            std::fs::create_dir(&project).unwrap();
            let paths = xylo_runtime::AppPaths::at(base.join("profile")).unwrap();
            let store = xylo_services::Store::open(&paths.database).unwrap();
            let settings = AppSettings {
                last_workspace: Some(project.clone()),
                ..Default::default()
            };
            store.save_settings(&settings).unwrap();
            let memory = xylo_services::SqliteMemory::new(store.clone());
            memory
                .remember_sync(MemoryRecord {
                    id: Uuid::new_v4(),
                    scope: project.to_string_lossy().into(),
                    content: "Original project choice".into(),
                    source: "Explicit native fixture entry".into(),
                    created_at: 7,
                    supersedes: None,
                })
                .unwrap();
            let host = HostClient::spawn_at(paths.clone()).unwrap();
            let client = host.0.clone();
            cx.update(gpui_kit::init);
            let (window, view) = cx.update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Default::default(),
                            size: size(px(1280.), px(1000.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| Xylo::new(Ok(host), window, cx)),
                )
                .unwrap()
            });
            wait(cx, &view, |view, _| {
                view.settings_loaded
                    && view.workspace_ready
                    && view.workspace.as_ref() == Some(&project)
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| view.navigate(Page::Memory, cx));
                window.render_frame(cx);
            })
            .unwrap();
            wait(cx, &view, |view, _| view.memories.len() == 1);
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.click(("memory-select", 0usize), cx);
            })
            .unwrap();
            wait(cx, &view, |view, _| {
                view.validation_fact_ready() && view.memory_form_ready()
            });
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.click("memory-edit", cx);
                view.update(cx, |view, cx| {
                    view.memory_content.update(cx, |input, cx| {
                        input.set_value("Corrected project choice", window, cx)
                    });
                });
                window.render_frame(cx);
                window.click("memory-add", cx);
            })
            .unwrap();
            wait(cx, &view, |view, cx| {
                view.memories.len() == 1
                    && view.memories[0].content == "Corrected project choice"
                    && view.memory_content.read(cx).value().is_empty()
            });
            let corrected = store
                .memory_page(&project.to_string_lossy(), None, 50)
                .unwrap()
                .records;
            assert_eq!(corrected.len(), 1);
            assert!(corrected[0].supersedes.is_some());
            // Hold the actual writer lock while leaving the source project.
            // Its later success must clear only the matching cached source form.
            let other = base.join("other-project");
            std::fs::create_dir(&other).unwrap();
            let write_lock = rusqlite::Connection::open(&paths.database).unwrap();
            write_lock.execute_batch("BEGIN IMMEDIATE").unwrap();
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.memory_content.update(cx, |input, cx| {
                        input.set_value("Saved while leaving project", window, cx)
                    })
                });
                window.render_frame(cx);
                window.click("memory-add", cx);
                view.update(cx, |view, cx| {
                    assert!(view.facts.busy());
                    view.set_workspace(other.clone(), window, cx);
                    assert_eq!(view.workspace.as_ref(), Some(&other));
                });
            })
            .unwrap();
            write_lock.execute_batch("COMMIT").unwrap();
            wait(cx, &view, |view, _| !view.facts.busy());
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.set_workspace(project.clone(), window, cx);
                    view.navigate(Page::Memory, cx);
                    assert_eq!(
                        view.memory_content.read(cx).value().as_ref(),
                        "",
                        "An acknowledged entry must not reappear; status={}",
                        view.facts.status_message()
                    );
                })
            })
            .unwrap();
            wait(cx, &view, |view, _| {
                view.workspace_ready && view.memories.len() == 2
            });
            rusqlite::Connection::open(&paths.database).unwrap().execute_batch("CREATE TRIGGER reject_native_memory BEFORE INSERT ON memories BEGIN SELECT RAISE(ABORT,'blocked memory write'); END").unwrap();
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.memory_content.update(cx, |input, cx| {
                        input.set_value("Keep this unsaved entry", window, cx)
                    });
                });
                window.render_frame(cx);
                window.click("memory-add", cx);
            })
            .unwrap();
            wait(cx, &view, |view, _| {
                !view.facts.busy()
                    && view
                        .facts
                        .status_message()
                        .contains("Memory could not be updated")
            });
            cx.update(|cx| {
                assert_eq!(
                    view.read(cx).memory_content.read(cx).value().as_ref(),
                    "Keep this unsaved entry"
                )
            });
            assert_eq!(
                store
                    .memory_page(&project.to_string_lossy(), None, 50)
                    .unwrap()
                    .records
                    .len(),
                2
            );
            client.shutdown().unwrap();
        }

        fn launch(
            cx: &mut TestAppContext,
            paths: xylo_runtime::AppPaths,
        ) -> (gpui_kit::AnyWindowHandle, Entity<Xylo>, HostClient) {
            let host = HostClient::spawn_at(paths).unwrap();
            let client = host.0.clone();
            cx.update(gpui_kit::init);
            let (window, view) = cx.update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Default::default(),
                            size: size(px(1280.), px(1200.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| Xylo::new(Ok(host), window, cx)),
                )
                .unwrap()
            });
            wait(cx, &view, |view, _| {
                view.settings_loaded && view.memory_form_ready()
            });
            cx.update_window(window, |_, _, cx| {
                view.update(cx, |view, cx| view.navigate(Page::Memory, cx))
            })
            .unwrap();
            (window, view, client)
        }
        #[gpui_kit::test]
        fn native_restores_global_memory_form_and_autosaves_new_typing_without_promoting_a_fact(
            cx: &mut TestAppContext,
        ) {
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let paths = xylo_runtime::AppPaths::at(directory.path().join("profile")).unwrap();
            let store = xylo_services::Store::open(&paths.database).unwrap();
            store
                .save_memory_form(xylo_runtime::MemoryFormWrite {
                    scope: "global".into(),
                    expected_revision: 0,
                    mutation_id: Uuid::new_v4(),
                    form: xylo_runtime::MemoryFormState {
                        text: "Recovered unsaved Memory text αβ 🦀".into(),
                        editing: None,
                        retry: None,
                        retry_input: None,
                    },
                })
                .unwrap();
            let (window, view, client) = launch(cx, paths.clone());
            cx.update(|cx| {
                assert_eq!(
                    view.read(cx).memory_content.read(cx).value().as_ref(),
                    "Recovered unsaved Memory text αβ 🦀"
                )
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.memory_content
                        .update(cx, |input, cx| input.focus(window, cx))
                });
                window.render_frame(cx);
                window.press("cmd-a", cx);
                window.input("Still unsaved, now edited", cx);
            })
            .unwrap();
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(400));
            wait(cx, &view, |view, _| {
                view.validation_memory_form_clean("global")
            });
            let reopened = xylo_services::Store::open(&paths.database).unwrap();
            assert_eq!(
                reopened.memory_form("global").unwrap().form.text,
                "Still unsaved, now edited"
            );
            assert!(
                reopened
                    .memory_page("global", None, 100)
                    .unwrap()
                    .records
                    .is_empty()
            );
            client.shutdown().unwrap();
        }
        #[gpui_kit::test]
        fn native_new_typing_after_pending_fact_save_survives_scope_switch_and_late_ack(
            cx: &mut TestAppContext,
        ) {
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let base = directory.path().canonicalize().unwrap();
            let project = base.join("a");
            let other = base.join("b");
            std::fs::create_dir(&project).unwrap();
            std::fs::create_dir(&other).unwrap();
            let paths = xylo_runtime::AppPaths::at(base.join("profile")).unwrap();
            let store = xylo_services::Store::open(&paths.database).unwrap();
            store
                .save_settings(&AppSettings {
                    last_workspace: Some(project.clone()),
                    ..Default::default()
                })
                .unwrap();
            let scope = project.to_string_lossy().to_string();
            let (window, view, client) = launch(cx, paths.clone());
            wait(cx, &view, |view, _| view.workspace_ready);
            let lock = rusqlite::Connection::open(&paths.database).unwrap();
            lock.execute_batch("BEGIN IMMEDIATE").unwrap();
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.memory_content.update(cx, |input, cx| {
                        input.set_value("Explicit fact submitted before leaving", window, cx)
                    })
                });
                window.render_frame(cx);
                window.click("memory-add", cx);
                view.update(cx, |view, cx| {
                    assert!(view.facts.busy());
                    view.memory_content.update(cx, |input, cx| {
                        input.set_value("New typing must survive the older save", window, cx)
                    });
                    view.set_workspace(other.clone(), window, cx);
                    view.memory_content.update(cx, |input, cx| {
                        input.set_value("Other project text stays here", window, cx)
                    });
                });
            })
            .unwrap();
            lock.execute_batch("COMMIT").unwrap();
            wait(cx, &view, |view, _| {
                !view.facts.busy() && view.validation_memory_form_clean(&scope)
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.set_workspace(project.clone(), window, cx)
                })
            })
            .unwrap();
            wait(cx, &view, |view, _| {
                view.memory_form_ready() && view.workspace_ready
            });
            cx.update(|cx| {
                assert_eq!(
                    view.read(cx).memory_content.read(cx).value().as_ref(),
                    "New typing must survive the older save"
                )
            });
            assert_eq!(
                store.memory_form(&scope).unwrap().form.text,
                "New typing must survive the older save"
            );
            let facts = store.memory_page(&scope, None, 50).unwrap().records;
            assert_eq!(facts.len(), 1);
            assert_eq!(facts[0].content, "Explicit fact submitted before leaving");
            client.shutdown().unwrap();
        }
        #[gpui_kit::test]
        fn native_chain_deletion_requires_confirmation_and_fences_changes_while_prompt_open(
            cx: &mut TestAppContext,
        ) {
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let base = directory.path().canonicalize().unwrap();
            let project = base.join("a");
            std::fs::create_dir(&project).unwrap();
            let scope = project.to_string_lossy().to_string();
            let paths = xylo_runtime::AppPaths::at(base.join("profile")).unwrap();
            let store = xylo_services::Store::open(&paths.database).unwrap();
            store
                .save_settings(&AppSettings {
                    last_workspace: Some(project.clone()),
                    ..Default::default()
                })
                .unwrap();
            let memory = xylo_services::SqliteMemory::new(store.clone());
            let original = MemoryRecord {
                id: Uuid::new_v4(),
                scope: scope.clone(),
                content: "Original fact".into(),
                source: "Explicit fixture".into(),
                created_at: 1,
                supersedes: None,
            };
            memory.remember_sync(original.clone()).unwrap();
            let corrected = MemoryRecord {
                id: Uuid::new_v4(),
                content: "Corrected fact".into(),
                created_at: 2,
                supersedes: Some(original.id),
                ..original.clone()
            };
            memory.remember_sync(corrected.clone()).unwrap();
            let (window, view, client) = launch(cx, paths.clone());
            wait(cx, &view, |view, _| {
                view.workspace_ready && view.memories.len() == 1 && view.memory_form_ready()
            });
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.click("memory-new", cx);
                view.update(cx, |view, cx| {
                    view.memory_content
                        .update(cx, |input, cx| input.focus(window, cx));
                });
                window.render_frame(cx);
                window.input("Unsaved entry must not be purged", cx);
                window.click(("memory-select", 0usize), cx);
            })
            .unwrap();
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(400));
            wait(cx, &view, |view, _| {
                view.validation_fact_ready() && view.validation_memory_form_clean(&scope)
            });
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.click("memory-forget-chain", cx);
            })
            .unwrap();
            wait(cx, &view, |view, _| view.validation_fact_prompt());
            assert!(cx.has_pending_prompt());
            cx.simulate_prompt_answer("Cancel");
            wait(cx, &view, |view, _| !view.facts.busy());
            assert_eq!(
                store
                    .fact_versions(
                        &scope,
                        Some(corrected.id),
                        None,
                        50,
                        &tokio_util::sync::CancellationToken::new()
                    )
                    .unwrap()
                    .versions
                    .len(),
                2
            );
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.click("memory-forget-chain", cx);
            })
            .unwrap();
            wait(cx, &view, |view, _| view.validation_fact_prompt());
            memory
                .remember_sync(MemoryRecord {
                    id: Uuid::new_v4(),
                    scope: scope.clone(),
                    content: "A different fact created during confirmation".into(),
                    source: "Explicit fixture".into(),
                    created_at: 3,
                    supersedes: None,
                })
                .unwrap();
            cx.simulate_prompt_answer("Delete saved versions");
            wait(cx, &view, |view, _| !view.facts.busy());
            assert_eq!(
                store
                    .fact_versions(
                        &scope,
                        None,
                        None,
                        50,
                        &tokio_util::sync::CancellationToken::new()
                    )
                    .unwrap()
                    .versions
                    .len(),
                3
            );
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.click("memory-forget-chain", cx);
            })
            .unwrap();
            wait(cx, &view, |view, _| view.validation_fact_prompt());
            cx.simulate_prompt_answer("Delete saved versions");
            wait(cx, &view, |view, _| {
                !view.facts.busy() && view.memories.len() == 1
            });
            assert_eq!(
                store
                    .fact_versions(
                        &scope,
                        None,
                        None,
                        50,
                        &tokio_util::sync::CancellationToken::new()
                    )
                    .unwrap()
                    .versions
                    .len(),
                1
            );
            cx.update(|cx| {
                assert_eq!(
                    view.read(cx).memory_content.read(cx).value().as_ref(),
                    "Unsaved entry must not be purged"
                )
            });
            assert_eq!(
                store.memory_form(&scope).unwrap().form.text,
                "Unsaved entry must not be purged"
            );
            client.shutdown().unwrap();
        }
        #[gpui_kit::test]
        fn typing_during_close_flush_cancels_close_intent_and_keeps_latest_entry(
            cx: &mut TestAppContext,
        ) {
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let paths = xylo_runtime::AppPaths::at(directory.path().join("profile")).unwrap();
            let (window, view, client) = launch(cx, paths.clone());
            let store = xylo_services::Store::open(&paths.database).unwrap();
            let lock = rusqlite::Connection::open(&paths.database).unwrap();
            lock.execute_batch("BEGIN IMMEDIATE").unwrap();
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.memory_content
                        .update(cx, |input, cx| input.set_value("Before quit", window, cx));
                    assert!(!view.memory_forms_flush(super::DraftFlushAction::Quit, cx));
                    assert!(view.memory_forms_flush_pending());
                    view.memory_content.update(cx, |input, cx| {
                        input.set_value("Typed after quit was requested", window, cx)
                    });
                    view.memory_form_changed(cx);
                    assert!(!view.memory_forms_flush_pending());
                })
            })
            .unwrap();
            lock.execute_batch("COMMIT").unwrap();
            wait(cx, &view, |view, _| {
                view.validation_memory_form_clean("global")
            });
            assert_eq!(
                store.memory_form("global").unwrap().form.text,
                "Typed after quit was requested"
            );
            cx.update(|cx| assert!(!view.read(cx).quitting));
            client.shutdown().unwrap();
        }
        #[gpui_kit::test]
        fn approved_memory_starts_with_saved_rows_and_opens_entry_on_request(
            cx: &mut TestAppContext,
        ) {
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let paths = xylo_runtime::AppPaths::at(directory.path().join("profile")).unwrap();
            let (window, view, client) = launch(cx, paths);
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                assert!(window.find("memory-new").visible());
                assert!(window.try_find_by_label("Unsaved Memory entry").is_none());
                assert!(window.try_find("refresh-learning").is_none());
                window.click("memory-new", cx);
                window.render_frame(cx);
                assert!(window.find_by_label("Unsaved Memory entry").visible());
            })
            .unwrap();
            cx.update_window(window, |_, window, cx| {
                window.click("memory-cancel-new", cx);
                window.render_frame(cx);
                assert!(window.try_find_by_label("Unsaved Memory entry").is_none());
                window.click("memory-new", cx);
                window.input("Keep the workspace focused.", cx);
                window.click("memory-add", cx);
            })
            .unwrap();
            wait(cx, &view, |view, cx| {
                view.memories
                    .iter()
                    .any(|record| record.content == "Keep the workspace focused.")
                    && view.memory_content.read(cx).value().is_empty()
            });
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                assert!(window.find("memory-new").visible());
                assert!(window.try_find_by_label("Unsaved Memory entry").is_none());
            })
            .unwrap();
            client.shutdown().unwrap();
        }
    }
}
