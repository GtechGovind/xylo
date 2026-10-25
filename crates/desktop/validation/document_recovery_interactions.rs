//! Missing-original recovery through real native controls, Host, Store and helper.
#![allow(dead_code, unused_imports)]
extern crate gpui_kit as gpui;
mod document_recovery_fixture;
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
    mod missing_tests {
        use super::{
            AnyWindowHandle, App, AppContext, Bounds, DocumentKind, Entity, HostClient,
            TestAppContext, Uuid, WindowBounds, WindowOptions, Workbench, px, size,
        };
        use crate::document_recovery_fixture::{self, Delivery, Profile};
        use gpui_kit::test::{TestQueryExt, TestWindowExt};
        use std::{
            sync::{Arc, atomic::Ordering},
            time::{Duration, Instant},
        };
        struct Fixture {
            profile: Profile,
            delivery: Arc<Delivery>,
            host: HostClient,
            window: AnyWindowHandle,
            view: Entity<Workbench>,
        }
        fn wait(
            cx: &mut TestAppContext,
            f: &Fixture,
            condition: impl Fn(&Workbench, &App) -> bool,
        ) {
            let deadline = Instant::now() + Duration::from_secs(25);
            loop {
                cx.executor().advance_clock(Duration::from_millis(25));
                cx.run_until_parked();
                if cx.update(|cx| condition(f.view.read(cx), cx)) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "Missing document recovery timed out: {}",
                    cx.update(|cx| f.view.read(cx).documents_validation_notice().to_string())
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fn open(cx: &mut TestAppContext, kind: DocumentKind) -> Fixture {
            cx.executor().allow_parking();
            let profile = document_recovery_fixture::seeded(kind);
            let delivery = Arc::new(Delivery::default());
            let host = document_recovery_fixture::host(&profile, delivery.clone());
            let client = host.0.clone();
            cx.update(gpui_kit::init);
            let (window, view) = cx.update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Default::default(),
                            size: size(px(1400.), px(1200.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| Workbench::new(Ok(host), window, cx)),
                )
                .unwrap()
            });
            let f = Fixture {
                profile,
                delivery,
                host: client,
                window,
                view,
            };
            wait(cx, &f, |view, _| {
                view.settings_loaded && view.workspace_ready
            });
            cx.update_window(window, |_, _, cx| {
                f.view
                    .update(cx, |view, cx| view.navigate(super::Page::Artifacts, cx))
            })
            .unwrap();
            wait(cx, &f, |view, _| view.documents_validation_recent() == 1);
            click(cx, &f, ("document-resume", 0usize));
            wait(cx, &f, |view, _| {
                view.documents_validation_missing() && view.documents_validation_saved()
            });
            f
        }
        fn click(cx: &mut TestAppContext, f: &Fixture, id: impl Into<gpui_kit::ElementId>) {
            cx.update_window(f.window, |_, window, cx| {
                window.render_frame(cx);
                window.click(id, cx);
            })
            .unwrap();
            cx.run_until_parked();
        }
        fn locate(cx: &mut TestAppContext, f: &Fixture, path: std::path::PathBuf) {
            cx.update_window(f.window, |_, _, cx| {
                f.view
                    .update(cx, |view, cx| view.document_locate_original(path, cx))
            })
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(25);
            while !cx.has_pending_prompt() {
                cx.run_until_parked();
                cx.executor().advance_clock(Duration::from_millis(25));
                assert!(
                    Instant::now() < deadline,
                    "No native original-location confirmation"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fn shutdown(cx: &mut TestAppContext, f: &Fixture) {
            f.host.shutdown().unwrap();
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(f.profile.paths.data.join("host.lock"))
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
        fn missing_source_keeps_all_changes_inspectable_and_copy_status_independent(
            cx: &mut TestAppContext,
        ) {
            let f = open(cx, DocumentKind::Spreadsheet);
            assert_eq!(cx.update(|cx| f.view.read(cx).documents.edits.len()), 2);
            click(cx, &f, ("document-recovered-change", 1usize));
            assert!(cx.update(|cx| {
                f.view
                    .read(cx)
                    .documents
                    .text
                    .read(cx)
                    .value()
                    .contains("Cell: B1\n\n=2+3")
            }));
            click(cx, &f, "document-copy-recovered");
            assert!(cx.update(|cx| {
                cx.read_from_clipboard()
                    .unwrap()
                    .text()
                    .unwrap()
                    .contains("Worksheet: Data\nCell: B1\n\n=2+3")
            }));
            assert!(
                cx.update_window(f.window, |_, window, cx| {
                    window.render_frame(cx);
                    window.find("document-find-original").visible()
                })
                .unwrap()
            );
            assert!(
                f.profile
                    .store
                    .document_draft(f.profile.key.clone())
                    .unwrap()
                    .state
                    .is_some()
            );
            assert!(!f.profile.original.exists());
            shutdown(cx, &f);
        }
        #[gpui_kit::test]
        fn wrong_original_is_rejected_and_moved_docx_relinks_then_preserves_package(
            cx: &mut TestAppContext,
        ) {
            let f = open(cx, DocumentKind::Docx);
            let wrong = f.profile.workspace.join("Wrong.docx");
            std::fs::write(&wrong, b"Different Office file").unwrap();
            cx.update_window(f.window, |_, _, cx| {
                f.view
                    .update(cx, |view, cx| view.document_locate_original(wrong, cx))
            })
            .unwrap();
            wait(cx, &f, |view, _| view.documents_validation_no_reads());
            assert!(!cx.has_pending_prompt());
            assert_eq!(
                f.profile
                    .store
                    .document_draft(f.profile.key.clone())
                    .unwrap()
                    .state
                    .unwrap()
                    .edits,
                f.profile.state.edits
            );
            let original_bytes = std::fs::read(&f.profile.moved).unwrap();
            locate(cx, &f, f.profile.moved.clone());
            cx.simulate_prompt_answer("Use this original");
            wait(cx, &f, |view, _| {
                view.documents_validation_ready()
                    && view
                        .documents
                        .source
                        .as_ref()
                        .is_some_and(|source| source.path == f.profile.moved)
            });
            assert!(
                f.profile
                    .store
                    .document_draft(f.profile.key.clone())
                    .unwrap()
                    .state
                    .is_none()
            );
            cx.update_window(f.window, |_, _, cx| {
                f.view
                    .update(cx, |view, cx| view.documents_validation_save(cx))
            })
            .unwrap();
            wait(cx, &f, |view, _| {
                view.documents_validation_output().is_some()
            });
            let output = cx.update(|cx| f.view.read(cx).documents_validation_output().unwrap());
            let mut zip = zip::ZipArchive::new(std::fs::File::open(output).unwrap()).unwrap();
            use std::io::Read;
            let mut document = String::new();
            zip.by_name("word/document.xml")
                .unwrap()
                .read_to_string(&mut document)
                .unwrap();
            assert!(document.contains("Recovered paragraph α"));
            let mut keep = String::new();
            zip.by_name("custom/keep.xml")
                .unwrap()
                .read_to_string(&mut keep)
                .unwrap();
            assert_eq!(keep, "<keep>Unchanged objects and metadata</keep>");
            assert_eq!(std::fs::read(&f.profile.moved).unwrap(), original_bytes);
            shutdown(cx, &f);
        }
        #[gpui_kit::test]
        fn moved_workbook_lost_relink_ack_checks_same_request_after_restart(
            cx: &mut TestAppContext,
        ) {
            let mut f = open(cx, DocumentKind::Spreadsheet);
            f.delivery.drop_relink.store(true, Ordering::SeqCst);
            locate(cx, &f, f.profile.moved.clone());
            cx.simulate_prompt_answer("Use this original");
            wait(cx, &f, |_, _| f.delivery.dropped.load(Ordering::SeqCst));
            let request = cx.update(|cx| f.view.read(cx).documents_validation_relink().unwrap());
            cx.executor().advance_clock(Duration::from_secs(31));
            cx.run_until_parked();
            click(cx, &f, "document-check-location");
            wait(cx, &f, |view, _| view.documents_validation_ready());
            shutdown(cx, &f);
            f.delivery = Arc::new(Delivery::default());
            let host = document_recovery_fixture::host(&f.profile, f.delivery.clone());
            f.host = host.0.clone();
            cx.update_window(f.window, |_, window, cx| {
                f.view
                    .update(cx, |view, cx| *view = Workbench::new(Ok(host), window, cx))
            })
            .unwrap();
            wait(cx, &f, |view, _| {
                view.settings_loaded && view.workspace_ready
            });
            cx.update_window(f.window, |_, _, cx| {
                f.view
                    .update(cx, |view, cx| view.navigate(super::Page::Artifacts, cx))
            })
            .unwrap();
            wait(cx, &f, |view, _| view.documents_validation_recent() == 1);
            let receipt = f
                .profile
                .store
                .document_draft_relink_status(f.profile.key.clone(), request.operation_id)
                .unwrap()
                .unwrap();
            assert_eq!(receipt.request, request);
            click(cx, &f, ("document-resume", 0usize));
            wait(cx, &f, |view, _| view.documents_validation_ready());
            assert_eq!(
                cx.update(|cx| f.view.read(cx).documents.edits.clone()),
                f.profile.state.edits
            );
            cx.update_window(f.window, |_, _, cx| {
                f.view
                    .update(cx, |view, cx| view.documents_validation_save(cx))
            })
            .unwrap();
            wait(cx, &f, |view, _| {
                view.documents_validation_output().is_some()
            });
            let output = cx.update(|cx| f.view.read(cx).documents_validation_output().unwrap());
            let mut zip = zip::ZipArchive::new(std::fs::File::open(output).unwrap()).unwrap();
            use std::io::Read;
            let mut sheet = String::new();
            zip.by_name("xl/worksheets/sheet1.xml")
                .unwrap()
                .read_to_string(&mut sheet)
                .unwrap();
            assert!(sheet.contains("Recovered cell α"));
            assert!(sheet.contains("2+3"));
            let mut other = String::new();
            zip.by_name("xl/worksheets/sheet2.xml")
                .unwrap()
                .read_to_string(&mut other)
                .unwrap();
            assert!(other.contains("Preserve this other sheet"));
            shutdown(cx, &f);
        }

        #[gpui_kit::test]
        fn committed_copy_is_checked_when_original_has_disappeared(cx: &mut TestAppContext) {
            let f = open(cx, DocumentKind::Text);
            std::fs::copy(&f.profile.moved, &f.profile.original).unwrap();
            cx.update_window(f.window, |_, window, cx| {
                f.view.update(cx, |view, cx| {
                    view.document_resume_saved(f.profile.original.clone(), window, cx)
                })
            })
            .unwrap();
            wait(cx, &f, |view, _| view.documents_validation_ready());
            let operation = cx
                .update_window(f.window, |_, _, cx| {
                    f.view.update(cx, |view, cx| {
                        view.documents_validation_save(cx);
                        view.documents_validation_pending().unwrap()
                    })
                })
                .unwrap();
            wait(cx, &f, |view, _| {
                view.documents_validation_output().is_some() && view.documents_validation_saved()
            });
            let snapshot = f
                .profile
                .store
                .document_draft(f.profile.key.clone())
                .unwrap();
            let mut state = snapshot.state.unwrap();
            state.pending_copy = Some(workbench_runtime::DocumentCopyIntent {
                operation_id: operation,
                generation: 1,
                name: state.export_name.clone(),
                edits: state.edits.clone(),
            });
            f.profile
                .store
                .save_document_draft(workbench_runtime::DocumentDraftWrite {
                    key: f.profile.key.clone(),
                    expected_revision: snapshot.revision,
                    mutation_id: Uuid::new_v4(),
                    state: Some(state),
                })
                .unwrap();
            std::fs::remove_file(&f.profile.original).unwrap();
            cx.update_window(f.window, |_, window, cx| {
                f.view.update(cx, |view, cx| {
                    view.documents = super::documents::DocumentUi::new(window, cx);
                    view.subscribe_documents(window, cx);
                    view.document_resume_saved(f.profile.original.clone(), window, cx);
                })
            })
            .unwrap();
            wait(cx, &f, |view, _| {
                view.documents_validation_missing()
                    && view.documents_validation_pending() == Some(operation)
            });
            click(cx, &f, "document-check-copy");
            wait(cx, &f, |view, _| {
                view.documents_validation_pending().is_none()
                    && view.documents_validation_output().is_some()
            });
            let output = cx.update(|cx| f.view.read(cx).documents_validation_output().unwrap());
            assert_eq!(
                std::fs::read_to_string(output).unwrap(),
                "Recovered full document α"
            );
            assert!(!f.profile.original.exists());
            assert_eq!(
                cx.update(|cx| f.view.read(cx).documents.edits.clone()),
                f.profile.state.edits
            );
            shutdown(cx, &f);
        }

        #[gpui_kit::test]
        fn cached_empty_destination_reloads_the_moved_proposal(cx: &mut TestAppContext) {
            let f = open(cx, DocumentKind::Text);
            cx.update_window(f.window, |_, window, cx| {
                f.view.update(cx, |view, cx| {
                    view.document_open(f.profile.moved.clone(), window, cx)
                })
            })
            .unwrap();
            wait(cx, &f, |view, _| view.documents_validation_ready());
            cx.update_window(f.window, |_, _, cx| {
                f.view.update(cx, |view, cx| {
                    view.document_before_workspace_switch(cx);
                })
            })
            .unwrap();
            wait(cx, &f, |view, _| view.documents_validation_saved());
            let destination = workbench_runtime::DocumentDraftKey {
                workspace: f.profile.workspace.clone(),
                source: f.profile.moved.clone(),
            };
            let snapshot = f.profile.store.document_draft(destination.clone()).unwrap();
            f.profile
                .store
                .save_document_draft(workbench_runtime::DocumentDraftWrite {
                    key: destination,
                    expected_revision: snapshot.revision,
                    mutation_id: Uuid::new_v4(),
                    state: None,
                })
                .unwrap();
            // The GUI still has its earlier clean destination cache. A successful
            // relink must reload it rather than project stale empty state.
            cx.update_window(f.window, |_, window, cx| {
                f.view.update(cx, |view, cx| {
                    view.document_resume_saved(f.profile.original.clone(), window, cx)
                })
            })
            .unwrap();
            wait(cx, &f, |view, _| view.documents_validation_missing());
            locate(cx, &f, f.profile.moved.clone());
            cx.simulate_prompt_answer("Use this original");
            wait(cx, &f, |view, _| view.documents_validation_ready());
            assert_eq!(
                cx.update(|cx| f.view.read(cx).documents.edits.clone()),
                f.profile.state.edits
            );
            assert_eq!(
                cx.update(|cx| f.view.read(cx).documents.text.read(cx).value().to_string()),
                "Recovered full document α"
            );
            shutdown(cx, &f);
        }
        #[gpui_kit::test]
        fn keep_previous_location_after_lost_commit_ack_preserves_moved_record(
            cx: &mut TestAppContext,
        ) {
            let f = open(cx, DocumentKind::Text);
            f.delivery.drop_relink.store(true, Ordering::SeqCst);
            locate(cx, &f, f.profile.moved.clone());
            cx.simulate_prompt_answer("Use this original");
            wait(cx, &f, |_, _| f.delivery.dropped.load(Ordering::SeqCst));
            cx.executor().advance_clock(Duration::from_secs(31));
            cx.run_until_parked();
            click(cx, &f, "document-keep-location");
            assert!(cx.has_pending_prompt());
            cx.simulate_prompt_answer("Keep previous location");
            wait(cx, &f, |view, _| view.documents_validation_conflict());
            let destination = workbench_runtime::DocumentDraftKey {
                workspace: f.profile.workspace.clone(),
                source: f.profile.moved.clone(),
            };
            assert_eq!(
                f.profile
                    .store
                    .document_draft(destination)
                    .unwrap()
                    .state
                    .unwrap()
                    .edits,
                f.profile.state.edits
            );
            click(cx, &f, "document-recovery-saved");
            wait(cx, &f, |view, _| {
                view.documents.source.is_none() && view.documents_validation_recent() == 1
            });
            click(cx, &f, ("document-resume", 0usize));
            wait(cx, &f, |view, _| view.documents_validation_ready());
            assert_eq!(
                cx.update(|cx| f.view.read(cx).documents.edits.clone()),
                f.profile.state.edits
            );
            shutdown(cx, &f);
        }
    }
}
