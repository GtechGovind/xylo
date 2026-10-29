//! Expandable project-tree controls through the production Host/index. All files
//! and editor proposals are temporary; no external service or model is used.
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
    mod tree_tests {
        use super::{Page, Xylo, file_browser::RowKey};
        use gpui_kit::{
            AnyWindowHandle, App, AppContext, Bounds, Entity, TestAppContext, WindowBounds,
            WindowOptions, px, size,
            test::{TestQueryExt, TestWindowExt},
        };
        use std::{
            path::{Path, PathBuf},
            time::{Duration, Instant},
        };
        use xylo_runtime::{
            AppPaths,
            host::{HostClient, HostEvent},
            workspace::{FileEntry, FilePage},
        };

        struct Fixture {
            _temporary: tempfile::TempDir,
            root: PathBuf,
            host: HostClient,
            window: AnyWindowHandle,
            view: Entity<Xylo>,
        }
        fn wait(
            cx: &mut TestAppContext,
            f: &Fixture,
            description: &str,
            condition: impl Fn(&Xylo, &App) -> bool,
        ) {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                // Drive the native receiver's bounded-backlog yield timer.
                cx.executor().advance_clock(std::time::Duration::from_millis(2));
                cx.run_until_parked();
                if cx.update(|cx| condition(f.view.read(cx), cx)) {
                    return;
                }
                assert!(Instant::now() < deadline, "Timed out: {description}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fn open(cx: &mut TestAppContext, create: impl FnOnce(&Path)) -> Fixture {
            cx.executor().allow_parking();
            cx.update(gpui_kit::init);
            let temporary = tempfile::tempdir().unwrap();
            let base = temporary.path().canonicalize().unwrap();
            let root = base.join("project");
            std::fs::create_dir(&root).unwrap();
            create(&root);
            let host = HostClient::spawn_at(AppPaths::at(base.join("profile")).unwrap()).unwrap();
            let client = host.0.clone();
            let (window, view) = cx.update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            Default::default(),
                            size(px(1280.), px(820.)),
                        ))),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| Xylo::new(Ok(host), window, cx)),
                )
                .unwrap()
            });
            let f = Fixture {
                _temporary: temporary,
                root,
                host: client,
                window,
                view,
            };
            wait(cx, &f, "settings", |view, _| view.settings_loaded);
            cx.update_window(window, |_, window, cx| {
                f.view.update(cx, |view, cx| {
                    view.set_workspace(f.root.clone(), window, cx)
                })
            })
            .unwrap();
            wait(cx, &f, "initial root page", |view, _| {
                view.workspace_ready
                    && !view.file_browser.indexing
                    && !view.file_browser.loading()
                    && !view.file_browser.entries.is_empty()
            });
            cx.update_window(window, |_, window, cx| {
                f.view.update(cx, |view, cx| {
                    view.page = Page::Files;
                    cx.notify();
                });
                window.activate_window();
                window.render_frame(cx);
            })
            .unwrap();
            cx.run_until_parked();
            f
        }
        fn click(cx: &mut TestAppContext, f: &Fixture, key: RowKey) {
            cx.update_window(f.window, |_, window, cx| {
                f.view.update(cx, |view, cx| {
                    let index = view
                        .file_browser
                        .tree
                        .rows
                        .iter()
                        .position(|row| row.key.as_ref() == Some(&key))
                        .expect("visible tree action");
                    view.file_scroll
                        .scroll_to_item(index, gpui_kit::ScrollStrategy::Top);
                    cx.notify();
                });
                window.render_frame(cx);
                let index = f
                    .view
                    .read(cx)
                    .file_browser
                    .tree
                    .rows
                    .iter()
                    .position(|row| row.key.as_ref() == Some(&key))
                    .unwrap();
                window.click(("file", index), cx);
            })
            .unwrap();
        }
        fn stop(cx: &mut TestAppContext, f: &Fixture) {
            f.host.shutdown().unwrap();
            cx.run_until_parked();
        }

        #[gpui_kit::test]
        fn lazy_nested_expansion_and_native_keyboard_preserve_the_editor(cx: &mut TestAppContext) {
            let f = open(cx, |root| {
                std::fs::create_dir(root.join("empty")).unwrap();
                std::fs::create_dir_all(root.join("z-source/nested")).unwrap();
                std::fs::write(root.join("z-source/nested/code.rs"), "fn main() {}\n").unwrap();
                std::fs::write(root.join("a.txt"), "Original\n").unwrap();
            });
            let folder = f.root.join("z-source");
            let nested = folder.join("nested");
            let code = nested.join("code.rs");
            cx.update(|cx| {
                let tree = &f.view.read(cx).file_browser.tree;
                assert!(
                    !tree.directories.contains_key(&folder),
                    "Collapsed folders are not eagerly requested"
                );
                let folder_index = tree.rows.iter().position(|row| row.path == folder).unwrap();
                let file_index = tree
                    .rows
                    .iter()
                    .position(|row| row.path == f.root.join("a.txt"))
                    .unwrap();
                assert!(
                    folder_index < file_index,
                    "Folders sort before alphabetically earlier files"
                );
            });
            let empty = f.root.join("empty");
            click(cx, &f, RowKey::Entry(empty.clone()));
            wait(cx, &f, "empty folder expansion", |view, _| {
                !view.file_browser.loading()
                    && view
                        .file_browser
                        .tree
                        .directories
                        .get(&empty)
                        .is_some_and(|node| node.loaded)
            });
            cx.update_window(f.window, |_, window, cx| {
                window.render_frame(cx);
                window.press("right", cx);
                assert_eq!(
                    f.view.read(cx).file_browser.tree.selected.as_ref(),
                    Some(&RowKey::Entry(empty.clone())),
                    "Right on an expanded empty folder must not select a sibling"
                );
            })
            .unwrap();
            click(cx, &f, RowKey::Entry(folder.clone()));
            wait(cx, &f, "first expansion", |view, _| {
                !view.file_browser.loading()
                    && view
                        .file_browser
                        .entries
                        .iter()
                        .any(|entry| entry.path == nested)
            });
            // Right enters the existing first child, then expands it; Down and
            // Enter open the file using the same production keyboard listener.
            cx.update_window(f.window, |_, window, cx| {
                window.render_frame(cx);
                window.press("right", cx);
                window.press("right", cx);
            })
            .unwrap();
            wait(cx, &f, "keyboard expansion", |view, _| {
                !view.file_browser.loading()
                    && view
                        .file_browser
                        .entries
                        .iter()
                        .any(|entry| entry.path == code)
            });
            cx.update_window(f.window, |_, window, cx| {
                window.render_frame(cx);
                window.press("down", cx);
                window.press("left", cx);
                assert_eq!(
                    f.view.read(cx).file_browser.tree.selected.as_ref(),
                    Some(&RowKey::Entry(nested.clone()))
                );
                window.press("right", cx);
                window.press("up", cx);
                window.press("down", cx);
                window.press("enter", cx);
            })
            .unwrap();
            wait(cx, &f, "keyboard opened file", |view, _| {
                view.editor_path.as_ref() == Some(&code)
            });
            cx.update_window(f.window, |_, window, cx| {
                f.view.update(cx, |view, cx| {
                    view.editor.update(cx, |editor, cx| {
                        editor.set_value("Unsaved Unicode 🦀 edits", window, cx)
                    });
                    view.editor_dirty = true;
                });
            })
            .unwrap();
            click(cx, &f, RowKey::Entry(folder.clone()));
            cx.update(|cx| {
                let view = f.view.read(cx);
                assert!(view.editor_dirty);
                assert_eq!(
                    view.editor.read(cx).value().as_ref(),
                    "Unsaved Unicode 🦀 edits"
                );
                assert!(
                    !view
                        .file_browser
                        .entries
                        .iter()
                        .any(|entry| entry.path == code)
                );
            });
            click(cx, &f, RowKey::Entry(folder));
            wait(cx, &f, "nested expansion restored", |view, _| {
                view.file_browser
                    .entries
                    .iter()
                    .any(|entry| entry.path == code)
            });
            click(cx, &f, RowKey::Entry(f.root.join("a.txt")));
            cx.update(|cx| {
                let view = f.view.read(cx);
                assert_eq!(view.editor_path.as_ref(), Some(&code));
                assert_eq!(
                    view.editor.read(cx).value().as_ref(),
                    "Unsaved Unicode 🦀 edits"
                );
            });
            assert_eq!(std::fs::read_to_string(&code).unwrap(), "fn main() {}\n");
            stop(cx, &f);
        }

        #[gpui_kit::test]
        fn collapsed_and_wrong_scope_replies_cannot_repopulate_the_tree(cx: &mut TestAppContext) {
            let f = open(cx, |root| {
                std::fs::create_dir(root.join("folder")).unwrap();
                std::fs::write(root.join("folder/real.txt"), "Real").unwrap();
            });
            let folder = f.root.join("folder");
            let fake = folder.join("injected.txt");
            cx.update_window(f.window, |_, window, cx| {
                f.view.update(cx, |view, cx| {
                    view.toggle_tree_folder(folder.clone(), cx);
                    let stale = view.file_browser.validation_request_for(&folder).unwrap();
                    view.toggle_tree_folder(folder.clone(), cx);
                    assert!(view.file_browser.validation_request_for(&folder).is_none());
                    view.event(
                        HostEvent::WorkspaceFiles {
                            request_id: stale,
                            result: Ok(FilePage {
                                root: f.root.clone(),
                                directory: folder.clone(),
                                revision: 999,
                                entries: vec![FileEntry {
                                    path: fake.clone(),
                                    is_dir: false,
                                    bytes: 0,
                                }],
                                next: None,
                                indexing: false,
                                file_count: 1,
                                directory_count: 1,
                                unreadable: 0,
                            }),
                        },
                        window,
                        cx,
                    );
                    assert!(
                        !view
                            .file_browser
                            .entries
                            .iter()
                            .any(|entry| entry.path == fake)
                    );
                    view.toggle_tree_folder(folder.clone(), cx);
                    let current = view.file_browser.validation_request_for(&folder).unwrap();
                    assert_ne!(stale, current);
                    view.event(
                        HostEvent::WorkspaceFiles {
                            request_id: current,
                            result: Ok(FilePage {
                                root: f.root.join("another-project"),
                                directory: folder.clone(),
                                revision: 1000,
                                entries: vec![FileEntry {
                                    path: fake.clone(),
                                    is_dir: false,
                                    bytes: 0,
                                }],
                                next: None,
                                indexing: false,
                                file_count: 1,
                                directory_count: 1,
                                unreadable: 0,
                            }),
                        },
                        window,
                        cx,
                    );
                    assert!(
                        !view
                            .file_browser
                            .entries
                            .iter()
                            .any(|entry| entry.path == fake)
                    );
                });
            })
            .unwrap();
            click(cx, &f, RowKey::Load(folder.clone()));
            wait(cx, &f, "real folder after malformed reply", |view, _| {
                !view.file_browser.loading()
                    && view
                        .file_browser
                        .entries
                        .iter()
                        .any(|entry| entry.path == folder.join("real.txt"))
            });
            cx.update_window(f.window, |_, window, cx| {
                f.view.update(cx, |view, cx| {
                    view.load_file_page(None, cx);
                    let old = view.file_browser.validation_request_for(&f.root).unwrap();
                    view.reset_file_browser();
                    view.event(
                        HostEvent::WorkspaceFiles {
                            request_id: old,
                            result: Err("Old project reply".into()),
                        },
                        window,
                        cx,
                    );
                    assert!(view.file_browser.tree.rows.is_empty());
                    assert!(view.file_browser.notice.is_empty());
                });
            })
            .unwrap();
            stop(cx, &f);
        }

        #[gpui_kit::test]
        fn large_folder_inline_windows_reach_all_paths_with_a_bounded_cache(
            cx: &mut TestAppContext,
        ) {
            let f = open(cx, |root| {
                for index in 0..2105 {
                    std::fs::write(root.join(format!("item-{index:04}.txt")), "Text").unwrap();
                }
            });
            cx.update_window(f.window, |_, window, cx| {
                let focus = f.view.read(cx).file_browser.focus.clone().unwrap();
                window.focus(&focus, cx);
                window.render_frame(cx);
                let before = f.view.read(cx).file_scroll.0.borrow().base_handle.offset();
                window.press("down", cx);
                window.press("down", cx);
                window.render_frame(cx);
                let after = f.view.read(cx).file_scroll.0.borrow().base_handle.offset();
                assert_eq!(
                    before, after,
                    "Arrow moves within the viewport must keep its position"
                );
            })
            .unwrap();
            // A matching request ID does not authorize a page from a different
            // catalogue revision to append into the retained directory window.
            let injected = f.root.join("injected.txt");
            cx.update_window(f.window, |_, window, cx| {
                f.view.update(cx, |view, cx| {
                    view.activate_tree_row(RowKey::More(f.root.clone()), window, cx);
                    let request_id = view.file_browser.validation_request_for(&f.root).unwrap();
                    view.event(
                        HostEvent::WorkspaceFiles {
                            request_id,
                            result: Ok(FilePage {
                                root: f.root.clone(),
                                directory: f.root.clone(),
                                revision: u64::MAX,
                                entries: vec![FileEntry {
                                    path: injected.clone(),
                                    is_dir: false,
                                    bytes: 0,
                                }],
                                next: None,
                                indexing: false,
                                file_count: 2105,
                                directory_count: 0,
                                unreadable: 0,
                            }),
                        },
                        window,
                        cx,
                    );
                    assert_eq!(view.file_browser.entries.len(), 500);
                    assert!(
                        !view
                            .file_browser
                            .entries
                            .iter()
                            .any(|entry| entry.path == injected)
                    );
                });
            })
            .unwrap();
            let mut seen = std::collections::BTreeSet::new();
            loop {
                let (paths, next) = cx.update(|cx| {
                    let node = f
                        .view
                        .read(cx)
                        .file_browser
                        .tree
                        .directories
                        .get(&f.root)
                        .unwrap();
                    assert!(node.entries.len() <= 1000);
                    (
                        node.entries
                            .iter()
                            .map(|entry| entry.path.clone())
                            .collect::<Vec<_>>(),
                        node.next.as_ref().map(|cursor| cursor.path.clone()),
                    )
                });
                seen.extend(paths);
                if next.is_none() {
                    break;
                }
                click(cx, &f, RowKey::More(f.root.clone()));
                wait(cx, &f, "inline next window", |view, _| {
                    !view.file_browser.loading()
                        && view
                            .file_browser
                            .tree
                            .directories
                            .get(&f.root)
                            .unwrap()
                            .next
                            .as_ref()
                            .map(|cursor| &cursor.path)
                            != next.as_ref()
                });
            }
            assert_eq!(seen.len(), 2105);
            click(cx, &f, RowKey::Earlier(f.root.clone()));
            wait(cx, &f, "earlier items remain reachable", |view, _| {
                !view.file_browser.loading()
                    && view
                        .file_browser
                        .entries
                        .first()
                        .is_some_and(|entry| entry.path == f.root.join("item-0000.txt"))
            });
            cx.update_window(f.window, |_, window, cx| {
                window.render_frame(cx);
                assert!(window.try_find("files-up").is_none());
                assert!(window.try_find("files-first").is_none());
                assert!(window.try_find("files-next").is_none());
            })
            .unwrap();
            stop(cx, &f);
        }

        #[gpui_kit::test]
        fn watcher_refreshes_an_expanded_branch_without_collapsing_siblings(
            cx: &mut TestAppContext,
        ) {
            let f = open(cx, |root| {
                for folder in ["left", "right"] {
                    std::fs::create_dir(root.join(folder)).unwrap();
                    std::fs::write(root.join(folder).join("kept.txt"), "Kept").unwrap();
                }
            });
            let left = f.root.join("left");
            let right = f.root.join("right");
            click(cx, &f, RowKey::Entry(left.clone()));
            click(cx, &f, RowKey::Entry(right.clone()));
            wait(cx, &f, "both expanded branches", |view, _| {
                !view.file_browser.loading() && view.file_browser.entries.len() == 4
            });
            let added = left.join("new.txt");
            std::fs::write(&added, "Watched addition").unwrap();
            wait(cx, &f, "nested watcher addition", |view, _| {
                !view.file_browser.loading()
                    && view
                        .file_browser
                        .entries
                        .iter()
                        .any(|entry| entry.path == added)
            });
            std::fs::remove_file(left.join("kept.txt")).unwrap();
            wait(cx, &f, "nested watcher deletion", |view, _| {
                !view.file_browser.loading()
                    && !view
                        .file_browser
                        .entries
                        .iter()
                        .any(|entry| entry.path == left.join("kept.txt"))
            });
            cx.update(|cx| {
                let tree = &f.view.read(cx).file_browser.tree;
                assert!(tree.expanded.contains(&left));
                assert!(tree.expanded.contains(&right));
                assert!(
                    f.view
                        .read(cx)
                        .file_browser
                        .entries
                        .iter()
                        .any(|entry| entry.path == right.join("kept.txt"))
                );
                assert_eq!(tree.selected.as_ref(), Some(&RowKey::Entry(right)));
            });
            stop(cx, &f);
        }
    }
}
