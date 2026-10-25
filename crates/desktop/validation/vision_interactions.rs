//! Real native image import, composer removal and restored-history previews.
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
    mod vision_native_tests {
        use super::{AttachmentUi, DraftUi, Workbench};
        use gpui_kit::{
            AppContext, Bounds, ClipboardEntry, ClipboardItem, ExternalPaths, FileDropEvent,
            InputEvent, SharedString, TestAppContext, WindowBounds, WindowOptions, px, size,
            test::TestWindowExt,
        };
        use std::{
            path::PathBuf,
            time::{Duration, Instant},
        };
        use uuid::Uuid;
        use workbench_core::{AttachmentSource, PermissionMode, ProviderConfig, TurnRequest};
        use workbench_runtime::{
            AppPaths,
            drafts::DraftKey,
            host::{HostClient, HostEvent},
        };
        use workbench_services::{Store, attachments::AttachmentStore};

        const PNG: &[u8] = include_bytes!("../../../packaging/icons/app.png");
        fn open(
            cx: &mut TestAppContext,
            paths: AppPaths,
        ) -> (
            HostClient,
            gpui_kit::AnyWindowHandle,
            gpui_kit::Entity<Workbench>,
        ) {
            cx.executor().allow_parking();
            cx.update(gpui_kit::init);
            let host = HostClient::spawn_at(paths).unwrap();
            let client = host.0.clone();
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
            wait(cx, &view, "settings", |view, _| view.settings_loaded);
            (client, window, view)
        }
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
        #[gpui_kit::test]
        fn native_clipboard_file_drop_remove_and_history_reopen_keep_only_managed_refs(
            cx: &mut TestAppContext,
        ) {
            let directory = tempfile::tempdir().unwrap();
            let base = directory.path().canonicalize().unwrap();
            let workspace = base.join("project");
            std::fs::create_dir(&workspace).unwrap();
            let selected = workspace.join("selected.png");
            std::fs::write(&selected, PNG).unwrap();
            let paths = AppPaths::at(base.join("profile")).unwrap();
            let store = Store::open(&paths.database).unwrap();
            let (client, window, view) = open(cx, paths.clone());
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.set_workspace(workspace.clone(), window, cx)
                })
            })
            .unwrap();
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| {
                cx.write_to_clipboard(ClipboardItem::from(ClipboardEntry::ExternalPaths(
                    ExternalPaths(vec![selected.clone()].into()),
                )));
                window.render_frame(cx);
                window.click("paste-image-files", cx);
                assert!(view.read(cx).attachments.importing());
            })
            .unwrap();
            wait(cx, &view, "clipboard image import", |view, _| {
                view.draft_attachments().len() == 1 && !view.attachments.importing()
            });
            let first = cx.update(|cx| view.read(cx).draft_attachments()[0].clone());
            assert_eq!(first.source, AttachmentSource::PastedFile);
            assert_eq!(first.name, "selected.png");
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                let position = window.find("paste-image-files").bounds().center();
                window.dispatch_event(
                    FileDropEvent::Entered {
                        position,
                        paths: ExternalPaths(vec![selected.clone()].into()),
                    }
                    .to_platform_input(),
                    cx,
                );
                window.render_frame(cx);
                window.dispatch_event(FileDropEvent::Submit { position }.to_platform_input(), cx);
            })
            .unwrap();
            wait(cx, &view, "native file drop import", |view, _| {
                view.draft_attachments().len() == 2 && !view.attachments.importing()
            });
            let images = cx.update(|cx| view.read(cx).draft_attachments());
            assert_eq!(images[1].source, AttachmentSource::Drop);
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.click(SharedString::from(format!("remove-image-{}", first.id)), cx);
                assert_eq!(view.read(cx).draft_attachments().len(), 1);
            })
            .unwrap();
            let remaining = cx.update(|cx| view.read(cx).draft_attachments()[0].clone());
            assert_eq!(remaining.id, images[1].id);
            let session = cx.update(|cx| view.read(cx).session);
            cx.update(|cx| {
                view.update(cx, |view, cx| {
                    assert!(view.draft_before_switch(cx));
                })
            });
            let key = DraftKey {
                session_id: session,
                workspace: Some(workspace.clone()),
            };
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                cx.run_until_parked();
                if store.draft(&key).unwrap().attachments == vec![remaining.clone()] {
                    break;
                }
                assert!(Instant::now() < deadline, "Draft image was not persisted");
                std::thread::sleep(Duration::from_millis(10));
            }
            let turn = TurnRequest {
                session_id: session,
                turn_id: Uuid::new_v4(),
                prompt: "Durable image history".into(),
                workspace: workspace.clone(),
                provider: ProviderConfig::default(),
                permissions: PermissionMode::ReadOnly,
                attachments: vec![remaining.clone()],
            };
            store.register_user_turn(&turn).unwrap();
            assert_eq!(
                store.messages(session, 10).unwrap()[0].attachments,
                vec![remaining.clone()]
            );
            std::fs::remove_file(&selected).unwrap();
            // Recreate transient view state under the profile's single host
            // owner; the worker integration separately proves process restart.
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.new_chat(cx);
                    view.drafts = DraftUi::new(window);
                    view.attachments = AttachmentUi::new();
                    view.histories.clear();
                    view.messages.clear();
                    view.open_conversation(session, workspace.clone(), window, cx)
                })
            })
            .unwrap();
            wait(cx, &view, "restored draft and image history", |view, _| {
                view.draft_attachments() == vec![remaining.clone()]
                    && view
                        .messages
                        .iter()
                        .any(|message| message.attachments == vec![remaining.clone()])
            });
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.click(
                    SharedString::from(format!("image-preview-{}", remaining.id)),
                    cx,
                );
            })
            .unwrap();
            wait(cx, &view, "managed history preview", |view, _| {
                view.attachments.validation_preview(remaining.id).is_some()
            });
            let preview = cx.update(|cx| {
                view.read(cx)
                    .attachments
                    .validation_preview(remaining.id)
                    .unwrap()
            });
            assert!(preview.starts_with(base.join("profile").join("artifacts")));
            assert_ne!(preview, selected);
            assert!(preview.metadata().unwrap().len() <= 512 * 1024);
            client.shutdown().unwrap();
            cx.run_until_parked();
        }
        #[gpui_kit::test]
        fn late_import_for_previous_chat_cannot_attach_to_new_chat(cx: &mut TestAppContext) {
            let directory = tempfile::tempdir().unwrap();
            let paths = AppPaths::at(directory.path().join("profile")).unwrap();
            let (client, window, view) = open(cx, paths.clone());
            let selected = directory.path().join("selected.png");
            std::fs::write(&selected, PNG).unwrap();
            let original_session = cx.update(|cx| view.read(cx).session);
            let assets = AttachmentStore::new(paths.artifacts.clone()).unwrap();
            let image = assets
                .import_file(
                    original_session,
                    selected.clone(),
                    AttachmentSource::Picker,
                    &tokio_util::sync::CancellationToken::new(),
                )
                .unwrap();
            let preview = assets.preview_path(&image).unwrap();
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.attach_image_paths(vec![selected.clone()], AttachmentSource::Picker, cx);
                    let (id, old_session) = view.attachments.validation_import().unwrap();
                    assert_eq!(old_session, original_session);
                    view.new_chat(cx);
                    assert_ne!(view.session, old_session);
                    view.event(
                        HostEvent::ImageImported {
                            request_id: id,
                            session_id: old_session,
                            result: Ok((image, preview)),
                        },
                        window,
                        cx,
                    );
                    assert!(view.draft_attachments().is_empty());
                })
            })
            .unwrap();
            wait(
                cx,
                &view,
                "discarded previous import completion",
                |view, _| !view.attachments.importing(),
            );
            cx.update(|cx| assert!(view.read(cx).draft_attachments().is_empty()));
            client.shutdown().unwrap();
            cx.run_until_parked();
        }
    }
}
