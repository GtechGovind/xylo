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
mod xylo {
    include!("../src/xylo.rs");
    #[cfg(test)]
    mod native_tests {
        use super::{Page, Xylo};
        use gpui_kit::test::TestWindowExt;
        use gpui_kit::{AppContext, Bounds, TestAppContext, WindowBounds, WindowOptions, px, size};
        use xylo_core::ApiFamily;

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
                            Xylo::new(
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
                assert_eq!(view.read(cx).page, Page::Appearance);
                window.click("nav-providers", cx);
                window.click("add-provider", cx);
                window.click("preset-anthropic", cx);
                assert_eq!(
                    view.read(cx).provider_from_form(cx).unwrap().family,
                    ApiFamily::Anthropic
                );
                assert_eq!(
                    view.read(cx).endpoint.read(cx).value().as_ref(),
                    "https://api.anthropic.com"
                );
                window.click("provider-change-type", cx);
                window.click("preset-local", cx);
                assert_eq!(
                    view.read(cx).provider_from_form(cx).unwrap().family,
                    ApiFamily::OpenAiChat
                );
                assert!(
                    view.read(cx)
                        .endpoint
                        .read(cx)
                        .value()
                        .starts_with("http://127.0.0.1:")
                );
                window.click("nav-context", cx);
                assert_eq!(view.read(cx).page, Page::Context);
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
                            Xylo::new(
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
                    view.workspace = Some(std::path::PathBuf::from("/fixture/project"));
                    view.workspace_ready = true;
                    view.sidebar_state.dirty = true;
                    view.turns
                        .entry(old)
                        .or_default()
                        .apply(xylo_core::EngineEvent::Started);
                    cx.notify();
                });
                window.render_frame(cx);
                window.click(("project-new-chat", 0usize), cx);
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
                    cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("No external host")), window, cx))
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
                    cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("No external host")), window, cx))
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
                        xylo_runtime::host::HostEvent::Saved(path.clone()),
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
                        xylo_runtime::host::HostEvent::SaveFailed {
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
        ) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<Xylo>) {
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
                        cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("No external host")), window, cx))
                    },
                )
                .unwrap()
            });
            (window, view)
        }
        fn wait_for_history(
            cx: &mut TestAppContext,
            view: &gpui_kit::Entity<Xylo>,
            description: &str,
            condition: impl Fn(&Xylo, &gpui_kit::App) -> bool,
        ) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                // Drive the native receiver's bounded-backlog yield timer.
                cx.executor()
                    .advance_clock(std::time::Duration::from_millis(2));
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
            use xylo_core::{ChatMessage, Conversation, HistoryPage};
            use xylo_runtime::{
                AppPaths,
                history::{HistorySearchPage, HistoryWindow},
                host::HostClient,
            };
            use xylo_services::Store;
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
                    |window, cx| cx.new(|cx| Xylo::new(Ok(host), window, cx)),
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
                        xylo_core::HistoryPage {
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
                        xylo_core::HistoryPage {
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
                        xylo_core::HistoryPage {
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
                    let mut settings = xylo_core::AppSettings {
                        theme: "dark".into(),
                        ..Default::default()
                    };
                    settings.ui.sidebar_width = 312.;
                    settings.ui.active_page = "appearance".into();
                    view.event(
                        xylo_runtime::host::HostEvent::Settings(settings),
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
                let edge = window.find("sidebar-resize").bounds().center();
                window.drag(edge, edge + gpui_kit::point(px(24.), px(0.)), cx);
                assert!(
                    view.read(cx).settings.ui.sidebar_width > 330.,
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
                    view.event(xylo_runtime::host::HostEvent::Capability{
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
            let host = xylo_runtime::host::HostClient::spawn_at(
                xylo_runtime::AppPaths::at(workspace.join("profile")).unwrap(),
            )
            .unwrap();
            let client = host.0.clone();
            cx.update(gpui_kit::init);
            let (handle, view) = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| Xylo::new(Ok(host), window, cx))
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
                cx.executor()
                    .advance_clock(std::time::Duration::from_millis(2));
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
                cx.executor()
                    .advance_clock(std::time::Duration::from_millis(2));
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
    #[cfg(test)]
    mod design_tests {
        use super::{Page, Xylo};
        use gpui_kit::test::TestWindowExt;
        use gpui_kit::{AnyWindowHandle, Entity};
        use gpui_kit::{AppContext, Bounds, TestAppContext, WindowBounds, WindowOptions, px, size};
        use std::path::{Path, PathBuf};
        use xylo_core::{Conversation, EngineEvent, PermissionMode};
        pub(super) fn fixture(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Xylo>) {
            cx.update(gpui_kit::init);
            cx.update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Default::default(),
                            size: size(px(1380.), px(880.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| {
                        cx.new(|cx| {
                            Xylo::new(Err(anyhow::anyhow!("Isolated UI check")), window, cx)
                        })
                    },
                )
                .unwrap()
            })
        }
        #[gpui_kit::test]
        fn short_chat_messages_do_not_reserve_empty_attachment_or_action_space(cx: &mut TestAppContext) {
            let (handle, view) = fixture(cx);
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.error = None;
                    view.settings.ui.sidebar_visible = false;
                    view.messages = vec![
                        super::message(view.session, "user", "hi".into()),
                        super::message(view.session, "assistant", "Hi! What can I help you with?".into()),
                        super::message(view.session, "user", "Study this project and explain the agent flow.".into()),
                    ];
                    view.sync_history_list(cx);
                });
                window.render_frame(cx);
                let user = window.find(("chat-message", 1usize)).bounds();
                let answer = window.find(("chat-message", 2usize)).bounds();
                let next = window.find(("chat-message", 3usize)).bounds();
                assert!(user.size.height <= px(40.), "A short user message must hug its text, not reserve invisible controls: {user:?}");
                assert!(answer.size.height <= px(50.), "A short answer must not reserve an empty attachment strip: {answer:?}");
                assert!(answer.top() - user.bottom() <= px(24.), "Turn spacing must stay compact");
                assert!(next.top() - answer.bottom() <= px(24.), "Followup spacing must stay compact");
                window.hover(("message-body", 1usize), cx);
                window.click(("copy-message", 1usize), cx);
                assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), "hi");
                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string("changed".into()));
                window.hover(("message-body", 3usize), cx);
                assert_eq!(window.find(("copy-message", 1usize)).focused(), Some(true));
                assert!(window.find(("copy-message", 1usize)).visible(), "Keyboard focus must reveal the copy action after pointer leaves");
                window.press("enter", cx);
                assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), "hi");
            }).unwrap();
        }
        #[gpui_kit::test]
        fn sidebar_project_action_and_chat_rows_share_consistent_geometry(cx: &mut TestAppContext) {
            let (handle, view) = fixture(cx);
            for width in [200., 222., 300.] {
                cx.update_window(handle, |_, window, cx| {
                    view.update(cx, |view, _cx| {
                        let root = PathBuf::from("/fixture/xylo");
                        view.workspace = Some(root.clone());
                        view.settings.ui.sidebar_visible = true;
                        view.settings.ui.sidebar_width = width;
                        view.settings.ui.open_projects = vec![root.clone()];
                        view.conversations = vec![Conversation { id: uuid::Uuid::new_v4(), title: "hi".into(), workspace: root, updated_at: 1 }];
                        view.messages.clear();
                        view.sidebar_state.dirty = true;
                    });
                    window.render_frame(cx);
                    let add = window.find(("project-new-chat", 0usize)).bounds();
                    assert!((f32::from(add.right()) - (width - 8.)).abs() <= 2., "Project action should align to sidebar trailing edge: {add:?}, sidebar {width}");
                    let draft = window.find("unsaved-chat").bounds();
                    let saved = window.find(("conversation", 2usize)).bounds();
                    assert_eq!(draft.left(), saved.left());
                    assert_eq!(draft.size, saved.size);
                    assert!(draft.size.height <= px(28.), "Chat controls should be compact");
                    assert!(draft.left() >= px(40.), "Chats must visibly nest under the project");
                }).unwrap();
            }
        }
        #[gpui_kit::test]
        fn settings_routes_and_controls_preserve_the_active_chat(cx: &mut TestAppContext) {
            let (handle, view) = super::design_tests::fixture(cx);
            cx.update_window(handle, |_, window, cx| {
                let session = view.read(cx).session;
                window.render_frame(cx);
                window.click("settings", cx);
                assert!(view.read(cx).settings_open);
                assert_eq!(view.read(cx).page, Page::Appearance);
                window.click("nav-permissions", cx);
                window.click(("permission-setting", 1usize), cx);
                assert_eq!(
                    view.read(cx).settings.permissions,
                    PermissionMode::AcceptEdits
                );
                window.click("nav-agent", cx);
                window.click("agent-timeout-30", cx);
                assert_eq!(view.read(cx).settings.provider.timeout_seconds, 30);
                window.click("nav-context", cx);
                assert_eq!(view.read(cx).page, Page::Context);
                window.click("manage-context-connections", cx);
                assert_eq!(view.read(cx).page, Page::Connectors);
                window.click("back-to-chat", cx);
                assert!(!view.read(cx).settings_open);
                assert_eq!(view.read(cx).page, Page::Chat);
                assert_eq!(view.read(cx).session, session);
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn collapsing_a_project_retains_its_chat_and_background_run(cx: &mut TestAppContext) {
            let (handle, view) = super::design_tests::fixture(cx);
            cx.update_window(handle, |_, window, cx| {
                let session = view.read(cx).session;
                view.update(cx, |view, cx| {
                    let path = PathBuf::from("/fixture/project");
                    view.workspace = Some(path.clone());
                    view.workspace_ready = true;
                    view.conversations = vec![Conversation {
                        id: session,
                        title: "Keep working".into(),
                        workspace: path,
                        updated_at: 0,
                    }];
                    view.turns
                        .entry(session)
                        .or_default()
                        .apply(EngineEvent::Started);
                    view.sidebar_state.dirty = true;
                    cx.notify();
                });
                window.render_frame(cx);
                window.click(("project", 0usize), cx);
                assert!(
                    view.read(cx)
                        .sidebar_state
                        .collapsed
                        .contains(Path::new("/fixture/project"))
                );
                assert!(view.read(cx).turns[&session].running);
                assert_eq!(view.read(cx).conversations.len(), 1);
                window.click(("project", 0usize), cx);
                assert!(view.read(cx).sidebar_state.collapsed.is_empty());
                assert!(view.read(cx).turns[&session].running);
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn late_git_results_cannot_replace_another_project_or_request(cx: &mut TestAppContext) {
            let (handle, view) = super::design_tests::fixture(cx);
            cx.update_window(handle, |_, _, cx| {
                view.update(cx, |view, cx| {
                    let root = PathBuf::from("/current/project");
                    let old_root = PathBuf::from("/previous/project");
                    let request = uuid::Uuid::new_v4();
                    let old_request = uuid::Uuid::new_v4();
                    view.workspace = Some(root.clone());
                    view.git_panel.request = Some(request);
                    view.git_panel.diff_request = Some(request);
                    view.git_panel.status = Some(xylo_runtime::workspace::git::GitStatus {
                        branch: "current".into(),
                        changes: Vec::new(),
                        truncated: false,
                    });
                    view.diff = "current diff".into();
                    for (id, received_root) in [(request, old_root), (old_request, root.clone())] {
                        view.git_status_received(
                            id,
                            received_root.clone(),
                            Ok(xylo_runtime::workspace::git::GitStatus {
                                branch: "stale".into(),
                                changes: Vec::new(),
                                truncated: false,
                            }),
                            cx,
                        );
                        view.git_diff_received(id, received_root, Ok("stale diff".into()), cx);
                    }
                    assert_eq!(view.git_panel.status.as_ref().unwrap().branch, "current");
                    assert_eq!(view.git_panel.request, Some(request));
                    assert_eq!(view.git_panel.diff_request, Some(request));
                    assert_eq!(view.diff, "current diff");
                    view.git_diff_received(request, root, Ok("fresh diff".into()), cx);
                    assert_eq!(view.diff, "fresh diff");
                    assert!(view.git_panel.diff_request.is_none());
                });
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn review_regression_legacy_file_routes_can_close_the_work_panel(cx: &mut TestAppContext) {
            let (handle, view) = super::design_tests::fixture(cx);
            for page in [Page::Files, Page::Changes] {
                cx.update_window(handle, |_, window, cx| {
                    view.update(cx, |view, cx| {
                        view.page = page;
                        view.settings_open = false;
                        view.settings.ui.work_panel_open = true;
                        cx.notify();
                    });
                    window.render_frame(cx);
                    assert!(window.try_find("work-tab-files").is_some());
                    window.click("toggle-work-panel", cx);
                    assert_eq!(view.read(cx).page, Page::Chat);
                    assert!(!view.read(cx).settings.ui.work_panel_open);
                    window.render_frame(cx);
                    assert!(window.try_find("work-tab-files").is_none());
                })
                .unwrap();
            }
        }
        #[gpui_kit::test]
        fn review_regression_files_resize_uses_the_work_panels_origin(cx: &mut TestAppContext) {
            let (handle, view) = super::design_tests::fixture(cx);
            for sidebar_visible in [true, false] {
                cx.update_window(handle, |_, window, cx| {
                    view.update(cx, |view, cx| {
                        view.page = Page::Chat;
                        view.settings.ui.sidebar_visible = sidebar_visible;
                        view.settings.ui.work_panel_open = true;
                        view.settings.ui.work_panel = "files".into();
                        view.settings.ui.work_panel_width = 600.;
                        view.settings.ui.files_width = 240.;
                        cx.notify();
                    });
                    window.render_frame(cx);
                    let edge = window.find("files-resize").bounds().center();
                    window.drag(edge, edge + gpui_kit::point(px(24.), px(0.)), cx);
                    window.render_frame(cx);
                    let width = view.read(cx).settings.ui.files_width;
                    let moved = f32::from(window.find("files-resize").bounds().center().x - edge.x);
                    assert!((moved - 24.).abs() < 4., "visible file split should move 24px, moved {moved} with sidebar={sidebar_visible}");
                    assert!(
                        (width - 264.).abs() < 8.,
                        "expected local 24px resize, got {width} with sidebar={sidebar_visible}"
                    );
                })
                .unwrap();
            }
        }
        #[gpui_kit::test]
        fn review_regression_never_sent_projects_persist_after_workspace_open(
            cx: &mut TestAppContext,
        ) {
            cx.executor().allow_parking();
            let temporary = tempfile::tempdir().unwrap();
            let base = std::fs::canonicalize(temporary.path()).unwrap();
            let roots = [base.join("first"), base.join("second")];
            for root in &roots {
                std::fs::create_dir(root).unwrap();
            }
            let paths = xylo_runtime::AppPaths::at(base.join("profile")).unwrap();
            let store = xylo_services::Store::open(&paths.database).unwrap();
            let host = xylo_runtime::host::HostClient::spawn_at(paths).unwrap();
            let client = host.0.clone();
            cx.update(gpui_kit::init);
            let (handle, view) = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| Xylo::new(Ok(host), window, cx))
                })
                .unwrap()
            });
            let wait = |cx: &mut TestAppContext, condition: &dyn Fn(&gpui_kit::App) -> bool| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    cx.executor()
                        .advance_clock(std::time::Duration::from_millis(2));
                    cx.run_until_parked();
                    if cx.update(|cx| condition(cx)) {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "timed out waiting for persisted open projects"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            };
            wait(cx, &|cx| view.read(cx).settings_loaded);
            for root in &roots {
                cx.update_window(handle, |_, window, cx| {
                    view.update(cx, |view, cx| view.set_workspace(root.clone(), window, cx))
                })
                .unwrap();
                wait(cx, &|cx| {
                    view.read(cx).workspace_ready && view.read(cx).workspace.as_ref() == Some(root)
                });
            }
            wait(cx, &|_| {
                store
                    .settings()
                    .unwrap()
                    .ui
                    .open_projects
                    .iter()
                    .filter(|path| roots.contains(path))
                    .count()
                    == 2
            });
            let settings = store.settings().unwrap();
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.event(
                        xylo_runtime::host::HostEvent::Settings(settings),
                        window,
                        cx,
                    );
                    assert!(
                        roots
                            .iter()
                            .all(|root| view.settings.ui.open_projects.contains(root))
                    );
                    assert!(
                        view.conversations
                            .iter()
                            .all(|chat| !roots.contains(&chat.workspace))
                    );
                })
            })
            .unwrap();
            client.shutdown().unwrap();
        }
        #[gpui_kit::test]
        fn project_panel_preferences_restore_independently(cx: &mut TestAppContext) {
            let (handle, view) = super::design_tests::fixture(cx);
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    let first = PathBuf::from("/fixture/first");
                    let second = PathBuf::from("/fixture/second");
                    view.workspace = Some(first.clone());
                    view.workspace_ready = true;
                    view.settings.ui.work_panel_open = true;
                    view.settings.ui.work_panel = "changes".into();
                    view.settings.ui.work_panel_width = 700.;
                    view.set_workspace(second.clone(), window, cx);
                    assert_eq!(view.workspace.as_ref(), Some(&second));
                    // The fixture has no host: acknowledge the second project before editing its durable layout.
                    view.workspace_ready = true;
                    view.settings.ui.work_panel_open = true;
                    view.settings.ui.work_panel = "terminal".into();
                    view.settings.ui.work_panel_width = 900.;
                    view.set_workspace(first.clone(), window, cx);
                    assert_eq!(view.workspace.as_ref(), Some(&first));
                    assert!(view.settings.ui.work_panel_open);
                    assert_eq!(view.settings.ui.work_panel, "changes");
                    assert_eq!(view.settings.ui.work_panel_width, 700.);
                    let second_panel = &view.settings.ui.project_panels[&second];
                    assert!(second_panel.open);
                    assert_eq!(second_panel.panel, "terminal");
                    assert_eq!(second_panel.width, 900.);
                })
            })
            .unwrap();
        }
    }

    #[cfg(test)]
    mod provider_editor_tests {
        use super::{ApiFamily, ProviderConfig, Xylo};
        use gpui_kit::test::TestWindowExt;
        use gpui_kit::{AppContext, TestAppContext, WindowOptions};
        #[gpui_kit::test]
        fn provider_editor_uses_selected_identity_without_changing_active_provider(
            cx: &mut TestAppContext,
        ) {
            cx.update(gpui_kit::init);
            let (window, view) = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("No external host")), window, cx))
                })
                .unwrap()
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    let active = ProviderConfig {
                        name: "Active A".into(),
                        family: ApiFamily::Anthropic,
                        endpoint: "https://api.anthropic.com".into(),
                        credential_ref: Some("secret-a".into()),
                        model: "model-a".into(),
                        ..Default::default()
                    };
                    view.settings.provider = active.clone();
                    let mut selected = ProviderConfig {
                        name: "Selected B".into(),
                        family: ApiFamily::Gemini,
                        endpoint: "https://generativelanguage.googleapis.com".into(),
                        credential_ref: Some("secret-b".into()),
                        model: "model-b".into(),
                        ..Default::default()
                    };
                    selected.headers.insert("X-Selected".into(), "B".into());
                    view.provider_form(&selected, window, cx);
                    let edited = view.provider_from_form(cx).unwrap();
                    assert_eq!(edited.family, ApiFamily::Gemini);
                    assert_eq!(edited.credential_ref.as_deref(), Some("secret-b"));
                    assert_eq!(edited.headers, selected.headers);
                    assert_eq!(view.settings.provider.family, ApiFamily::Anthropic);
                    assert_eq!(
                        view.settings.provider.credential_ref.as_deref(),
                        Some("secret-a")
                    );
                    Xylo::set_input(&view.endpoint, "https://different.example/v1", window, cx);
                    let moved = view.provider_from_form(cx).unwrap();
                    assert!(
                        moved.credential_ref.is_none(),
                        "An edited endpoint must not inherit credentials"
                    );
                })
            })
            .unwrap();
        }

        #[gpui_kit::test]
        fn custom_provider_form_serializes_headers_and_discovery_without_inheriting_active_auth(
            cx: &mut TestAppContext,
        ) {
            use super::Page;
            use gpui_kit::test::{TestQueryExt, TestWindowExt};
            cx.update(gpui_kit::init);
            let (window, view) = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("No external host")), window, cx))
                })
                .unwrap()
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.settings.provider.credential_ref = Some("active-secret".into());
                    view.settings.provider.family = ApiFamily::Anthropic;
                    view.page = Page::Providers;
                    view.settings_open = true;
                    view.provider_form_open = true;
                    view.choose_provider_kind("Custom", window, cx);
                    Xylo::set_input(&view.endpoint, "https://custom.example/v1", window, cx);
                    Xylo::set_input(
                        &view.provider_discovery_url,
                        "https://catalog.example/models",
                        window,
                        cx,
                    );
                    view.provider_headers.update(cx, |input, cx| {
                        input.set_value("{\"X-Api-Key\":\"explicit-custom-header\"}", window, cx)
                    });
                    let custom = view.provider_from_form(cx).unwrap();
                    assert_eq!(
                        custom.headers.get("X-Api-Key").map(String::as_str),
                        Some("explicit-custom-header")
                    );
                    assert_eq!(
                        custom.discovery_url.as_deref(),
                        Some("https://catalog.example/models")
                    );
                    assert!(custom.credential_ref.is_none());
                    assert_eq!(
                        view.settings.provider.credential_ref.as_deref(),
                        Some("active-secret")
                    );
                });
                window.render_frame(cx);
                assert!(
                    window
                        .try_find_by_label("Extra headers (JSON object)")
                        .is_some()
                );
                assert!(
                    window
                        .try_find_by_label("Model discovery URL (optional)")
                        .is_some()
                );
                view.update(cx, |view, cx| {
                    view.choose_provider_kind("OpenAI", window, cx)
                });
                window.render_frame(cx);
                assert!(window.try_find_by_label("Base URL").is_none());
                assert!(
                    window
                        .try_find_by_label("Extra headers (JSON object)")
                        .is_none()
                );
                view.update(cx, |view, cx| {
                    let clean = view.provider_from_form(cx).unwrap();
                    assert!(clean.headers.is_empty());
                    assert!(clean.discovery_url.is_none());
                    assert!(clean.credential_ref.is_none());
                });
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn controlled_approval_controls_route_allow_once_always_and_deny(cx: &mut TestAppContext) {
            let (handle, view) = super::design_tests::fixture(cx);
            let (sender, receiver) = std::sync::mpsc::sync_channel(8);
            cx.update(|cx| {
                view.update(cx, |view, _| view.validation_worker_commands = Some(sender))
            });
            for (control, expected_allow, expected_remember) in [
                ("allow", true, false),
                ("allow-always", true, true),
                ("deny", false, false),
            ] {
                let action = uuid::Uuid::new_v4();
                let session = cx.update(|cx| view.read(cx).session);
                cx.update_window(handle,|_,window,cx| {
                    view.update(cx,|view,cx| {
                        view.event(xylo_runtime::host::HostEvent::Engine(xylo_core::Envelope::new(uuid::Uuid::new_v4(),Some(session),1,xylo_core::EngineEvent::Started)),window,cx);
                        view.event(xylo_runtime::host::HostEvent::Engine(xylo_core::Envelope::new(uuid::Uuid::new_v4(),Some(session),2,xylo_core::EngineEvent::ApprovalRequired { action_id:action,name:"write_file".into(),arguments:serde_json::json!({"path":"hello.txt","content":"Hello"}),rule:Some("write hello.txt".into()) })),window,cx);
                    });
                    window.render_frame(cx);
                    window.click((control,0usize),cx);
                    let turn=&view.read(cx).turns[&session];
                    assert!(!turn.tools[0].approval);
                    if !expected_allow { assert_eq!(turn.tools[0].success,Some(false)); }
                }).unwrap();
                match receiver.try_recv().expect("UI decision dispatched") {
                    (
                        actual_session,
                        xylo_core::WorkerCommand::Approve {
                            action_id,
                            allow,
                            remember,
                        },
                    ) => {
                        assert_eq!(actual_session, session);
                        assert_eq!(action_id, action);
                        assert_eq!(allow, expected_allow);
                        assert_eq!(remember, expected_remember);
                    }
                    other => panic!("Unexpected UI dispatch: {other:?}"),
                }
            }
            assert!(receiver.try_recv().is_err());
        }
        #[gpui_kit::test]
        fn controlled_stop_control_preserves_partial_output_and_background_chat(
            cx: &mut TestAppContext,
        ) {
            let (handle, view) = super::design_tests::fixture(cx);
            let (sender, receiver) = std::sync::mpsc::sync_channel(4);
            let background = uuid::Uuid::new_v4();
            let session = cx.update(|cx| view.read(cx).session);
            cx.update_window(handle,|_,window,cx| {
                view.update(cx,|view,cx|{
                    view.validation_worker_commands=Some(sender);
                    for (id,text) in [(session,"Partial response"),(background,"Background response")] {
                        view.event(xylo_runtime::host::HostEvent::Engine(xylo_core::Envelope::new(uuid::Uuid::new_v4(),Some(id),1,xylo_core::EngineEvent::Started)),window,cx);
                        view.event(xylo_runtime::host::HostEvent::Engine(xylo_core::Envelope::new(uuid::Uuid::new_v4(),Some(id),2,xylo_core::EngineEvent::Delta {text:text.into()})),window,cx);
                    }
                });
                window.render_frame(cx);window.click("cancel-task",cx);
                assert!(matches!(receiver.try_recv(),Ok((id,xylo_core::WorkerCommand::Cancel)) if id==session));
                view.update(cx,|view,cx|view.event(xylo_runtime::host::HostEvent::Engine(xylo_core::Envelope::new(uuid::Uuid::new_v4(),Some(session),3,xylo_core::EngineEvent::Cancelled)),window,cx));
                assert!(!view.read(cx).turns[&session].running);
                assert_eq!(view.read(cx).turns[&session].text,"Partial response");
                assert!(view.read(cx).turns[&background].running);
                assert_eq!(view.read(cx).turns[&background].text,"Background response");
            }).unwrap();
        }
    }
    #[cfg(test)]
    mod responsiveness_tests {
        use super::Xylo;
        use gpui_kit::test::TestWindowExt;
        use gpui_kit::{
            AppContext, Bounds, EntityInputHandler, TestAppContext, WindowBounds, WindowOptions,
            px, size,
        };
        #[gpui_kit::test]
        fn thousand_delta_receiver_yields_to_native_input_and_preserves_every_token(
            cx: &mut TestAppContext,
        ) {
            use std::sync::{Arc, Mutex};
            use std::time::{Duration, Instant};
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let paths = xylo_runtime::AppPaths::at(directory.path().join("profile")).unwrap();
            let (client, _actual_events) = xylo_runtime::host::HostClient::spawn_at(paths).unwrap();
            let (sender, receiver) = async_channel::unbounded();
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
                    |window, cx| cx.new(|cx| Xylo::new(Ok((client.clone(), receiver)), window, cx)),
                )
                .unwrap()
            });
            cx.run_until_parked();
            let session = cx.update(|cx| view.read(cx).session);
            let chunk = "A realistic reasoning token batch with Unicode αβ and ordered output. ";
            let expected = chunk.repeat(1000);
            let stream = uuid::Uuid::new_v4();
            sender
                .try_send(xylo_runtime::host::HostEvent::Engine(
                    xylo_core::Envelope::new(
                        stream,
                        Some(session),
                        0,
                        xylo_core::EngineEvent::Started,
                    ),
                ))
                .unwrap();
            for index in 0..1000 {
                sender
                    .try_send(xylo_runtime::host::HostEvent::Engine(
                        xylo_core::Envelope::new(
                            stream,
                            Some(session),
                            index + 1,
                            xylo_core::EngineEvent::Delta { text: chunk.into() },
                        ),
                    ))
                    .unwrap();
            }
            let observed = Arc::new(Mutex::new(None));
            let receipt = observed.clone();
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |_, cx| {
                    cx.spawn_in(window, async move |view, cx| {
                        loop {
                            let ready = view
                                .update_in(cx, |view, window, cx| {
                                    let count =
                                        view.turns.get(&session).map_or(0, |turn| turn.text.len());
                                    if count == 0 {
                                        return false;
                                    }
                                    view.composer.update(cx, |input, cx| {
                                        input.focus(window, cx);
                                        input.replace_text_in_range(
                                            None,
                                            "Follow up while reasoning αβ",
                                            window,
                                            cx,
                                        );
                                    });
                                    *receipt.lock().unwrap() = Some(count);
                                    true
                                })
                                .unwrap();
                            if ready {
                                break;
                            }
                            cx.background_executor()
                                .timer(Duration::from_millis(1))
                                .await;
                        }
                    })
                    .detach();
                })
            })
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                cx.run_until_parked();
                if cx.update(|cx| {
                    view.read(cx)
                        .turns
                        .get(&session)
                        .is_some_and(|turn| turn.text.len() == expected.len())
                }) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "Receiver failed to deliver all stream events: {} bytes, {} queued, input {:?}",
                    cx.update(|cx| view
                        .read(cx)
                        .turns
                        .get(&session)
                        .map_or(0, |turn| turn.text.len())),
                    sender.len(),
                    observed.lock().unwrap()
                );
                // GPUI's timer clock is virtual; real sleeps do not advance it.
                cx.executor().advance_clock(Duration::from_millis(2));
                std::thread::sleep(Duration::from_millis(2));
            }
            let delivered = observed.lock().unwrap().expect("Input task must run");
            assert!(
                delivered > 0 && delivered < expected.len(),
                "Input must run between stream batches, observed {delivered} of {} bytes",
                expected.len()
            );
            cx.update(|cx| {
                assert_eq!(view.read(cx).turns[&session].text, expected);
                assert_eq!(
                    view.read(cx).composer.read(cx).value().as_ref(),
                    "Follow up while reasoning αβ"
                );
                assert!(view.read(cx).turns[&session].running);
            });
            client.shutdown().unwrap();
        }
        #[gpui_kit::test]
        fn long_live_response_typing_does_not_invalidate_unchanged_history(
            cx: &mut TestAppContext,
        ) {
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
                            Xylo::new(
                                Err(anyhow::anyhow!("Controlled stream fixture")),
                                window,
                                cx,
                            )
                        })
                    },
                )
                .unwrap()
            });
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.error = None;
                    let turn = view.turns.entry(view.session).or_default();
                    turn.apply(xylo_core::EngineEvent::Started);
                    turn.apply(xylo_core::EngineEvent::Delta {
                        text: "Reasoning through the implementation with Unicode αβ.\n\n"
                            .repeat(2000),
                    });
                    view.sync_history_list(cx);
                });
                window.render_frame(cx);
                let before = view.read(cx).histories[&view.read(cx).session].validation_remeasures;
                for text in ["Continue", " with", " the", " second", " message αβ"] {
                    let composer = view.read(cx).composer.clone();
                    composer.update(cx, |input, cx| {
                        input.focus(window, cx);
                        input.replace_text_in_range(None, text, window, cx);
                    });
                    view.update(cx, |_, cx| cx.notify());
                    window.render_frame(cx);
                }
                assert_eq!(
                    view.read(cx).composer.read(cx).value().as_ref(),
                    "Continue with the second message αβ"
                );
                assert_eq!(
                    view.read(cx).histories[&view.read(cx).session].validation_remeasures,
                    before,
                    "Typing must reuse unchanged live history measurements"
                );
            })
            .unwrap();
        }
    }

    #[cfg(test)]
    mod tool_results_tests {
        use super::Xylo;
        use gpui_kit::test::TestWindowExt;
        use gpui_kit::{AppContext, TestAppContext, WindowOptions};
        #[gpui_kit::test]
        fn file_results_expand_to_rows_with_secondary_raw_disclosure(cx: &mut TestAppContext) {
            cx.update(gpui_kit::init);
            let (handle, view) = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| {
                        Xylo::new(Err(anyhow::anyhow!("Controlled tool fixture")), window, cx)
                    })
                })
                .unwrap()
            });
            cx.update_window(handle,|_,window,cx|{
                view.update(cx,|view,cx|{
                    view.error=None;let id=uuid::Uuid::new_v4();let turn=view.turns.entry(view.session).or_default();
                    turn.apply(xylo_core::EngineEvent::Started);
                    turn.apply(xylo_core::EngineEvent::ToolStarted {action_id:id,name:"files_list".into(),arguments:serde_json::json!({"path":"crates"})});
                    turn.apply(xylo_core::EngineEvent::ToolFinished {action_id:id,name:"files_list".into(),result:serde_json::json!({"status":"success","summary":{"entries":["core/","desktop/","Cargo.toml"]}}).to_string(),success:true});
                    cx.notify();
                });
                window.render_frame(cx);assert!(window.try_find(("tool-files",0usize)).is_none());
                window.click(("tool-detail",0usize),cx);
                assert!(window.try_find(("tool-files",0usize)).is_some(),"Expanded listing must show semantic rows");
                assert!(window.try_find(("tool-raw-input",0usize)).is_none(),"Raw arguments must stay hidden");
                window.click(("tool-raw-toggle",0usize),cx);
                assert!(window.try_find(("tool-raw-input",0usize)).is_some());
                window.click(("tool-raw-toggle",0usize),cx);
                assert!(window.try_find(("tool-raw-input",0usize)).is_none());
                window.click(("tool-detail",0usize),cx);
                assert!(window.try_find(("tool-files",0usize)).is_none());
            }).unwrap();
        }
        #[gpui_kit::test]
        fn large_tool_result_reuses_cached_presentation_across_native_frames(
            cx: &mut TestAppContext,
        ) {
            cx.update(gpui_kit::init);
            let (handle, view) = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| {
                        Xylo::new(Err(anyhow::anyhow!("Controlled large result")), window, cx)
                    })
                })
                .unwrap()
            });
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.error = None;
                    let id = uuid::Uuid::new_v4();
                    let turn = view.turns.entry(view.session).or_default();
                    turn.apply(xylo_core::EngineEvent::Started);
                    turn.apply(xylo_core::EngineEvent::ToolStarted {
                        action_id: id,
                        name: "files_list".into(),
                        arguments: serde_json::json!({"path":"src"}),
                    });
                    let entries = (0..2000)
                        .map(|index| format!("folder-{index}-{}", "unicode-αβ".repeat(8)))
                        .collect::<Vec<_>>();
                    turn.apply(xylo_core::EngineEvent::ToolFinished {
                        action_id: id,
                        name: "files_list".into(),
                        result:
                            serde_json::json!({"status":"success","summary":{"entries":entries}})
                                .to_string(),
                        success: true,
                    });
                    cx.notify();
                });
                let cached = view.read(cx).turns[&view.read(cx).session].tools[0]
                    .presentation
                    .parsed_output
                    .clone()
                    .unwrap();
                window.render_frame(cx);
                window.click(("tool-detail", 0usize), cx);
                assert!(window.try_find(("tool-files", 0usize)).is_some());
                for _ in 0..20 {
                    view.update(cx, |_, cx| cx.notify());
                    window.render_frame(cx);
                }
                let tool = &view.read(cx).turns[&view.read(cx).session].tools[0];
                assert!(
                    std::sync::Arc::ptr_eq(
                        &cached,
                        tool.presentation.parsed_output.as_ref().unwrap()
                    ),
                    "Rendering must not parse/rebuild the result presentation"
                );
                let crate::state::tool_presentation::ToolBody::Files { entries, remaining } =
                    &tool.presentation.body
                else {
                    panic!("Expected file rows")
                };
                assert_eq!(entries.len(), 24);
                assert_eq!(*remaining, 1976);
                assert!(tool.result.as_ref().unwrap().len() > 100_000);
                assert!(
                    tool.presentation
                        .raw_result
                        .as_ref()
                        .unwrap()
                        .chars()
                        .count()
                        <= 12_001
                );
            })
            .unwrap();
        }

        #[gpui_kit::test]
        fn read_search_command_and_delegate_expand_without_raw_envelopes(cx: &mut TestAppContext) {
            cx.update(gpui_kit::init);
            let (handle, view) = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| {
                        Xylo::new(
                            Err(anyhow::anyhow!("Controlled structured outputs")),
                            window,
                            cx,
                        )
                    })
                })
                .unwrap()
            });
            cx.update_window(handle,|_,window,cx|{
                let cases=[
                    ("file_read",serde_json::json!({"path":"src/main.rs"}),serde_json::json!({"content":"    12\tfn main() {}\n","start_line":12,"end_line":12,"total_lines":20}),"tool-read"),
                    ("search_project",serde_json::json!({"query":"main"}),serde_json::json!({"matches":1,"files":1,"results":"src/main.rs:12:fn main() {}\n"}),"tool-search"),
                    ("run_command",serde_json::json!({"program":"cargo","args":["check"]}),serde_json::json!({"exit_code":0,"stdout":"Finished successfully","stderr":""}),"tool-command"),
                    ("agent_delegate",serde_json::json!({"task":"Review navigation"}),serde_json::json!({"result":"## Findings\nThe sidebar is clear."}),"tool-delegate")
                ];
                for (name,input,summary,body_id) in cases {
                    view.update(cx,|view,cx|{
                        view.expanded_tools.clear();view.error=None;let id=uuid::Uuid::new_v4();let turn=view.turns.entry(view.session).or_default();turn.apply(xylo_core::EngineEvent::Started);
                        turn.apply(xylo_core::EngineEvent::ToolStarted {action_id:id,name:name.into(),arguments:input});
                        turn.apply(xylo_core::EngineEvent::ToolFinished {action_id:id,name:name.into(),result:serde_json::json!({"status":"success","summary":summary}).to_string(),success:true});view.invalidate_live_history();cx.notify();
                    });
                    window.render_frame(cx);window.click(("tool-detail",0usize),cx);
                    assert!(window.try_find((body_id,0usize)).is_some(),"Expected semantic {name} body");
                    assert!(window.try_find(("tool-raw-input",0usize)).is_none());
                    assert!(window.try_find(("tool-raw-result",0usize)).is_none());
                    window.click(("tool-detail",0usize),cx);
                }
            }).unwrap();
        }
    }
}
