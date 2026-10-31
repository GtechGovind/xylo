//! Native document editor against the actual host/helper and durable database.
#![allow(dead_code, unused_imports)]
extern crate gpui_kit as gpui;
#[path = "../src/icons.rs"]
mod icons;
#[path = "../src/motion.rs"]
mod motion;
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
    mod document_tests {
        use super::{
            App, AppContext, AppSettings, Bounds, Context, DocumentCellValue, DocumentContent,
            DocumentEdit, DocumentKind, Entity, HostClient, TestAppContext, Uuid, WindowBounds,
            WindowOptions, Xylo, px, size,
        };
        use gpui_kit::test::TestWindowExt;
        use std::time::{Duration, Instant};
        fn wait(
            cx: &mut TestAppContext,
            view: &Entity<Xylo>,
            condition: impl Fn(&Xylo, &App) -> bool,
        ) {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                cx.executor().advance_clock(Duration::from_millis(50));
                cx.run_until_parked();
                if cx.update(|cx| condition(view.read(cx), cx)) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "Timed out waiting for native document flow: {}",
                    cx.update(|cx| {
                        let view = view.read(cx);
                        format!(
                            "ready={},saved={},source={:?},notice={},error={:?}",
                            view.documents_validation_ready(),
                            view.documents_validation_saved(),
                            view.documents.source,
                            view.documents_validation_notice(),
                            view.error
                        )
                    })
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fn shutdown(cx: &mut TestAppContext, client: &HostClient, data: &std::path::Path) {
            client.shutdown().unwrap();
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(data.join("host.lock"))
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                cx.run_until_parked();
                if lock.try_lock().is_ok() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "Host did not finish native fixture shutdown"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        #[gpui_kit::test]
        fn imported_text_edits_recover_and_save_copy_without_changing_source(
            cx: &mut TestAppContext,
        ) {
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let base = directory.path().canonicalize().unwrap();
            let workspace = base.join("project");
            std::fs::create_dir(&workspace).unwrap();
            let source = workspace.join("notes.txt");
            std::fs::write(&source, "Original notes\n").unwrap();
            #[cfg(unix)]
            let imported_path = {
                let alias = workspace.join("alias.txt");
                std::os::unix::fs::symlink(&source, &alias).unwrap();
                alias
            };
            #[cfg(not(unix))]
            let imported_path = source.clone();
            let paths = xylo_runtime::AppPaths::at(base.join("profile")).unwrap();
            let store = xylo_services::Store::open(&paths.database).unwrap();
            store
                .save_settings(&AppSettings {
                    last_workspace: Some(workspace.clone()),
                    ..Default::default()
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
                            size: size(px(1400.), px(1100.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| Xylo::new(Ok(host), window, cx)),
                )
                .unwrap()
            });
            wait(cx, &view, |view, _| {
                view.settings_loaded && view.workspace_ready
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.document_open(imported_path.clone(), window, cx)
                })
            })
            .unwrap();
            wait(cx, &view, |view, _| view.documents_validation_ready());
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.documents
                        .text
                        .update(cx, |input, cx| input.focus(window, cx))
                });
                window.render_frame(cx);
                window.press("cmd-a", cx);
                window.input("Updated imported text", cx);
            })
            .unwrap();
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(400));
            wait(cx, &view, |view, _| view.documents_validation_saved());
            let key = xylo_runtime::DocumentDraftKey {
                workspace: workspace.clone(),
                source: source.clone(),
            };
            let saved = store.document_draft(key.clone()).unwrap().state.unwrap();
            assert!(
                matches!(&saved.edits[..],[DocumentEdit::Text{text}] if text=="Updated imported text")
            );
            assert_eq!(
                std::fs::read_to_string(&source).unwrap(),
                "Original notes\n"
            );
            let operation = cx
                .update_window(window, |_, window, cx| {
                    view.update(cx, |view, cx| {
                        view.documents.name.update(cx, |input, cx| {
                            input.set_value("edited-notes.txt", window, cx)
                        });
                        view.documents_validation_save(cx);
                        view.documents_validation_pending().unwrap()
                    })
                })
                .unwrap();
            wait(cx, &view, |view, _| {
                view.documents_validation_output().is_some()
            });
            let output = cx.update(|cx| view.read(cx).documents_validation_output().unwrap());
            assert_eq!(
                std::fs::read_to_string(&output).unwrap(),
                "Updated imported text"
            );
            assert_eq!(
                std::fs::read_to_string(&source).unwrap(),
                "Original notes\n"
            );
            wait(cx, &view, |view, _| view.documents_validation_saved());
            // Recreate only the native editor controller from the durable draft,
            // as a new process would. The actual helper receipt already exists.
            let stored = store.document_draft(key.clone()).unwrap();
            let mut recovered = stored.state.unwrap();
            recovered.pending_copy = Some(xylo_runtime::DocumentCopyIntent {
                operation_id: operation,
                generation: 1,
                name: "edited-notes.txt".into(),
                edits: recovered.edits.clone(),
            });
            assert!(matches!(
                store
                    .save_document_draft(xylo_runtime::DocumentDraftWrite {
                        key: key.clone(),
                        expected_revision: stored.revision,
                        mutation_id: Uuid::new_v4(),
                        state: Some(recovered)
                    })
                    .unwrap(),
                xylo_runtime::DocumentDraftSave::Saved(_)
            ));
            std::fs::write(&source, "Externally changed after copy").unwrap();
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.documents = super::documents::DocumentUi::new(window, cx);
                    view.subscribe_documents(window, cx);
                    view.document_open(source.clone(), window, cx);
                })
            })
            .unwrap();
            wait(cx, &view, |view, _| {
                view.documents_validation_pending() == Some(operation)
                    && view.documents_validation_conflicted()
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    assert!(view.document_before_workspace_switch(cx));
                    assert!(view.document_drafts_flush(super::DraftFlushAction::Update, cx));
                });
                window.render_frame(cx);
                window.click("document-check-copy", cx);
            })
            .unwrap();
            wait(cx, &view, |view, _| {
                view.documents_validation_pending().is_none()
                    && view.documents_validation_output().is_some()
            });
            assert_eq!(
                cx.update(|cx| view.read(cx).documents_validation_output().unwrap()),
                output
            );
            assert_eq!(
                std::fs::read_dir(&paths.artifacts)
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter(|entry| entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "txt"))
                    .count(),
                1,
                "Reconciliation must not create another copy"
            );
            shutdown(cx, &client, &paths.data);
        }

        fn workbook(path: &std::path::Path) {
            use std::io::Write;
            let file = std::fs::File::create(path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let parts = [
                (
                    "[Content_Types].xml",
                    r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/worksheets/sheet2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
                ),
                (
                    "_rels/.rels",
                    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="root" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
                ),
                (
                    "xl/workbook.xml",
                    r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="First" sheetId="1" r:id="one"/><sheet name="Second" sheetId="2" r:id="two"/></sheets></workbook>"#,
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="one" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="two" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#,
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:C70"/><sheetData><row r="1"><c r="A1"><v>7</v></c><c r="B1"><f>A1*2</f><v>14</v></c></row><row r="70"><c r="C70" t="inlineStr"><is><t>Far cell</t></is></c></row></sheetData></worksheet>"#,
                ),
                (
                    "xl/worksheets/sheet2.xml",
                    r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:B2"/><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Second sheet</t></is></c></row></sheetData><mergeCells count="1"><mergeCell ref="A1:B2"/></mergeCells></worksheet>"#,
                ),
            ];
            for (name, xml) in parts {
                zip.start_file(name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(xml.as_bytes()).unwrap();
            }
            zip.finish().unwrap();
        }
        #[gpui_kit::test]
        fn native_spreadsheet_pages_sheets_keyboard_typed_values_and_copy(cx: &mut TestAppContext) {
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let base = directory.path().canonicalize().unwrap();
            let workspace = base.join("project");
            std::fs::create_dir(&workspace).unwrap();
            let source = workspace.join("book.xlsx");
            workbook(&source);
            let original = std::fs::read(&source).unwrap();
            let paths = xylo_runtime::AppPaths::at(base.join("profile")).unwrap();
            let store = xylo_services::Store::open(&paths.database).unwrap();
            store
                .save_settings(&AppSettings {
                    last_workspace: Some(workspace.clone()),
                    ..Default::default()
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
                            size: size(px(1500.), px(1100.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| Xylo::new(Ok(host), window, cx)),
                )
                .unwrap()
            });
            wait(cx, &view, |view, _| {
                view.workspace_ready && view.settings_loaded
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.document_open(source.clone(), window, cx)
                })
            })
            .unwrap();
            wait(cx, &view, |view, cx| {
                view.documents_validation_ready()
                    && matches!(view.documents_validation_current_cell(cx).map(|cell|cell.value),Some(DocumentCellValue::Number{value}) if value=="7")
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.documents_validation_focus_grid(window, cx)
                });
                window.render_frame(cx);
                window.press("right", cx);
            })
            .unwrap();
            wait(cx, &view, |view, _| {
                view.documents_validation_selected() == Some((0, 1))
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.documents_validation_cell(69, 2, window, cx)
                })
            })
            .unwrap();
            wait(cx, &view, |view, cx| {
                view.documents_validation_current_cell(cx).is_some_and(
                    |cell| matches!(cell.value,DocumentCellValue::Text{text} if text=="Far cell"),
                )
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.documents_validation_sheet("Second".into(), window, cx)
                })
            })
            .unwrap();
            wait(cx, &view, |view, cx| {
                view.documents_validation_current_cell(cx)
                    .is_some_and(|cell| !cell.editable)
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.documents_validation_sheet("First".into(), window, cx)
                })
            })
            .unwrap();
            wait(cx, &view, |view, cx| {
                view.documents_validation_current_cell(cx)
                    .is_some_and(|cell| cell.editable)
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.documents
                        .value
                        .update(cx, |input, cx| input.focus(window, cx))
                });
                window.render_frame(cx);
                window.press("cmd-a", cx);
                window.input("42", cx);
            })
            .unwrap();
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(400));
            wait(cx, &view, |view, _| view.documents_validation_saved());
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.documents.name.update(cx, |input, cx| {
                        input.set_value("changed-book.xlsx", window, cx)
                    });
                    view.documents_validation_save(cx);
                });
            })
            .unwrap();
            wait(cx, &view, |view, _| {
                view.documents_validation_output().is_some() && view.documents_validation_saved()
            });
            let copy = cx.update(|cx| view.read(cx).documents_validation_output().unwrap());
            assert_eq!(std::fs::read(&source).unwrap(), original);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| view.document_open(copy.clone(), window, cx))
            })
            .unwrap();
            wait(cx, &view, |view, cx| {
                view.documents_validation_ready()
                    && matches!(view.documents_validation_current_cell(cx).map(|cell|cell.value),Some(DocumentCellValue::Number{value}) if value=="42")
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.documents_validation_cell(0, 1, window, cx)
                })
            })
            .unwrap();
            wait(cx, &view, |view, cx| {
                view.documents_validation_current_cell(cx).is_some_and(|cell|matches!(cell.value,DocumentCellValue::Formula{expression,..} if expression=="=A1*2"))
            });
            shutdown(cx, &client, &paths.data);
        }
        #[gpui_kit::test]
        fn document_typing_during_ack_survives_and_project_switch_waits(cx: &mut TestAppContext) {
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let base = directory.path().canonicalize().unwrap();
            let workspace = base.join("project");
            let other = base.join("other");
            std::fs::create_dir(&workspace).unwrap();
            std::fs::create_dir(&other).unwrap();
            let source = workspace.join("draft.txt");
            std::fs::write(&source, "Original").unwrap();
            let paths = xylo_runtime::AppPaths::at(base.join("profile")).unwrap();
            let store = xylo_services::Store::open(&paths.database).unwrap();
            store
                .save_settings(&AppSettings {
                    last_workspace: Some(workspace.clone()),
                    ..Default::default()
                })
                .unwrap();
            let host = HostClient::spawn_at(paths.clone()).unwrap();
            let client = host.0.clone();
            cx.update(gpui_kit::init);
            let (window, view) = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| Xylo::new(Ok(host), window, cx))
                })
                .unwrap()
            });
            wait(cx, &view, |view, _| {
                view.workspace_ready && view.settings_loaded
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.document_open(source.clone(), window, cx)
                })
            })
            .unwrap();
            wait(cx, &view, |view, _| {
                view.documents_validation_ready() && view.documents_validation_saved()
            });
            let lock = rusqlite::Connection::open(&paths.database).unwrap();
            lock.execute_batch("BEGIN IMMEDIATE").unwrap();
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.documents
                        .text
                        .update(cx, |input, cx| input.focus(window, cx))
                });
                window.render_frame(cx);
                window.press("cmd-a", cx);
                window.input("First edit", cx);
            })
            .unwrap();
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(400));
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.input(" plus newer typing", cx);
                view.update(cx, |view, cx| {
                    view.set_workspace(other.clone(), window, cx);
                    assert_eq!(
                        view.workspace.as_ref(),
                        Some(&workspace),
                        "Switch cannot discard unacknowledged recovery"
                    );
                    assert!(!view.document_drafts_flush(super::DraftFlushAction::Quit, cx));
                    assert!(view.document_drafts_flush_pending());
                });
                window.input(" after quit intent", cx);
            })
            .unwrap();
            cx.run_until_parked();
            assert!(
                cx.update(|cx| !view.read(cx).document_drafts_flush_pending()),
                "New typing cancels delayed Quit"
            );
            lock.execute_batch("COMMIT").unwrap();
            cx.executor().advance_clock(Duration::from_millis(400));
            wait(cx, &view, |view, _| view.documents_validation_saved());
            let draft = store
                .document_draft(xylo_runtime::DocumentDraftKey {
                    workspace: workspace.clone(),
                    source: source.clone(),
                })
                .unwrap()
                .state
                .unwrap();
            assert!(
                matches!(&draft.edits[..],[DocumentEdit::Text{text}] if text=="First edit plus newer typing after quit intent")
            );
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| view.set_workspace(other.clone(), window, cx))
            })
            .unwrap();
            wait(cx, &view, |view, _| {
                view.workspace_ready && view.workspace.as_ref() == Some(&other)
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.set_workspace(workspace.clone(), window, cx)
                })
            })
            .unwrap();
            wait(cx, &view, |view, _| {
                view.workspace_ready && view.workspace.as_ref() == Some(&workspace)
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.document_open(source.clone(), window, cx)
                })
            })
            .unwrap();
            wait(cx, &view, |view, cx| {
                view.documents_validation_ready()
                    && view.documents.text.read(cx).value().as_ref()
                        == "First edit plus newer typing after quit intent"
            });
            std::fs::write(&source, "Source replaced outside the app").unwrap();
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.document_open(source.clone(), window, cx)
                })
            })
            .unwrap();
            wait(cx, &view, |view, _| view.documents_validation_conflicted());
            assert!(!cx.update(|cx| view.read(cx).documents_validation_ready()));
            let preserved = store
                .document_draft(xylo_runtime::DocumentDraftKey {
                    workspace: workspace.clone(),
                    source: source.clone(),
                })
                .unwrap()
                .state
                .unwrap();
            assert!(
                matches!(&preserved.edits[..],[DocumentEdit::Text{text}] if text=="First edit plus newer typing after quit intent")
            );
            shutdown(cx, &client, &paths.data);
        }
        #[gpui_kit::test]
        fn native_new_word_and_spreadsheet_use_the_structured_editor(cx: &mut TestAppContext) {
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let base = directory.path().canonicalize().unwrap();
            let workspace = base.join("project");
            std::fs::create_dir(&workspace).unwrap();
            let paths = xylo_runtime::AppPaths::at(base.join("profile")).unwrap();
            let store = xylo_services::Store::open(&paths.database).unwrap();
            store
                .save_settings(&AppSettings {
                    last_workspace: Some(workspace),
                    ..Default::default()
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
                            size: size(px(1400.), px(1100.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| Xylo::new(Ok(host), window, cx)),
                )
                .unwrap()
            });
            wait(cx, &view, |view, _| {
                view.workspace_ready && view.settings_loaded
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| view.navigate(super::Page::Artifacts, cx));
                window.render_frame(cx);
                window.click("document-new-word", cx);
            })
            .unwrap();
            wait(cx, &view, |view, _| {
                view.documents_validation_ready()
                    && view
                        .documents
                        .source
                        .as_ref()
                        .is_some_and(|source| source.kind == DocumentKind::Docx)
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.documents
                        .text
                        .update(cx, |input, cx| input.focus(window, cx))
                });
                window.render_frame(cx);
                window.input("A new Word paragraph", cx);
            })
            .unwrap();
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(400));
            wait(cx, &view, |view, _| view.documents_validation_saved());
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.documents.name.update(cx, |input, cx| {
                        input.set_value("new-report.docx", window, cx)
                    });
                });
                window.render_frame(cx);
                window.press(
                    if cfg!(target_os = "macos") {
                        "cmd-s"
                    } else {
                        "ctrl-s"
                    },
                    cx,
                );
            })
            .unwrap();
            wait(cx, &view, |view, _| {
                view.documents_validation_output().is_some() && view.documents_validation_saved()
            });
            let copy = cx.update(|cx| view.read(cx).documents_validation_output().unwrap());
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| view.document_open(copy.clone(), window, cx))
            })
            .unwrap();
            wait(cx, &view, |view, cx| {
                view.documents_validation_ready()
                    && view.documents.text.read(cx).value().as_ref() == "A new Word paragraph"
            });
            wait(cx, &view, |view, _| view.documents_validation_saved());
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.click("document-new-sheet", cx);
            })
            .unwrap();
            wait(cx, &view, |view, _| {
                view.documents_validation_ready()
                    && view
                        .documents
                        .source
                        .as_ref()
                        .is_some_and(|source| source.kind == DocumentKind::Spreadsheet)
            });
            assert!(cx.update(|cx| {
                view.read(cx)
                    .documents_validation_current_cell(cx)
                    .is_some_and(|cell| cell.editable && cell.value == DocumentCellValue::Blank)
            }));
            shutdown(cx, &client, &paths.data);
        }

        #[gpui_kit::test]
        fn dense_workbook_falls_back_to_focused_pages_without_truncating_cells(
            cx: &mut TestAppContext,
        ) {
            use std::io::{Read, Write};
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let base = directory.path().canonicalize().unwrap();
            let workspace = base.join("project");
            std::fs::create_dir(&workspace).unwrap();
            let source = workspace.join("dense.xlsx");
            workbook(&source);
            let mut parts = Vec::new();
            {
                let mut input =
                    zip::ZipArchive::new(std::fs::File::open(&source).unwrap()).unwrap();
                for index in 0..input.len() {
                    let mut part = input.by_index(index).unwrap();
                    let name = part.name().to_string();
                    let mut bytes = Vec::new();
                    part.read_to_end(&mut bytes).unwrap();
                    parts.push((name, bytes));
                }
            }
            let value = "界".repeat(32767);
            let mut sheet = String::from(
                r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:H4"/><sheetData>"#,
            );
            for row in 1..=4 {
                sheet.push_str(&format!("<row r=\"{row}\">"));
                for column in b'A'..=b'H' {
                    sheet.push_str(&format!(
                        "<c r=\"{}{row}\" t=\"inlineStr\"><is><t>{value}</t></is></c>",
                        column as char
                    ));
                }
                sheet.push_str("</row>");
            }
            sheet.push_str("</sheetData></worksheet>");
            let mut output = zip::ZipWriter::new(std::fs::File::create(&source).unwrap());
            for (name, bytes) in parts {
                output
                    .start_file(
                        &name,
                        zip::write::SimpleFileOptions::default()
                            .compression_method(zip::CompressionMethod::Deflated),
                    )
                    .unwrap();
                output
                    .write_all(if name == "xl/worksheets/sheet1.xml" {
                        sheet.as_bytes()
                    } else {
                        &bytes
                    })
                    .unwrap();
            }
            output.finish().unwrap();
            let paths = xylo_runtime::AppPaths::at(base.join("profile")).unwrap();
            let store = xylo_services::Store::open(&paths.database).unwrap();
            store
                .save_settings(&AppSettings {
                    last_workspace: Some(workspace),
                    ..Default::default()
                })
                .unwrap();
            let host = HostClient::spawn_at(paths.clone()).unwrap();
            let client = host.0.clone();
            cx.update(gpui_kit::init);
            let (window, view) = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| Xylo::new(Ok(host), window, cx))
                })
                .unwrap()
            });
            wait(cx, &view, |view, _| {
                view.workspace_ready && view.settings_loaded
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| view.document_open(source, window, cx))
            })
            .unwrap();
            wait(cx, &view, |view, cx| {
                view.documents_validation_ready() && view.documents_validation_current_cell(cx).is_some_and(|cell|matches!(cell.value,DocumentCellValue::Text{text} if text.chars().count()==32767))
            });
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.documents_validation_cell(0, 1, window, cx)
                })
            })
            .unwrap();
            wait(cx, &view, |view, cx| {
                view.documents_validation_current_cell(cx).is_some_and(|cell|matches!(cell.value,DocumentCellValue::Text{text} if text.chars().count()==32767))
            });
            assert_eq!(
                cx.update(|cx| view
                    .read(cx)
                    .documents
                    .value
                    .read(cx)
                    .value()
                    .chars()
                    .count()),
                32767
            );
            shutdown(cx, &client, &paths.data);
        }
    }
}
