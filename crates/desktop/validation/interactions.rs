//! Production native controls exercised with GPUI's interaction harness.
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
    mod native_tests {
        use super::{Page, Workbench};
        use gpui_kit::test::TestWindowExt;
        use gpui_kit::{AppContext, Bounds, TestAppContext, WindowBounds, WindowOptions, px, size};
        use workbench_core::ApiFamily;

        #[gpui_kit::test]
        fn native_navigation_and_provider_setup_are_real_controls(cx: &mut TestAppContext) {
            cx.update(gpui_kit::init);
            let (handle, view) = cx.update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Default::default(),
                            size: size(px(1280.), px(820.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| {
                        cx.new(|cx| {
                            Workbench::new(
                                Err(anyhow::anyhow!("Test has no external host")),
                                window,
                                cx,
                            )
                        })
                    },
                )
                .unwrap()
            });
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.click("settings", cx);
                assert_eq!(view.read(cx).page, Page::Providers);
                window.click("preset-anthropic", cx);
                assert_eq!(view.read(cx).settings.provider.family, ApiFamily::Anthropic);
                assert_eq!(
                    view.read(cx).endpoint.read(cx).value().as_ref(),
                    "https://api.anthropic.com"
                );
                window.click("preset-local", cx);
                assert_eq!(
                    view.read(cx).settings.provider.family,
                    ApiFamily::OpenAiChat
                );
                assert!(
                    view.read(cx)
                        .endpoint
                        .read(cx)
                        .value()
                        .starts_with("http://127.0.0.1:")
                );
                window.click("nav-7", cx);
                assert_eq!(view.read(cx).page, Page::Diagnostics);
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn new_conversation_does_not_cancel_a_background_turn(cx: &mut TestAppContext) {
            cx.update(gpui_kit::init);
            let (handle, view) = cx.update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Default::default(),
                            size: size(px(1280.), px(820.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| {
                        cx.new(|cx| {
                            Workbench::new(
                                Err(anyhow::anyhow!("Test has no external host")),
                                window,
                                cx,
                            )
                        })
                    },
                )
                .unwrap()
            });
            cx.update_window(handle, |_, window, cx| {
                let old = view.read(cx).session;
                view.update(cx, |view, cx| {
                    view.turns
                        .entry(old)
                        .or_default()
                        .apply(workbench_core::EngineEvent::Started);
                    cx.notify();
                });
                window.render_frame(cx);
                window.click("new-chat", cx);
                assert_ne!(view.read(cx).session, old);
                assert!(view.read(cx).turns.get(&old).unwrap().running);
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn cancelling_quit_retains_unsaved_editor_text(cx: &mut TestAppContext) {
            cx.update(gpui_kit::init);
            let (handle, view) = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| {
                        Workbench::new(Err(anyhow::anyhow!("No external host")), window, cx)
                    })
                })
                .unwrap()
            });
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.editor_dirty = true;
                    view.editor_path = Some(std::path::PathBuf::from("draft.txt"));
                    view.request_quit(window, cx);
                })
            })
            .unwrap();
            assert!(cx.has_pending_prompt());
            cx.simulate_prompt_answer("Cancel");
            cx.run_until_parked();
            cx.update(|cx| {
                assert!(view.read(cx).editor_dirty);
                assert!(!view.read(cx).quitting);
                assert!(!view.read(cx).quit_prompt_open);
            });
        }
        #[gpui_kit::test]
        fn save_ack_does_not_quit_with_newer_edits_and_save_failure_keeps_text(
            cx: &mut TestAppContext,
        ) {
            cx.update(gpui_kit::init);
            let (handle, view) = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| {
                        Workbench::new(Err(anyhow::anyhow!("No external host")), window, cx)
                    })
                })
                .unwrap()
            });
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    let path = std::path::PathBuf::from("draft.txt");
                    view.editor_path = Some(path.clone());
                    view.editor
                        .update(cx, |editor, cx| editor.set_value("newer edits", window, cx));
                    view.editor_dirty = true;
                    view.pending_saves
                        .insert(path.clone(), "older contents".into());
                    view.quit_after_save = Some(path.clone());
                    view.event(
                        workbench_runtime::host::HostEvent::Saved(path.clone()),
                        window,
                        cx,
                    );
                    assert!(view.editor_dirty);
                    assert!(!view.quitting);
                    assert!(view.quit_after_save.is_none());
                    view.pending_saves
                        .insert(path.clone(), "newer edits".into());
                    view.quit_after_save = Some(path.clone());
                    view.event(
                        workbench_runtime::host::HostEvent::SaveFailed {
                            path,
                            message: "Unable to save this file".into(),
                        },
                        window,
                        cx,
                    );
                    assert!(view.editor_dirty);
                    assert!(!view.quitting);
                    assert!(view.pending_saves.is_empty());
                    assert!(view.quit_after_save.is_none());
                    assert_eq!(view.editor.read(cx).value().as_ref(), "newer edits");
                })
            })
            .unwrap();
        }

        fn open_test_view(
            cx: &mut TestAppContext,
        ) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<Workbench>) {
            cx.update(gpui_kit::init);
            let (window, view) = cx.update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Default::default(),
                            size: size(px(1280.), px(820.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| {
                        cx.new(|cx| {
                            Workbench::new(Err(anyhow::anyhow!("No external host")), window, cx)
                        })
                    },
                )
                .unwrap()
            });
            (window, view)
        }
        fn wait_for_history(
            cx: &mut TestAppContext,
            view: &gpui_kit::Entity<Workbench>,
            description: &str,
            condition: impl Fn(&Workbench, &gpui_kit::App) -> bool,
        ) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                cx.run_until_parked();
                if cx.update(|cx| condition(view.read(cx), cx)) {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "Timed out waiting for {description}"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        #[gpui_kit::test]
        fn database_search_opens_exact_old_message_and_navigates_bounded_windows(
            cx: &mut TestAppContext,
        ) {
            use uuid::Uuid;
            use workbench_core::{ChatMessage, Conversation, HistoryPage};
            use workbench_runtime::{
                AppPaths,
                history::{HistorySearchPage, HistoryWindow},
                host::HostClient,
            };
            use workbench_services::Store;
            cx.executor().allow_parking();
            let temporary = tempfile::tempdir().unwrap();
            let base = temporary.path().canonicalize().unwrap();
            let workspace = base.join("project");
            std::fs::create_dir(&workspace).unwrap();
            let paths = AppPaths::at(base.join("profile")).unwrap();
            let store = Store::open(&paths.database).unwrap();
            let session = Uuid::new_v4();
            store
                .upsert_conversation(&Conversation {
                    id: session,
                    title: "Archived conversation".into(),
                    workspace: workspace.clone(),
                    updated_at: 7,
                })
                .unwrap();
            let saved: Vec<_> = (0..800)
                .map(|index| ChatMessage {
                    id: Uuid::new_v4(),
                    session_id: session,
                    role: "assistant".into(),
                    content: if index == 350 {
                        "αβ 🦀 historicalneedle exact source message".into()
                    } else {
                        format!("Saved message {index}")
                    },
                    created_at: 7,
                    attachments: Vec::new(),
                })
                .collect();
            for message in &saved {
                store.append_message(message).unwrap();
            }
            let anchor = saved[350].id;
            let host = HostClient::spawn_at(paths).unwrap();
            let client = host.0.clone();
            cx.update(gpui_kit::init);
            let (window, view) = cx.update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Default::default(),
                            size: size(px(1280.), px(900.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| Workbench::new(Ok(host), window, cx)),
                )
                .unwrap()
            });
            wait_for_history(cx, &view, "initial settings", |view, _| {
                view.settings_loaded
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.set_workspace(workspace.clone(), window, cx);
                    view.conversation_query
                        .update(cx, |input, cx| input.set_value("oldquery", window, cx));
                    view.history_search_changed(cx);
                    view.history_search_submit(cx);
                    let stale = view.history_search.validation_request().unwrap();
                    view.conversation_query.update(cx, |input, cx| {
                        input.set_value("historicalneedle", window, cx)
                    });
                    view.history_search_changed(cx);
                    view.history_search_result(
                        stale,
                        Ok(HistorySearchPage {
                            query: "oldquery".into(),
                            items: vec![],
                            before: None,
                            has_more: false,
                            indexing: false,
                        }),
                        cx,
                    );
                    assert!(view.history_search.validation_items().is_empty());
                });
                window.render_frame(cx);
            })
            .unwrap();
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.click("history-search-refresh", cx);
            })
            .unwrap();
            wait_for_history(cx, &view, "database-wide source result", |view, _| {
                view.history_search.validation_request().is_none()
                    && view.history_search.validation_items().len() == 1
            });
            cx.update_window(window, |_, window, cx| {
                let hit = &view.read(cx).history_search.validation_items()[0];
                assert_eq!(hit.message_id, Some(anchor));
                assert!(hit.snippet.contains("historicalneedle"));
                assert!(
                    view.read(cx)
                        .messages
                        .iter()
                        .all(|message| message.id != anchor)
                );
                window.render_frame(cx);
                window.click(("history-match", 0usize), cx);
                view.update(cx, |view, cx| {
                    let mut wrong = store.history_window(session, saved[10].id, 10).unwrap();
                    wrong.anchor = Some(saved[10].id);
                    view.history_window_result(wrong, cx);
                    assert!(
                        view.history_loading(),
                        "wrong source response must not clear the pending jump"
                    );
                });
            })
            .unwrap();
            wait_for_history(cx, &view, "centered source window", |view, _| {
                view.session == session
                    && !view.history_loading()
                    && view.messages.iter().any(|message| message.id == anchor)
            });
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                let source_index = view
                    .read(cx)
                    .messages
                    .iter()
                    .position(|message| message.id == anchor)
                    .unwrap();
                assert_eq!(view.read(cx).messages.len(), 200);
                assert!(view.read(cx).history_message_highlight(anchor));
                assert!(
                    window
                        .try_find(("copy-message", source_index + 1))
                        .is_some(),
                    "exact source message must be in the native viewport"
                );
                assert!(!view.read(cx).histories[&session].list.is_following_tail());
                window.click("later-messages", cx);
                view.update(cx, |view, cx| {
                    let (_, after, _) = view.validation_history_window();
                    let mut stale = store.history_after(session, after.unwrap(), 10).unwrap();
                    stale.request_after = Some(after.unwrap() + 10);
                    view.history_window_result(stale, cx);
                    assert_eq!(view.messages.len(), 200);
                    assert!(
                        view.history_loading(),
                        "wrong later-page cursor must remain pending"
                    );
                });
            })
            .unwrap();
            wait_for_history(cx, &view, "later messages", |view, _| {
                !view.history_loading() && view.messages.len() == 300
            });
            cx.update_window(window, |_, _window, cx| {
                view.update(cx, |view, cx| {
                    assert!(
                        view.messages.iter().any(|message| message.id == anchor),
                        "later paging retains the source window"
                    );
                    let retained: Vec<_> = view.messages.iter().map(|message| message.id).collect();
                    view.history_page(
                        HistoryPage {
                            session_id: session,
                            messages: saved[700..].to_vec(),
                            before: Some(701),
                            has_more: true,
                        },
                        cx,
                    );
                    assert_eq!(
                        view.messages
                            .iter()
                            .map(|message| message.id)
                            .collect::<Vec<_>>(),
                        retained,
                        "unsolicited latest refresh cannot overwrite an active source window"
                    );
                    view.load_older(cx);
                    view.load_older(cx);
                });
            })
            .unwrap();
            wait_for_history(cx, &view, "earlier bounded window", |view, _| {
                !view.history_loading()
                    && view
                        .messages
                        .first()
                        .is_some_and(|message| message.id == saved[150].id)
            });
            cx.update_window(window, |_, window, cx| {
                assert!(view.read(cx).messages.len() <= 400);
                assert!(
                    view.read(cx)
                        .messages
                        .iter()
                        .any(|message| message.id == saved[250].id)
                );
                window.render_frame(cx);
                window.click("latest-messages", cx);
            })
            .unwrap();
            wait_for_history(cx, &view, "latest history", |view, _| {
                !view.history_loading()
                    && view
                        .messages
                        .last()
                        .is_some_and(|message| message.id == saved[799].id)
                    && !view.validation_history_window().0
            });
            client.shutdown().unwrap();
        }

        #[gpui_kit::test]
        fn paged_history_preserves_visible_message_and_pixel_anchor(cx: &mut TestAppContext) {
            let (window, view) = open_test_view(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    let session = view.session;
                    let latest: Vec<_> = (0..100)
                        .map(|i| {
                            crate::state::message(
                                session,
                                "assistant",
                                format!("Latest message {i}"),
                            )
                        })
                        .collect();
                    let overlap = latest[0].clone();
                    view.history_page(
                        workbench_core::HistoryPage {
                            session_id: session,
                            messages: latest,
                            before: Some(100),
                            has_more: true,
                        },
                        cx,
                    );
                    view.histories[&session]
                        .list
                        .scroll_to(gpui_kit::ListOffset {
                            item_ix: 40,
                            offset_in_item: px(12.),
                        });
                    view.load_older(cx);
                    let mut older: Vec<_> = (0..80)
                        .map(|i| {
                            crate::state::message(session, "user", format!("Earlier message {i}"))
                        })
                        .collect();
                    older.push(overlap);
                    view.history_page(
                        workbench_core::HistoryPage {
                            session_id: session,
                            messages: older,
                            before: Some(20),
                            has_more: true,
                        },
                        cx,
                    );
                    assert_eq!(view.messages.len(), 180);
                    assert_eq!(view.messages[80].content, "Latest message 0");
                    let anchor = view.histories[&session].list.logical_scroll_top();
                    assert_eq!(anchor.item_ix, 120);
                    assert_eq!(anchor.offset_in_item, px(12.));
                });
                window.render_frame(cx);
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn native_history_virtualizes_full_conversation_and_can_reveal_earliest_message(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open_test_view(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.error = None;
                    let session = view.session;
                    let messages = (0..2000)
                        .map(|i| {
                            crate::state::message(
                                session,
                                "assistant",
                                format!("Message {i} with readable content."),
                            )
                        })
                        .collect();
                    view.history_page(
                        workbench_core::HistoryPage {
                            session_id: session,
                            messages,
                            before: None,
                            has_more: false,
                        },
                        cx,
                    );
                });
                window.render_frame(cx);
                let state = view.read(cx).histories[&view.read(cx).session].list.clone();
                assert_eq!(state.item_count(), 2001);
                let rendered = (1usize..=2000)
                    .filter(|i| window.try_find(("copy-message", *i)).is_some())
                    .count();
                assert!(
                    rendered > 0 && rendered < 40,
                    "rendered {rendered} of 2000 message controls"
                );
                assert!(window.try_find(("copy-message", 1usize)).is_none());
                assert!(window.try_find(("copy-message", 2000usize)).is_some());
                state.pause_following_tail();
                state.scroll_to(gpui_kit::ListOffset {
                    item_ix: 1,
                    offset_in_item: px(0.),
                });
                view.update(cx, |_, cx| cx.notify());
                window.render_frame(cx);
                assert!(
                    window.try_find(("copy-message", 1usize)).is_some(),
                    "earliest message remains reachable"
                );
                assert!(!state.is_following_tail());
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn persisted_theme_and_layout_restore_to_native_controls(cx: &mut TestAppContext) {
            let (window, view) = open_test_view(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    let mut settings = workbench_core::AppSettings {
                        theme: "dark".into(),
                        ..Default::default()
                    };
                    settings.ui.sidebar_width = 312.;
                    settings.ui.active_page = "appearance".into();
                    view.event(
                        workbench_runtime::host::HostEvent::Settings(settings),
                        window,
                        cx,
                    );
                    assert_eq!(view.page, Page::Appearance);
                    assert_eq!(view.settings.ui.sidebar_width, 312.);
                    assert_eq!(crate::ui::bg(), 0x17191f);
                });
                window.render_frame(cx);
                window.click("theme-light", cx);
                assert_eq!(view.read(cx).settings.theme, "light");
                assert_eq!(crate::ui::bg(), 0xf9fafb);
                window.render_frame(cx);
                window.click("layout-0-larger", cx);
                assert_eq!(view.read(cx).settings.ui.sidebar_width, 336.);
                window.render_frame(cx);
                let edge = window.find("sidebar-resize").bounds().center();
                window.drag(edge, edge + gpui_kit::point(px(24.), px(0.)), cx);
                assert!(
                    view.read(cx).settings.ui.sidebar_width > 350.,
                    "native split handle must resize the sidebar"
                );
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn composer_uses_native_ime_preedit_and_unicode_commit_without_submitting(
            cx: &mut TestAppContext,
        ) {
            use gpui_kit::EntityInputHandler;
            let (window, view) = open_test_view(cx);
            cx.update_window(window, |_, window, cx| {
                let composer = view.read(cx).composer.clone();
                composer.update(cx, |input, cx| {
                    input.focus(window, cx);
                    input.replace_and_mark_text_in_range(None, "にほん", Some(3..3), window, cx);
                    assert!(input.marked_text_range(window, cx).is_some());
                    input.replace_text_in_range(None, "日本語🙂", window, cx);
                    assert_eq!(input.value().as_ref(), "日本語🙂");
                    assert!(input.marked_text_range(window, cx).is_none());
                });
                assert!(view.read(cx).turns.is_empty());
            })
            .unwrap();
        }

        #[gpui_kit::test]
        fn catalog_handles_real_nullable_metadata_and_hides_runtime_dependencies(
            cx: &mut TestAppContext,
        ) {
            let (handle, view) = open_test_view(cx);
            cx.update_window(handle,|_,window,cx| {
                view.update(cx,|view,cx| {
                    view.page=Page::Assets;view.settings_open=true;view.error=None;
                    view.event(workbench_runtime::host::HostEvent::Capability{
                        request_id:uuid::Uuid::new_v4(),name:"helper_asset_catalog".into(),
                        result:Ok(serde_json::json!({"assets":[
                            {"id":"test-model","name":"Selected test model","kind":"model","description":"Internal engine details","size_bytes":12345678,"license":{"identifier":"Apache-2.0","url":"https://example.com/license"},"supported":true,"unavailable_reason":null,"installed":false,"active":false,"requires":["runtime"]},
                            {"id":"runtime","name":"Internal runtime","kind":"runtime","supported":true,"hidden":true,"unavailable_reason":null}
                        ]}))
                    },window,cx);
                    assert!(view.native.pending_id("Starting download").is_none(),"catalog reads must not start an install");
                });
                window.render_frame(cx);
                assert!(window.try_find(("install-asset",0usize)).is_some());
                assert!(window.try_find(("install-asset",1usize)).is_none());
                assert!(view.read(cx).error.is_none());
            }).unwrap();
        }
        #[gpui_kit::test]
        #[ignore = "requires a native clangd; run explicitly for OS qualification"]
        fn native_editor_completion_uses_real_host_and_clangd(cx: &mut TestAppContext) {
            cx.executor().allow_parking();
            let temporary = tempfile::tempdir().unwrap();
            let workspace = std::fs::canonicalize(temporary.path()).unwrap();
            let path = workspace.join("sample.cpp");
            let source = "struct Widget { int count; };\nint main() { Widget w; w. }\n";
            std::fs::write(&path, source).unwrap();
            let host = workbench_runtime::host::HostClient::spawn_at(
                workbench_runtime::AppPaths::at(workspace.join("profile")).unwrap(),
            )
            .unwrap();
            let client = host.0.clone();
            cx.update(gpui_kit::init);
            let (handle, view) = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| Workbench::new(Ok(host), window, cx))
                })
                .unwrap()
            });
            wait_for_history(
                cx,
                &view,
                "initial settings before opening clangd workspace",
                |view, _| view.settings_loaded,
            );
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.set_workspace(workspace.clone(), window, cx)
                })
            })
            .unwrap();
            cx.update(|cx| {
                view.update(cx, |view, cx| {
                    view.request_editor_file(path.clone(), None, false, cx)
                })
            });
            wait_for_history(cx, &view, "real clangd file open", |view, _| {
                view.editor_path.as_ref() == Some(&path)
            });
            cx.update_window(handle, |_, window, cx| {
                let editor = view.read(cx).editor.clone();
                editor.update(cx, |editor, cx| {
                    assert!(editor.lsp().completion_provider.is_some());
                    editor.set_cursor_position(lsp_types::Position::new(1, 25), window, cx);
                    editor.show_completions(window, cx);
                });
                window.render_frame(cx);
            })
            .unwrap();
            let mut completed = false;
            for _ in 0..300 {
                cx.run_until_parked();
                completed = cx.update(|cx| {
                    view.read(cx)
                        .editor
                        .read(cx)
                        .completion_menu_state()
                        .items
                        .iter()
                        .any(|item| item.label.contains("count"))
                });
                if completed {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            // Completion responses and publishDiagnostics are independent LSP
            // messages. Receiving a completion does not acknowledge AST diagnostics.
            wait_for_history(
                cx,
                &view,
                "real clangd diagnostic decorations",
                |view, cx| {
                    view.editor
                        .read(cx)
                        .diagnostics()
                        .is_some_and(|diagnostics| !diagnostics.is_empty())
                },
            );
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                let editor = view.read(cx).editor.read(cx);
                assert!(
                    completed,
                    "real clangd completion did not reach the native menu"
                );
                assert!(editor.completion_menu_state().open);
                assert!(
                    editor
                        .diagnostics()
                        .is_some_and(|diagnostics| !diagnostics.is_empty()),
                    "real clangd diagnostics must decorate the editor"
                );
            })
            .unwrap();
            cx.update_window(handle, |_, window, cx| {
                let editor = view.read(cx).editor.clone();
                editor.update(cx, |editor, cx| {
                    editor.dismiss_lsp_overlays(cx);
                    editor.set_cursor_position(lsp_types::Position::new(0, 8), window, cx);
                });
                window.render_frame(cx);
                let position = editor.read(cx).cursor_layout().unwrap().0.origin
                    + gpui_kit::point(px(2.), px(5.));
                window.dispatch_event(
                    gpui_kit::PlatformInput::MouseMove(gpui_kit::MouseMoveEvent {
                        position,
                        ..Default::default()
                    }),
                    cx,
                );
            })
            .unwrap();
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            let mut hovered = false;
            for _ in 0..200 {
                cx.run_until_parked();
                hovered = cx.update(|cx| view.read(cx).editor.read(cx).hover_popover().is_some());
                if hovered {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(
                hovered,
                "real clangd hover did not reach the native popover"
            );
            client.shutdown().unwrap();
            for _ in 0..100 {
                cx.run_until_parked();
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}
