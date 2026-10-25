//! Actual GPUI update/save/terminal state transitions with correlated fixtures.
//! No release is downloaded, no updater installs, and no audio device is opened.
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
    mod updater_native_tests {
        use super::Workbench;
        use gpui_kit::{
            AnyWindowHandle, AppContext, Context, Entity, TestAppContext, WindowOptions,
        };
        use serde_json::json;
        use std::path::PathBuf;
        use uuid::Uuid;
        use workbench_runtime::host::HostEvent;

        fn open_view(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Workbench>) {
            cx.update(gpui_kit::init);
            cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| {
                        Workbench::new(
                            Err(anyhow::anyhow!("Isolated native interaction fixture")),
                            window,
                            cx,
                        )
                    })
                })
                .unwrap()
            })
        }
        fn response(view: &mut Workbench, sequence: u64, phase: &str, cx: &mut Context<Workbench>) {
            let id = Uuid::new_v4();
            view.updates.pending.insert(id, "updates_status".into());
            assert!(view.update_result(
                id,
                &json!({"sequence":sequence,"phase":phase,
                "enabled":true,"can_install":phase=="ready","restart_required":false}),
                cx
            ));
        }

        #[gpui_kit::test]
        fn stale_snapshots_cannot_stop_active_progress_or_restart_completed_polling(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open_view(cx);
            cx.update_window(window, |_, _, cx| {
                view.update(cx, |view, cx| {
                    response(view, 10, "downloading", cx);
                    assert!(view.updates.poll.is_some());
                    response(view, 9, "idle", cx);
                    assert_eq!(view.updates.status["sequence"], 10);
                    assert!(
                        view.updates.poll.is_some(),
                        "Stale idle response stopped a live download timer"
                    );
                    response(view, 11, "ready", cx);
                    assert!(view.updates.poll.is_none());
                    response(view, 10, "downloading", cx);
                    assert_eq!(view.updates.status["phase"], "ready");
                    assert!(
                        view.updates.poll.is_none(),
                        "Stale download response revived completed polling"
                    );
                })
            })
            .unwrap();
        }

        #[gpui_kit::test]
        fn save_enqueue_failure_releases_install_intent_and_retains_editor_contents(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open_view(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    let path = PathBuf::from("unsaved.txt");
                    view.editor_path = Some(path.clone());
                    view.editor.update(cx, |editor, cx| {
                        editor.set_value("keep these edits", window, cx)
                    });
                    view.editor_dirty = true;
                    view.updates.after_save = Some(path);
                    view.save_editor(false, cx); // Missing host fails before enqueue.
                    assert!(view.updates.after_save.is_none());
                    assert!(view.pending_saves.is_empty());
                    assert!(view.editor_dirty);
                    assert_eq!(view.editor.read(cx).value().as_ref(), "keep these edits");
                    assert!(!view.updates.installing);
                })
            })
            .unwrap();
        }

        #[gpui_kit::test]
        fn save_ack_with_newer_edits_or_save_failure_never_begins_install(cx: &mut TestAppContext) {
            let (window, view) = open_view(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    let path = PathBuf::from("draft.txt");
                    view.editor_path = Some(path.clone());
                    view.editor
                        .update(cx, |editor, cx| editor.set_value("newer text", window, cx));
                    view.editor_dirty = true;
                    view.pending_saves.insert(path.clone(), "older text".into());
                    view.updates.after_save = Some(path.clone());
                    view.event(HostEvent::Saved(path.clone()), window, cx);
                    assert!(view.updates.after_save.is_none());
                    assert!(!view.updates.installing);
                    assert!(view.editor_dirty);
                    view.pending_saves.insert(path.clone(), "newer text".into());
                    view.updates.after_save = Some(path.clone());
                    view.event(
                        HostEvent::SaveFailed {
                            path,
                            message: "Fixture denied write".into(),
                        },
                        window,
                        cx,
                    );
                    assert!(view.updates.after_save.is_none());
                    assert!(view.pending_saves.is_empty());
                    assert!(!view.updates.installing);
                    assert_eq!(view.editor.read(cx).value().as_ref(), "newer text");
                })
            })
            .unwrap();
        }

        #[gpui_kit::test]
        fn refused_install_preserves_real_terminal_entity_and_live_shell(cx: &mut TestAppContext) {
            let directory = tempfile::tempdir().unwrap();
            // Shell creation is explicit test setup, away from production render.
            let (session, events) =
                workbench_runtime::terminal::TerminalSession::spawn(directory.path()).unwrap();
            let (window, view) = open_view(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    let terminal = cx.new(|cx| {
                        crate::terminal_workspace::TerminalWorkspace::new(directory.path().to_path_buf(), session, events, window, cx)
                    });
                    let identity = terminal.entity_id();
                    view.terminal = Some(terminal);
                    view.terminal_visible = true;
                    response(view, 20, "ready", cx);
                    view.begin_update_install(window, cx);
                    assert!(
                        view.terminal.is_some(),
                        "A refused install discarded the native PTY"
                    );
                    assert_eq!(view.terminal.as_ref().unwrap().entity_id(), identity);
                    assert!(view.terminal_visible);
                    assert!(!view.updates.installing);
                    assert!(!view.quitting);
                    // Release our own test shell through the normal owner lifecycle.
                    view.terminal = None;
                })
            })
            .unwrap();
        }

        #[gpui_kit::test]
        fn install_error_restores_product_ui_without_clearing_draft(cx: &mut TestAppContext) {
            let (window, view) = open_view(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.editor.update(cx, |editor, cx| {
                        editor.set_value("retained draft", window, cx)
                    });
                    view.editor_dirty = true;
                    response(view, 20, "ready", cx);
                    view.updates.installing = true;
                    let id = Uuid::new_v4();
                    view.updates.pending.insert(id, "updates_install".into());
                    assert!(view.update_error(id, "The update could not be verified.", cx));
                    assert!(!view.updates.installing);
                    assert!(!view.quitting);
                    assert!(view.editor_dirty);
                    assert_eq!(view.editor.read(cx).value().as_ref(), "retained draft");
                    response(view, 21, "ready", cx);
                    assert_eq!(
                        view.updates.notice, "The update could not be verified.",
                        "A same-phase status refresh hid the actionable install failure"
                    );
                })
            })
            .unwrap();
        }

        #[gpui_kit::test]
        fn cancelling_install_save_prompt_preserves_the_draft_and_starts_no_action(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open_view(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.editor_path = Some(PathBuf::from("draft.txt"));
                    view.editor.update(cx, |editor, cx| {
                        editor.set_value("never discard this", window, cx)
                    });
                    view.editor_dirty = true;
                    response(view, 10, "ready", cx);
                    view.request_update_install(window, cx);
                })
            })
            .unwrap();
            assert!(cx.has_pending_prompt());
            cx.simulate_prompt_answer("Cancel");
            cx.run_until_parked();
            cx.update(|cx| {
                let view = view.read(cx);
                assert!(view.editor_dirty);
                assert!(!view.updates.prompt_open);
                assert!(view.updates.after_save.is_none());
                assert!(!view.updates.installing);
                assert!(view.updates.pending.is_empty());
                assert_eq!(view.editor.read(cx).value().as_ref(), "never discard this");
            });
        }

        #[gpui_kit::test]
        fn unsent_composer_requires_durable_flush_before_install_and_after_file_save(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open_view(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    response(view, 10, "ready", cx);
                    view.error = None;
                    view.composer.update(cx, |composer, cx| {
                        composer.set_value("unsent message", window, cx)
                    });
                    view.request_update_install(window, cx);
                    assert!(!view.updates.installing);
                    assert!(view.updates.pending.is_empty());
                    assert!(view.draft_flush_pending(),"Installation must wait for the exact draft save receipt");
                    // File save completion uses this second entrypoint. A message
                    // typed while saving must still prevent the closing install.
                    view.updates.after_save = Some(PathBuf::from("saved.txt"));
                    view.begin_update_install(window, cx);
                    assert!(view.updates.after_save.is_some(),"The saved-file installation intent remains pending behind draft persistence");
                    assert!(!view.updates.installing);
                    assert!(view.updates.pending.is_empty());
                    assert_eq!(view.composer.read(cx).value().as_ref(), "unsent message");
                })
            })
            .unwrap();
            assert!(!cx.has_pending_prompt());
        }
    }
}
