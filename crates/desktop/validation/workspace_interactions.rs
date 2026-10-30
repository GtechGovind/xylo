//! Native production UI and real host/index/watch/file operations. Speech uses
//! synthetic correlated host responses exclusively; no audio device is touched.
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
    mod workspace_native_tests {
        use super::{Page, Xylo};
        use gpui_kit::{
            AppContext, Bounds, TestAppContext, WindowBounds, WindowOptions, px, size,
            test::{TestQueryExt, TestWindowExt},
        };
        use serde_json::json;
        use std::{
            path::{Path, PathBuf},
            time::{Duration, Instant},
        };
        use uuid::Uuid;
        use xylo_runtime::{
            AppPaths,
            host::{HostClient, HostEvent},
            workspace::{SearchHit, SearchPage},
        };

        fn open_view(
            cx: &mut TestAppContext,
            host: anyhow::Result<(HostClient, async_channel::Receiver<HostEvent>)>,
        ) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<Xylo>) {
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
                    |window, cx| cx.new(|cx| Xylo::new(host, window, cx)),
                )
                .unwrap()
            });
            (window, view)
        }
        fn wait_for(
            cx: &mut TestAppContext,
            view: &gpui_kit::Entity<Xylo>,
            description: &str,
            condition: impl Fn(&Xylo, &gpui_kit::App) -> bool,
        ) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                // Drive the native receiver's bounded-backlog yield timer.
                cx.executor()
                    .advance_clock(std::time::Duration::from_millis(2));
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
        fn host_fixture(
            cx: &mut TestAppContext,
        ) -> (
            tempfile::TempDir,
            PathBuf,
            HostClient,
            gpui_kit::AnyWindowHandle,
            gpui_kit::Entity<Xylo>,
        ) {
            cx.executor().allow_parking();
            let temporary = tempfile::tempdir().unwrap();
            let base = temporary.path().canonicalize().unwrap();
            let workspace = base.join("project");
            std::fs::create_dir(&workspace).unwrap();
            let host = HostClient::spawn_at(AppPaths::at(base.join("profile")).unwrap()).unwrap();
            let client = host.0.clone();
            let (window, view) = open_view(cx, Ok(host));
            wait_for(cx, &view, "initial settings", |view, _| {
                view.settings_loaded
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.set_workspace(workspace.clone(), window, cx)
                })
            })
            .unwrap();
            (temporary, workspace, client, window, view)
        }

        #[gpui_kit::test]
        fn directory_pages_reach_every_file_and_expansion_preserves_unsaved_editor(
            cx: &mut TestAppContext,
        ) {
            use super::file_browser::RowKey;
            let (_temporary, workspace, client, window, view) = host_fixture(cx);
            let directory = workspace.join("z-folder");
            std::fs::create_dir(&directory).unwrap();
            let child = directory.join("child.txt");
            std::fs::write(&child, "Nested text").unwrap();
            for number in 0..1200 {
                std::fs::write(workspace.join(format!("file-{number:04}.txt")), "Original")
                    .unwrap();
            }
            client.refresh_workspace().unwrap();
            wait_for(cx, &view, "complete bounded catalogue", |view, _| {
                !view.file_browser.indexing
                    && !view.file_browser.loading()
                    && view.file_browser.file_count == 1201
                    && view.file_browser.entries.len() == 500
            });
            let mut seen = std::collections::BTreeSet::new();
            loop {
                let (paths, next) = cx.update(|cx| {
                    let node = view
                        .read(cx)
                        .file_browser
                        .tree
                        .directories
                        .get(&workspace)
                        .unwrap();
                    (
                        node.entries
                            .iter()
                            .map(|entry| entry.path.clone())
                            .collect::<Vec<_>>(),
                        node.next.as_ref().map(|cursor| cursor.path.clone()),
                    )
                });
                assert!(paths.len() <= 1000);
                seen.extend(paths);
                if next.is_none() {
                    break;
                }
                cx.update_window(window, |_, window, cx| {
                    view.update(cx, |view, cx| {
                        view.page = Page::Files;
                        let row = view
                            .file_browser
                            .tree
                            .rows
                            .iter()
                            .position(|row| {
                                row.key.as_ref() == Some(&RowKey::More(workspace.clone()))
                            })
                            .unwrap();
                        view.file_scroll
                            .scroll_to_item(row, gpui_kit::ScrollStrategy::Top);
                        cx.notify();
                    });
                    window.render_frame(cx);
                    let row = view
                        .read(cx)
                        .file_browser
                        .tree
                        .rows
                        .iter()
                        .position(|row| row.key.as_ref() == Some(&RowKey::More(workspace.clone())))
                        .unwrap();
                    window.click(("file", row), cx);
                })
                .unwrap();
                wait_for(cx, &view, "inline directory continuation", |view, _| {
                    !view.file_browser.loading()
                        && view
                            .file_browser
                            .tree
                            .directories
                            .get(&workspace)
                            .unwrap()
                            .next
                            .as_ref()
                            .map(|cursor| &cursor.path)
                            != next.as_ref()
                });
            }
            assert_eq!(
                seen.len(),
                1201,
                "Every root path stays reachable across bounded windows"
            );
            assert!(seen.contains(&directory));
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.activate_tree_row(RowKey::Earlier(workspace.clone()), window, cx)
                });
            })
            .unwrap();
            wait_for(cx, &view, "earlier root entries", |view, _| {
                !view.file_browser.loading()
                    && view
                        .file_browser
                        .entries
                        .first()
                        .is_some_and(|entry| entry.path == directory)
            });
            let edit = workspace.join("file-0000.txt");
            cx.update(|cx| {
                view.update(cx, |view, cx| {
                    view.request_editor_file(edit.clone(), None, false, cx)
                })
            });
            wait_for(cx, &view, "opened fixture", |view, _| {
                view.editor_path.as_ref() == Some(&edit)
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.editor.update(cx, |editor, cx| {
                        editor.set_value("Unsaved local edits", window, cx)
                    });
                    view.editor_dirty = true;
                    view.activate_tree_row(RowKey::Entry(directory.clone()), window, cx);
                });
            })
            .unwrap();
            wait_for(cx, &view, "expanded nested directory", |view, _| {
                !view.file_browser.loading()
                    && view
                        .file_browser
                        .entries
                        .iter()
                        .any(|entry| entry.path == child)
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    assert!(view.editor_dirty);
                    assert_eq!(view.editor.read(cx).value().as_ref(), "Unsaved local edits");
                    // Collapse cancels only that subtree; late replies cannot
                    // replace the root or clear the preserved editor proposal.
                    view.toggle_tree_folder(directory.clone(), cx);
                    view.toggle_tree_folder(directory.clone(), cx);
                    view.load_file_page(None, cx);
                    let stale = view
                        .file_browser
                        .validation_request_for(&workspace)
                        .unwrap();
                    view.reset_file_browser();
                    view.load_file_page(None, cx);
                    view.event(
                        HostEvent::WorkspaceFiles {
                            request_id: stale,
                            result: Err("Stale response must not replace the tree".into()),
                        },
                        window,
                        cx,
                    );
                    assert!(view.file_browser.notice.is_empty());
                });
            })
            .unwrap();
            wait_for(cx, &view, "latest tree refresh", |view, _| {
                !view.file_browser.loading() && !view.file_browser.entries.is_empty()
            });
            assert_eq!(std::fs::read_to_string(edit).unwrap(), "Original");
            client.shutdown().unwrap();
            cx.run_until_parked();
        }

        #[gpui_kit::test]
        fn search_pages_remain_bounded_and_reach_matches_after_two_thousand(
            cx: &mut TestAppContext,
        ) {
            let (_temporary, workspace, client, window, view) = host_fixture(cx);
            let path = workspace.join("many.txt");
            std::fs::write(
                &path,
                (0..2105)
                    .map(|n| format!("needle {n}\n"))
                    .collect::<String>(),
            )
            .unwrap();
            client.refresh_workspace().unwrap();
            wait_for(cx, &view, "search catalogue", |view, _| {
                !view.file_browser.indexing && view.file_browser.file_count == 1
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.open_project_search(window, cx);
                    view.validation_search_query("needle", window, cx);
                });
                window.render_frame(cx);
                window.click("project-search-submit", cx);
            })
            .unwrap();
            let mut lines = std::collections::BTreeSet::new();
            loop {
                wait_for(cx, &view, "search page", |view, _| {
                    view.project_search.validation_request().is_none()
                });
                cx.update(|cx| {
                    let hits = view.read(cx).project_search.validation_hits();
                    assert!(hits.len() <= 100);
                    for hit in hits {
                        assert!(lines.insert(hit.line), "Match repeated across pages");
                    }
                });
                let has_next = cx
                    .update_window(window, |_, window, cx| {
                        window.render_frame(cx);
                        if window.try_find("project-search-more").is_some() {
                            window.click("project-search-more", cx);
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap();
                if !has_next {
                    break;
                }
            }
            assert_eq!(lines.len(), 2105);
            assert!(lines.contains(&2105));
            client.shutdown().unwrap();
            cx.run_until_parked();
        }

        #[gpui_kit::test]
        fn changing_search_query_or_options_invalidates_continuation(cx: &mut TestAppContext) {
            let (_temporary, workspace, client, window, view) = host_fixture(cx);
            let path = workspace.join("queries.txt");
            std::fs::write(
                &path,
                "TARGET first line\n".to_owned() + &"needle\n".repeat(120),
            )
            .unwrap();
            client.refresh_workspace().unwrap();
            wait_for(cx, &view, "query fixture catalogue", |view, _| {
                !view.file_browser.indexing && view.file_browser.file_count == 1
            });
            for change in ["query", "case", "regex"] {
                cx.update_window(window, |_, window, cx| {
                    view.update(cx, |view, cx| {
                        view.open_project_search(window, cx);
                        view.validation_search_query("needle", window, cx);
                    });
                    window.render_frame(cx);
                    window.click("project-search-submit", cx);
                })
                .unwrap();
                wait_for(cx, &view, "first query page", |view, _| {
                    view.project_search.validation_request().is_none()
                        && view.project_search.validation_hits().len() == 100
                });
                cx.update_window(window, |_, window, cx| {
                    window.render_frame(cx);
                    assert!(window.try_find("project-search-more").is_some());
                    match change {
                        "query" => {
                            view.update(cx, |view, cx| view.open_project_search(window, cx));
                            window.press("cmd-a", cx);
                            window.input("TARGET", cx);
                        }
                        "case" => window.click("project-search-case", cx),
                        _ => window.click("project-search-regex", cx),
                    }
                })
                .unwrap();
                cx.run_until_parked();
                cx.update_window(window, |_, window, cx| {
                    window.render_frame(cx);
                    assert!(window.try_find("project-search-more").is_none());
                    assert!(view.read(cx).project_search.validation_hits().is_empty());
                    if change == "query" {
                        window.click("project-search-submit", cx);
                    }
                })
                .unwrap();
                if change == "query" {
                    wait_for(cx, &view, "new query starts at the beginning", |view, _| {
                        view.project_search.validation_request().is_none()
                            && view
                                .project_search
                                .validation_hits()
                                .first()
                                .is_some_and(|hit| hit.line == 1)
                    });
                }
            }
            client.shutdown().unwrap();
            cx.run_until_parked();
        }

        #[gpui_kit::test]
        fn real_search_hit_opens_unicode_byte_position_and_watcher_preserves_dirty_text(
            cx: &mut TestAppContext,
        ) {
            let (_temporary, workspace, client, window, view) = host_fixture(cx);
            let path = workspace.join("unicode.txt");
            let content = "Header\nαβ 🦀 needle in this line\nEnd\n";
            std::fs::write(&path, content).unwrap();
            std::fs::write(workspace.join("other.txt"), "ordinary contents").unwrap();
            client.refresh_workspace().unwrap();
            wait_for(cx, &view, "real workspace index", |view, _| {
                view.file_browser
                    .entries
                    .iter()
                    .any(|file| file.path == path)
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.open_project_search(window, cx);
                    view.validation_search_query("needle", window, cx);
                });
                window.render_frame(cx);
                window.click("project-search-submit", cx);
            })
            .unwrap();
            wait_for(cx, &view, "host search results", |view, _| {
                view.project_search.validation_request().is_none()
                    && view.project_search.validation_hits().len() == 1
            });
            cx.update_window(window, |_, window, cx| {
                let hit = &view.read(cx).project_search.validation_hits()[0];
                assert_eq!(hit.path, path);
                assert_eq!(hit.line, 2);
                assert_eq!(hit.byte_column, "αβ 🦀 ".len());
                window.render_frame(cx);
                window.click(("project-match", 0usize), cx);
            })
            .unwrap();
            wait_for(cx, &view, "real host file open", |view, _| {
                view.editor_path.as_ref() == Some(&path)
            });
            cx.update(|cx| {
                let editor = view.read(cx).editor.read(cx);
                assert_eq!(editor.value().as_ref(), content);
                assert_eq!(editor.cursor(), content.find("needle").unwrap());
            });
            let (editor_id, cursor) = cx.update(|cx| {
                let xylo = view.read(cx);
                (xylo.editor.entity_id(), xylo.editor.read(cx).cursor())
            });
            cx.update(|cx| {
                view.update(cx, |view, cx| {
                    view.request_editor_file(path.clone(), None, true, cx);
                    assert!(view.project_search.validation_opening().is_some());
                })
            });
            wait_for(cx, &view, "unchanged host file refresh", |view, _| {
                view.project_search.validation_opening().is_none()
            });
            cx.update(|cx| {
                let xylo = view.read(cx);
                assert_eq!(
                    xylo.editor.entity_id(),
                    editor_id,
                    "unchanged refresh must retain the editor and its language assistance state"
                );
                assert_eq!(
                    xylo.editor.read(cx).cursor(),
                    cursor,
                    "unchanged refresh must retain the search-hit cursor"
                );
                assert_eq!(xylo.editor.read(cx).value().as_ref(), content);
                assert!(!xylo.project_search.external_conflict);
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.editor.update(cx, |editor, cx| {
                        editor.set_value("Local unsaved αβ 🦀 edits", window, cx)
                    });
                    view.editor_dirty = true;
                    cx.notify();
                })
            })
            .unwrap();
            let revision_before_write =
                cx.update(|cx| view.read(cx).project_search.validation_revision());
            std::fs::write(&path, "Changed externally on disk\n").unwrap();
            wait_for(
                cx,
                &view,
                "native watcher conflict delivered to UI",
                |view, _| {
                    view.project_search.external_conflict
                        && view.project_search.validation_revision() != revision_before_write
                },
            );
            cx.update_window(window, |_, window, cx| {
                assert!(view.read(cx).editor_dirty);
                assert_eq!(
                    view.read(cx).editor.read(cx).value().as_ref(),
                    "Local unsaved αβ 🦀 edits"
                );
                window.render_frame(cx);
                assert!(window.try_find("reload-external-file").is_some());
                window.click("reload-external-file", cx);
                assert_eq!(
                    view.read(cx).editor.read(cx).value().as_ref(),
                    "Local unsaved αβ 🦀 edits"
                );
            })
            .unwrap();
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "Changed externally on disk\n"
            );
            let created = workspace.join("added.txt");
            std::fs::write(&created, "watched addition").unwrap();
            wait_for(cx, &view, "watcher addition in file tree", |view, _| {
                view.file_browser
                    .entries
                    .iter()
                    .any(|file| file.path == created)
            });
            cx.update(|cx| {
                assert_eq!(
                    view.read(cx).editor.read(cx).value().as_ref(),
                    "Local unsaved αβ 🦀 edits"
                )
            });
            client.shutdown().unwrap();
            cx.run_until_parked();
        }

        #[gpui_kit::test]
        fn external_change_save_requires_explicit_overwrite_and_cancel_preserves_both_versions(
            cx: &mut TestAppContext,
        ) {
            let (_temporary, workspace, client, window, view) = host_fixture(cx);
            let path = workspace.join("conflicted.txt");
            let local = "Unsaved editor version αβ 🦀\n";
            let external = "Changed externally on disk\n";
            std::fs::write(&path, "Original contents\n").unwrap();
            client.refresh_workspace().unwrap();
            wait_for(cx, &view, "indexed conflict fixture", |view, _| {
                view.file_browser
                    .entries
                    .iter()
                    .any(|file| file.path == path)
            });
            cx.update(|cx| {
                view.update(cx, |view, cx| {
                    view.request_editor_file(path.clone(), None, false, cx)
                })
            });
            wait_for(cx, &view, "opened conflict fixture", |view, _| {
                view.editor_path.as_ref() == Some(&path)
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.editor
                        .update(cx, |editor, cx| editor.set_value(local, window, cx));
                    view.editor_dirty = true;
                    cx.notify();
                })
            })
            .unwrap();
            let revision_before_write =
                cx.update(|cx| view.read(cx).project_search.validation_revision());
            std::fs::write(&path, external).unwrap();
            wait_for(cx, &view, "external save conflict", |view, _| {
                view.project_search.external_conflict
                    && view.project_search.validation_revision() != revision_before_write
            });

            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.click("save-file", cx);
                assert!(view.read(cx).pending_saves.is_empty());
                assert!(view.read(cx).editor_dirty);
            })
            .unwrap();
            assert!(cx.has_pending_prompt());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), external);
            cx.simulate_prompt_answer("Cancel");
            cx.run_until_parked();
            assert!(!cx.has_pending_prompt());
            cx.update(|cx| {
                let view = view.read(cx);
                assert!(view.pending_saves.is_empty());
                assert!(view.editor_dirty);
                assert!(view.project_search.external_conflict);
                assert_eq!(view.editor.read(cx).value().as_ref(), local);
            });
            assert_eq!(std::fs::read_to_string(&path).unwrap(), external);

            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.click("save-file", cx);
                assert!(view.read(cx).pending_saves.is_empty());
            })
            .unwrap();
            assert!(cx.has_pending_prompt());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), external);
            cx.simulate_prompt_answer("Overwrite");
            wait_for(
                cx,
                &view,
                "explicit overwrite acknowledged by host",
                |view, _| {
                    !view.editor_dirty
                        && !view.project_search.external_conflict
                        && view.pending_saves.is_empty()
                        && view.status.starts_with("Saved ")
                },
            );
            assert!(!cx.has_pending_prompt());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), local);
            cx.update(|cx| assert_eq!(view.read(cx).editor.read(cx).value().as_ref(), local));
            client.shutdown().unwrap();
            cx.run_until_parked();
        }

        #[gpui_kit::test]
        fn stale_search_and_file_responses_cannot_replace_the_latest_selection(
            cx: &mut TestAppContext,
        ) {
            let (_temporary, workspace, client, window, view) = host_fixture(cx);
            let old_path = workspace.join("old.txt");
            let new_path = workspace.join("new.txt");
            std::fs::write(&old_path, "obsolete search token").unwrap();
            std::fs::write(&new_path, "current search token").unwrap();
            client.refresh_workspace().unwrap();
            wait_for(cx, &view, "indexed files", |view, _| {
                view.file_browser.entries.len() == 2
            });
            let mut old_search = None;
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.open_project_search(window, cx);
                    view.validation_search_query("obsolete", window, cx);
                });
                window.render_frame(cx);
                window.click("project-search-submit", cx);
                old_search = view.read(cx).project_search.validation_request();
                view.update(cx, |view, cx| {
                    view.validation_search_query("current", window, cx)
                });
                window.render_frame(cx);
                window.click("project-search-submit", cx);
                view.update(cx, |view, cx| {
                    view.event(
                        HostEvent::ProjectSearch {
                            request_id: old_search.unwrap(),
                            result: Ok(SearchPage {
                                root: workspace.clone(),
                                query: "obsolete".into(),
                                hits: vec![SearchHit {
                                    path: old_path.clone(),
                                    line: 1,
                                    byte_column: 0,
                                    preview: "stale injected search".into(),
                                    match_bytes: 8,
                                }],
                                next: None,
                                skipped_files: 0,
                                timed_out: false,
                                indexing: false,
                            }),
                        },
                        window,
                        cx,
                    )
                });
                assert!(view.read(cx).project_search.validation_hits().is_empty());
            })
            .unwrap();
            wait_for(cx, &view, "latest actual host search", |view, _| {
                view.project_search.validation_request().is_none()
                    && view.project_search.validation_hits().len() == 1
            });
            cx.update(|cx| {
                assert_eq!(
                    view.read(cx).project_search.validation_hits()[0].path,
                    new_path
                )
            });
            let mut stale = None;
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.request_editor_file(old_path.clone(), None, false, cx);
                    stale = view.project_search.validation_opening();
                    view.request_editor_file(new_path.clone(), None, false, cx);
                    let current = view.project_search.validation_opening().unwrap();
                    assert_ne!(stale, Some(current));
                    view.event(
                        HostEvent::OpenedFile {
                            request_id: stale.unwrap(),
                            path: old_path.clone(),
                            result: Ok("STALE READ MUST NOT APPLY".into()),
                        },
                        window,
                        cx,
                    );
                    assert!(view.editor_path.is_none());
                    assert!(!view.editor.read(cx).value().contains("STALE READ"));
                })
            })
            .unwrap();
            wait_for(cx, &view, "latest actual host file", |view, _| {
                view.editor_path.as_ref() == Some(&new_path)
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    assert_eq!(
                        view.editor.read(cx).value().as_ref(),
                        "current search token"
                    );
                    view.event(
                        HostEvent::OpenedFile {
                            request_id: stale.unwrap(),
                            path: old_path.clone(),
                            result: Ok("LATE OLD READ".into()),
                        },
                        window,
                        cx,
                    );
                    assert_eq!(
                        view.editor.read(cx).value().as_ref(),
                        "current search token"
                    );
                    view.request_editor_file(new_path.clone(), None, true, cx);
                    let refresh = view.project_search.validation_opening().unwrap();
                    view.editor.update(cx, |editor, cx| {
                        editor.set_value("New edits made while refresh was pending", window, cx)
                    });
                    view.editor_dirty = true;
                    view.event(
                        HostEvent::OpenedFile {
                            request_id: refresh,
                            path: new_path.clone(),
                            result: Ok("disk refresh snapshot".into()),
                        },
                        window,
                        cx,
                    );
                    assert!(view.project_search.external_conflict);
                    assert_eq!(
                        view.editor.read(cx).value().as_ref(),
                        "New edits made while refresh was pending"
                    );
                })
            })
            .unwrap();
            client.shutdown().unwrap();
            cx.run_until_parked();
        }

        fn capability(id: Uuid, name: &str, value: serde_json::Value) -> HostEvent {
            HostEvent::Capability {
                request_id: id,
                name: name.into(),
                result: Ok(
                    json!({"status":"completed","summary":value.to_string(),"artifacts":[]}),
                ),
            }
        }
        #[gpui_kit::test]
        fn speech_stale_cancelled_and_late_start_events_cannot_fill_the_wrong_draft(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open_view(
                cx,
                Err(anyhow::anyhow!(
                    "No host or audio devices in this synthetic speech test"
                )),
            );
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.error = None;
                    view.composer.update(cx, |composer, cx| {
                        composer.set_value("Existing draft", window, cx)
                    });
                    view.navigate(Page::Voice, cx);
                    assert!(!view.speech.busy());
                    let old = Uuid::new_v4();
                    let latest = Uuid::new_v4();
                    view.speech.validation_request(old, "speech_transcribe");
                    view.speech.validation_request(latest, "speech_transcribe");
                    assert!(view.speech.busy());
                    view.event(
                        capability(
                            old,
                            "speech_transcribe",
                            json!({"text":"wrong old transcript"}),
                        ),
                        window,
                        cx,
                    );
                    assert!(view.speech.transcript.is_empty());
                    assert_eq!(view.composer.read(cx).value().as_ref(), "Existing draft");
                    view.event(
                        capability(
                            latest,
                            "speech_transcribe",
                            json!({"text":"Reviewed current transcript"}),
                        ),
                        window,
                        cx,
                    );
                    assert_eq!(view.speech.transcript, "Reviewed current transcript");
                    assert!(!view.speech.busy());
                    assert_eq!(view.composer.read(cx).value().as_ref(), "Existing draft");
                    let cancelled = Uuid::new_v4();
                    view.speech
                        .validation_request(cancelled, "speech_transcribe");
                    view.stop_speech(cx);
                    view.event(
                        capability(
                            cancelled,
                            "speech_transcribe",
                            json!({"text":"cancelled transcript"}),
                        ),
                        window,
                        cx,
                    );
                    assert_eq!(view.speech.transcript, "Reviewed current transcript");
                    let start = Uuid::new_v4();
                    view.speech
                        .validation_request(start, "helper_audio_record_start");
                    assert!(view.speech.busy());
                    view.stop_speech(cx);
                    view.event(
                        capability(
                            start,
                            "helper_audio_record_start",
                            json!({
                                "state":"recording",
                                "recording_id":Uuid::new_v4(),
                                "sample_rate":48000,
                                "channels":2,
                            }),
                        ),
                        window,
                        cx,
                    );
                    assert_ne!(view.speech.recording_state, "recording");
                    assert_eq!(view.composer.read(cx).value().as_ref(), "Existing draft");
                    view.error = None;
                    view.navigate(Page::Voice, cx);
                });
                window.render_frame(cx);
                window.click("speech-use-transcript", cx);
                assert_eq!(
                    view.read(cx).composer.read(cx).value().as_ref(),
                    "Existing draft\nReviewed current transcript"
                );
                assert_eq!(view.read(cx).page, Page::Chat);
                assert!(
                    !view.read(cx).settings_open,
                    "Add transcript must restore workspace navigation"
                );
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn recording_controls_remain_available_after_settings_navigation(cx: &mut TestAppContext) {
            let (handle, view) = open_view(cx, Err(anyhow::anyhow!("No external device or host")));
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.error = None;
                    view.speech
                        .validation_request(Uuid::new_v4(), "helper_audio_record_start");
                    view.navigate(Page::Providers, cx);
                });
                window.render_frame(cx);
                assert!(
                    window.try_find("global-recording-stop").is_some(),
                    "Recording Stop must remain available outside the composer"
                );
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn schedule_enqueue_preserves_prompt_until_durable_creation(cx: &mut TestAppContext) {
            let (_directory, workspace, client, handle, view) = host_fixture(cx);
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.workspace = Some(workspace);
                    view.schedule_prompt.update(cx, |input, cx| {
                        input.set_value("Review the local notes", window, cx)
                    });
                    view.navigate(Page::Schedules, cx);
                });
                window.render_frame(cx);
                window.click("create-schedule", cx);
                view.update(cx, |view, cx| {
                    assert_eq!(
                        view.schedule_prompt.read(cx).value().as_ref(),
                        "Review the local notes",
                        "Queue admission is not durable task creation"
                    );
                });
            })
            .unwrap();
            wait_for(cx, &view, "durable scheduled task", |view, cx| {
                view.schedule_pending.is_none()
                    && view.schedule_prompt.read(cx).value().is_empty()
                    && view
                        .schedules
                        .iter()
                        .any(|task| task.prompt == "Review the local notes")
            });
            client.shutdown().unwrap();
        }
        #[gpui_kit::test]
        fn schedule_receipts_preserve_newer_input_and_ignore_unrelated_completions(
            cx: &mut TestAppContext,
        ) {
            let (handle, view) = open_view(cx, Err(anyhow::anyhow!("No host required")));
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    let id = Uuid::new_v4();
                    view.schedule_pending = Some((id, Some("Submitted draft".into())));
                    view.schedule_prompt
                        .update(cx, |input, cx| input.set_value("Newer draft", window, cx));
                    view.event(
                        HostEvent::ScheduleMutation {
                            request_id: Uuid::new_v4(),
                            result: Ok(()),
                        },
                        window,
                        cx,
                    );
                    assert!(view.schedule_pending.is_some());
                    view.event(
                        HostEvent::ScheduleMutation {
                            request_id: id,
                            result: Ok(()),
                        },
                        window,
                        cx,
                    );
                    assert!(view.schedule_pending.is_none());
                    assert_eq!(
                        view.schedule_prompt.read(cx).value().as_ref(),
                        "Newer draft"
                    );
                    let id = Uuid::new_v4();
                    view.schedule_pending = Some((id, Some("Newer draft".into())));
                    view.event(
                        HostEvent::ScheduleMutation {
                            request_id: id,
                            result: Err("Disk unavailable".into()),
                        },
                        window,
                        cx,
                    );
                    assert_eq!(
                        view.schedule_prompt.read(cx).value().as_ref(),
                        "Newer draft"
                    );
                    assert!(view.schedule_notice.contains("Disk unavailable"));
                    let id = Uuid::new_v4();
                    view.schedule_pending = Some((id, Some("Newer draft".into())));
                    view.event(
                        HostEvent::ScheduleMutation {
                            request_id: id,
                            result: Ok(()),
                        },
                        window,
                        cx,
                    );
                    assert!(view.schedule_prompt.read(cx).value().is_empty());
                });
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn task_error_stays_in_its_chat_instead_of_repeating_as_global_banner(
            cx: &mut TestAppContext,
        ) {
            let (handle, view) = open_view(cx, Err(anyhow::anyhow!("No live provider")));
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.error = None;
                    let session = view.session;
                    let id = Uuid::new_v4();
                    view.event(
                        HostEvent::Engine(xylo_core::Envelope::new(
                            id,
                            Some(session),
                            1,
                            xylo_core::EngineEvent::Started,
                        )),
                        window,
                        cx,
                    );
                    view.event(
                        HostEvent::Engine(xylo_core::Envelope::new(
                            id,
                            Some(session),
                            2,
                            xylo_core::EngineEvent::Error {
                                message: "Provider unavailable".into(),
                                retryable: true,
                            },
                        )),
                        window,
                        cx,
                    );
                    assert!(view.error.is_none());
                    assert_eq!(
                        view.turns
                            .get(&session)
                            .and_then(|turn| turn.error.as_deref()),
                        Some("Provider unavailable")
                    );
                });
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn downloads_enqueue_failure_exposes_retry_without_eternal_loading(
            cx: &mut TestAppContext,
        ) {
            let (handle, view) = open_view(cx, Err(anyhow::anyhow!("No host required")));
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.error = None;
                    view.page = Page::Assets;
                    view.refresh_assets(cx);
                });
                window.render_frame(cx);
                assert!(window.try_find("retry-assets").is_some());
                assert!(view.read(cx).error.is_none());
                window.click("retry-assets", cx);
                window.render_frame(cx);
                assert!(window.try_find("retry-assets").is_some());
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn downloads_retry_dispatches_and_settles_real_helper_catalog(cx: &mut TestAppContext) {
            let (_directory, _workspace, client, handle, view) = host_fixture(cx);
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.page = Page::Assets;
                    view.error = None;
                    assert!(view.asset_error("helper_asset_catalog", "Fixture failure", cx));
                });
                window.render_frame(cx);
                assert!(window.try_find("retry-assets").is_some());
                window.click("retry-assets", cx);
                assert!(
                    view.read(cx)
                        .native
                        .pending_id("Loading available features")
                        .is_some()
                );
            })
            .unwrap();
            wait_for(cx, &view, "retried helper catalog", |view, _| {
                view.native
                    .pending_id("Loading available features")
                    .is_none()
            });
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                assert!(
                    window.try_find("retry-assets").is_none(),
                    "Real helper catalog must settle successfully"
                );
                assert!(view.read(cx).error.is_none());
            })
            .unwrap();
            client.shutdown().unwrap();
        }
    }
}
