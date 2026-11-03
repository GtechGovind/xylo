use super::super::Xylo;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AnyWindowHandle, AppContext, Bounds, Entity, Focusable, TestAppContext, WindowBounds,
    WindowOptions, px, size,
};
use uuid::Uuid;
use xylo_core::{Conversation, TitleSource, WorkspacePanelPreferences};
use xylo_runtime::host::HostEvent;

fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Xylo>) {
    cx.update(gpui_kit::init);
    cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(1000.), px(680.)),
                ))),
                ..Default::default()
            },
            cx,
            |window, cx| {
                cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Offline fixture")), window, cx))
            },
        )
        .unwrap()
    })
}

#[gpui_kit::test]
fn a_loop_command_keeps_its_draft_when_dispatch_fails(cx: &mut TestAppContext) {
    let (window, view) = open(cx);
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.workspace = Some("/fixture/project".into());
            let command = "/loop 5m inspect the build";
            view.composer
                .update(cx, |input, cx| input.set_value(command, window, cx));
            view.submit(window, cx);
            assert_eq!(view.composer.read(cx).value().as_str(), command);
            assert!(view.error.is_some());
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_failed_search_dispatch_does_not_keep_saying_searching(cx: &mut TestAppContext) {
    let (window, view) = open(cx);
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.open_history_search(Some("needle".into()), window, cx);
            view.history_search_submit(cx);
            assert!(view.history_search.validation_request().is_none());
            assert!(
                !view
                    .history_search
                    .validation_notice()
                    .contains("Searching")
            );
            assert!(
                view.history_search
                    .validation_notice()
                    .contains("unavailable")
            );
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_failed_sidebar_mutation_keeps_the_conversation_visible(cx: &mut TestAppContext) {
    let (window, view) = open(cx);
    cx.update_window(window, |_, _, cx| {
        view.update(cx, |view, cx| {
            let id = Uuid::new_v4();
            view.conversations.push(Conversation {
                id,
                title: "Keep me".into(),
                workspace: "/fixture/project".into(),
                updated_at: 0,
                title_source: TitleSource::User,
                pinned: false,
                archived: false,
            });
            view.set_chat_pinned(id, true, cx);
            assert!(!view.conversations[0].pinned);
            view.archive_chat(id, cx);
            assert!(view.conversations.iter().any(|chat| chat.id == id));
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn search_dismissal_restores_keyboard_focus(cx: &mut TestAppContext) {
    let (window, view) = open(cx);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        view.update(cx, |view, cx| {
            view.composer
                .update(cx, |input, cx| input.focus(window, cx));
            view.open_history_search(None, window, cx);
        });
        window.render_frame(cx);
        window.press("escape", cx);
        assert!(
            view.read(cx)
                .composer
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn quit_shortcut_reaches_the_unsaved_editor_guard(cx: &mut TestAppContext) {
    let (window, view) = open(cx);
    cx.update(|cx| {
        cx.set_global(crate::ShellState {
            window,
            view: view.clone(),
            _tray: None,
        });
        crate::configure_shell_actions(cx);
        let quit_action = cx
            .get_menus()
            .unwrap()
            .into_iter()
            .flat_map(|menu| menu.items)
            .find_map(|item| match item {
                gpui_kit::OwnedMenuItem::Action { name, action, .. } if name == "Quit Xylo" => {
                    Some(action)
                }
                _ => None,
            })
            .expect("The native app menu keeps its Quit action");
        assert!(quit_action.as_any().is::<crate::xylo::QuitApplication>());
        assert!(
            cx.key_bindings()
                .borrow()
                .bindings_for_action(quit_action.as_ref())
                .next()
                .is_some(),
            "The native Quit menu action must keep its keyboard accelerator"
        );
    });
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.editor_dirty = true;
            view.editor_path = Some("/fixture/unsaved.rs".into());
            view.composer
                .update(cx, |input, cx| input.focus(window, cx));
        });
        window.render_frame(cx);
        window.press(
            if cfg!(target_os = "macos") {
                "cmd-q"
            } else {
                "ctrl-q"
            },
            cx,
        );
        assert!(
            view.read(cx).quit_prompt_open,
            "Quit must reach the unsaved editor prompt"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn canonical_workspace_keeps_one_project_and_its_panel_preferences(cx: &mut TestAppContext) {
    let (window, view) = open(cx);
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            let requested = std::path::PathBuf::from("/fixture/alias");
            let root = std::path::PathBuf::from("/fixture/canonical");
            view.workspace = Some(requested.clone());
            view.settings.ui.open_projects = vec![requested.clone(), root.clone()];
            view.settings.ui.collapsed_projects = vec![requested.clone()];
            view.sidebar_state.collapsed.insert(requested.clone());
            view.settings.ui.work_panel_open = true;
            view.settings.ui.work_panel = "changes".into();
            view.settings.ui.work_panel_width = 768.;
            view.settings.ui.project_panels.insert(
                requested.clone(),
                WorkspacePanelPreferences {
                    open: true,
                    panel: "changes".into(),
                    width: 768.,
                },
            );
            view.event(
                HostEvent::WorkspaceOpened {
                    requested: requested.clone(),
                    root: root.clone(),
                },
                window,
                cx,
            );
            assert_eq!(view.workspace.as_ref(), Some(&root));
            assert_eq!(view.settings.ui.open_projects, vec![root.clone()]);
            assert!(view.sidebar_state.collapsed.contains(&root));
            assert_eq!(view.settings.ui.project_panels[&root].width, 768.);
            assert!(!view.settings.ui.project_panels.contains_key(&requested));
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn an_unseen_alias_restores_the_canonical_projects_saved_panel(cx: &mut TestAppContext) {
    let (window, view) = open(cx);
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            let requested = std::path::PathBuf::from("/fixture/new-alias");
            let root = std::path::PathBuf::from("/fixture/canonical");
            view.workspace = Some(requested.clone());
            view.settings.ui.project_panels.insert(
                root.clone(),
                WorkspacePanelPreferences {
                    open: true,
                    panel: "changes".into(),
                    width: 480.,
                },
            );
            view.event(
                HostEvent::WorkspaceOpened {
                    requested,
                    root: root.clone(),
                },
                window,
                cx,
            );
            assert!(view.settings.ui.work_panel_open);
            assert_eq!(view.settings.ui.work_panel, "changes");
            assert_eq!(view.settings.ui.work_panel_width, 480.);
            assert_eq!(view.settings.ui.project_panels[&root].width, 480.);
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn file_splitter_tracks_the_full_width_stacked_work_pane(cx: &mut TestAppContext) {
    let (window, view) = open(cx);
    cx.update_window(window, |_, _, cx| {
        view.update(cx, |view, _| {
            view.settings.ui.sidebar_visible = true;
            view.settings.ui.sidebar_width = 400.;
            view.settings.ui.work_panel_width = 640.;
            assert_eq!(view.files_width_at_pointer(1000., 560.), 160.);
            assert_eq!(view.files_width_at_pointer(1000., 900.), 270.);
            view.settings.ui.sidebar_visible = false;
            assert_eq!(view.files_width_at_pointer(1000., 560.), 160.);
        });
    })
    .unwrap();
}
