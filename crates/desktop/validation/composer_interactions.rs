//! Production composer controls with no live account, inference, or device use.
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
    #[cfg(test)]
    mod composer_controls {
        use super::{EngineEvent, PermissionMode, ProviderConfig, Xylo};
        use gpui_kit::test::{TestQueryExt, TestWindowExt};
        use gpui_kit::{
            AnyWindowHandle, AppContext, Bounds, Context, Entity, TestAppContext, WindowBounds,
            WindowOptions, px, size,
        };
        fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Xylo>) {
            cx.update(gpui_kit::init);
            cx.update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            Default::default(),
                            size(px(1380.), px(1000.)),
                        ))),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| {
                        cx.new(|cx| {
                            Xylo::new(Err(anyhow::anyhow!("No external service")), window, cx)
                        })
                    },
                )
                .unwrap()
            })
        }
        fn model(view: &mut Xylo, cx: &mut Context<Xylo>) {
            let first = ProviderConfig {
                name: "Fixture".into(),
                model: "future-coder".into(),
                reasoning_options: Some(vec!["low".into(), "high".into(), "xhigh".into()]),
                default_reasoning_effort: Some("high".into()),
                ..Default::default()
            };
            let mut second = first.clone();
            second.model = "future-fast".into();
            second.reasoning_options = Some(vec![]);
            second.default_reasoning_effort = None;
            view.settings.provider = first.clone();
            view.settings.providers = vec![first, second];
            view.error = None;
            cx.notify();
        }
        #[gpui_kit::test]
        fn settings_destinations_and_project_empty_states_are_actionable(cx: &mut TestAppContext) {
            let (handle, view) = open(cx);
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.navigate(super::Page::Appearance, cx);
                });
                window.render_frame(cx);
                window.click("nav-connections", cx);
                window.render_frame(cx);
                window.click("connector-add", cx);
                assert!(view.read(cx).page == super::Page::Connectors);
                window.render_frame(cx);
                assert!(window.find("transport-stdio").visible());
                assert!(window.find_by_label("Connection name").visible());
                window.click("transport-http", cx);
                assert_eq!(view.read(cx).connector_transport, "http");
                window.render_frame(cx);
                assert!(window.find_by_label("Server endpoint").visible());
                window.click("nav-environments", cx);
                assert!(view.read(cx).page == super::Page::Environments);
                view.update(cx, |view, cx| {
                    view.workspace = None;
                    view.navigate(super::Page::Files, cx);
                });
                window.render_frame(cx);
                // One way to open a project from the empty Files pane.
                assert!(window.try_find("files-open-project").is_none());
                assert!(window.find("editor-open-project").visible());
            })
            .unwrap();
        }

        #[gpui_kit::test]
        fn chat_home_setup_follows_project_then_model_state(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.workspace = None;
                    view.settings.provider.model.clear();
                    cx.notify();
                });
                window.render_frame(cx);
                window.find_by_label("Choose project");
                assert!(window.try_find(("chat-home-starter", 0usize)).is_none());
                view.update(cx, |view, cx| {
                    view.workspace = Some("/fixture/project".into());
                    view.workspace_ready = false;
                    cx.notify();
                });
                window.render_frame(cx);
                window.find_by_label("Set up model");
                window.click("chat-home-setup", cx);
                assert!(view.read(cx).page == super::Page::Providers);
                view.update(cx, |view, cx| {
                    view.page = super::Page::Chat;
                    view.settings_open = false;
                    view.drafts = super::drafts::DraftUi::new(window);
                    model(view, cx);
                    assert!(view.settings.provider.validate().is_ok());
                });
                window.render_frame(cx);
                assert!(window.try_find("chat-home-setup").is_none());
                for index in 0..3usize {
                    assert!(window.try_find(("chat-home-starter", index)).is_some());
                }
            })
            .unwrap();
        }

        #[gpui_kit::test]
        fn chat_home_starters_populate_editable_drafts_without_execution(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    model(view, cx);
                    view.workspace = Some("/fixture/project".into());
                    view.workspace_ready = true;
                    view.drafts = super::drafts::DraftUi::new(window);
                    cx.notify();
                });
                for (index, expected) in [
                    "Explain this project",
                    "Review the current project changes",
                    "Inspect this project",
                ]
                .into_iter()
                .enumerate()
                {
                    view.update(cx, |view, cx| {
                        view.composer
                            .update(cx, |input, cx| input.set_value("", window, cx));
                        view.drafts = super::drafts::DraftUi::new(window);
                        cx.notify();
                    });
                    window.render_frame(cx);
                    window.click(("chat-home-starter", index), cx);
                    let state = view.read(cx);
                    let draft = state.composer.read(cx).value();
                    assert!(draft.starts_with(expected));
                    assert!(draft.contains(if index < 2 {
                        "read-only"
                    } else {
                        "before making any edits"
                    }));
                    assert!(state.messages.is_empty());
                    assert!(
                        state.turns.is_empty(),
                        "A starter must not create or execute a turn"
                    );
                    assert!(!state.running());
                    window.render_frame(cx);
                    assert!(window.try_find(("chat-home-starter", 0usize)).is_none());
                }
            })
            .unwrap();
        }

        #[gpui_kit::test]
        fn chat_home_starters_preserve_text_and_recheck_click_state(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    model(view, cx);
                    view.workspace = Some("/fixture/project".into());
                    cx.notify();
                });
                for text in ["Existing draft", " \n\t"] {
                    window.render_frame(cx);
                    view.update(cx, |view, cx| {
                        view.composer
                            .update(cx, |input, cx| input.set_value(text, window, cx));
                        // Invoke the actual click handler after its render-time condition changed.
                        view.populate_chat_starter("Must not replace this draft", window, cx);
                        assert_eq!(view.composer.read(cx).value().as_str(), text);
                        cx.notify();
                    });
                    window.render_frame(cx);
                    assert!(window.try_find(("chat-home-starter", 0usize)).is_none());
                }
                view.update(cx, |view, cx| {
                    view.composer
                        .update(cx, |input, cx| input.set_value("", window, cx));
                    view.recovery.open = true;
                    view.populate_chat_starter("Must not bypass recovery", window, cx);
                    assert!(view.composer.read(cx).value().is_empty());
                    view.recovery.open = false;
                    view.turns
                        .entry(view.session)
                        .or_default()
                        .apply(EngineEvent::Started);
                    view.populate_chat_starter("Must not start while running", window, cx);
                    assert!(view.composer.read(cx).value().is_empty());
                    assert!(view.messages.is_empty());
                });
            })
            .unwrap();
        }

        #[gpui_kit::test]
        fn compact_toolbar_places_permissions_left_and_unified_model_right(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, model);
                window.render_frame(cx);
                let permission = window.find("composer-permissions").bounds();
                let model = window.find("composer-model").bounds();
                let send = window.find("send-prompt").bounds();
                let input = window.find_by_label("Message").bounds();
                assert!(send.bottom() - input.top() <= px(88.), "A one-line composer should not reserve a large blank input region: input={input:?}, send={send:?}");
                assert!(
                    window.try_find("composer-context").is_none(),
                    "The context ring stays hidden until the chat uses 5% of the window"
                );
                assert!(permission.origin.x < model.origin.x && model.origin.x < send.origin.x);
                view.update(cx, |view, cx| {
                    view.turns.entry(view.session).or_default().context =
                        Some(xylo_core::ContextUsage {
                            capacity: 100_000,
                            messages: 17_000,
                            ..Default::default()
                        });
                    cx.notify();
                });
                window.render_frame(cx);
                let context = window.find("composer-context").bounds();
                let model = window.find("composer-model").bounds();
                assert!(
                    permission.origin.x < context.origin.x && context.origin.x < model.origin.x,
                    "The ring sits just before the model control"
                );
                assert!(
                    window.try_find("composer-reasoning").is_none(),
                    "Effort belongs in the unified control, not another toolbar selector"
                );
                window.click("composer-model", cx);
                // The picker lists models directly with an inline reasoning menu.
                assert!(window.try_find("composer-model-list").is_some());
                assert!(window.try_find("composer-effort").is_some());
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn split_workspace_keeps_every_composer_control_inside_the_surface(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open(cx);
            cx.update_window(window,|_,window,cx| {
                view.update(cx,|view,cx| {
                    model(view,cx);view.settings.provider.model="a-deliberately-long-provider-model-name".into();
                    view.settings.ui.sidebar_width=222.;view.settings.ui.work_panel_open=true;view.settings.ui.work_panel_width=320.;
                    cx.notify();
                });
                window.resize(size(px(1000.),px(880.)));window.bounds_changed(cx);window.render_frame(cx);
                let surface=window.find_by_label("Message").bounds();
                let mut right=surface.left()-px(4.);
                for id in ["composer-context-actions","composer-permissions","composer-context","composer-model","record-voice-composer","send-prompt"] {
                    // The context ring is hidden until the chat uses 5% of the window.
                    let Some(item)=window.try_find(id).map(|node|node.bounds()) else { assert_eq!(id,"composer-context"); continue; };
                    assert!(item.left()>=right && item.right()<=surface.right()+px(4.),"{id} clips or overlaps in the actual split workspace: {item:?}, surface {surface:?}");right=item.right();
                }
            }).unwrap();
        }
        #[gpui_kit::test]
        fn unified_model_switching_honors_explicit_unsupported_metadata(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, model);
                window.render_frame(cx);
                window.click("composer-model", cx);
                window.click(("composer-model-choice", 1usize), cx);
                assert_eq!(view.read(cx).settings.provider.model, "future-fast");
                window.render_frame(cx);
                window.click("composer-model", cx);
                assert!(
                    window.try_find("composer-effort").is_none(),
                    "Unsupported models cannot expose effort choices"
                );
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn permission_selector_changes_next_task_defaults_without_stopping_current_turn(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.turns
                        .entry(view.session)
                        .or_default()
                        .apply(EngineEvent::Started);
                    cx.notify();
                });
                window.render_frame(cx);
                window.click("composer-permissions", cx);
                window.within("popup-menu").click(4usize, cx);
                assert_eq!(view.read(cx).settings.permissions, PermissionMode::ReadOnly);
                assert!(
                    view.read(cx).running(),
                    "Changing the next-task default must not replace or stop the active task"
                );
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn shortened_legacy_diff_cannot_be_approved_by_keyboard(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    let id = uuid::Uuid::new_v4();
                    view.turns.entry(view.session).or_default().apply(EngineEvent::ApprovalRequired {
                        parent_id: None, rule_key: None, reason: None, action_id:id, name:"file_edit".into(), arguments:serde_json::json!({"path":"large.txt","diff":"shortened", "diff_truncated":true}), rule:None
                    });
                    // The shortcut applies: chat page, card shown, composer focused.
                    view.composer.update(cx, |input, cx| input.focus(window, cx));
                    view.allow_pending(window, cx);
                    assert!(view.turns[&view.session].tools[0].approval, "A legacy shortened action must remain unapproved");
                });
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn split_preserves_readable_chat_and_restores_saved_panel_preference(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.settings.ui.sidebar_visible=true; view.settings.ui.sidebar_width=222.;
                    view.settings.ui.work_panel_open=true; view.settings.ui.work_panel_width=600.; cx.notify();
                });
                window.resize(size(px(1000.),px(680.))); window.bounds_changed(cx); window.render_frame(cx);
                let message=window.find_by_label("Message").bounds();
                assert!(window.find("native-chat-pane").bounds().size.width >= px(400.));
                assert!(message.size.width >= px(310.), "A large saved panel must not squeeze the chat reading/composing area: {message:?}");
                assert_eq!(view.read(cx).settings.ui.work_panel_width,600., "Effective clamp must retain saved preference");
                window.resize(size(px(1920.),px(880.))); window.bounds_changed(cx); window.render_frame(cx);
                let panel=window.find("work-tab-files").bounds();
                assert!(panel.left()>px(1200.), "Wide viewport must restore the preferred600px panel: {panel:?}");
                assert_eq!(view.read(cx).settings.ui.work_panel_width,600.);
            }).unwrap();
        }
        #[gpui_kit::test]
        fn narrow_workspace_stacks_without_overlap_and_restores_saved_width(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    model(view, cx);
                    view.settings.ui.sidebar_visible = true;
                    view.settings.ui.sidebar_width = 400.;
                    view.settings.ui.work_panel_open = true;
                    view.settings.ui.work_panel = "changes".into();
                    view.settings.ui.work_panel_width = 600.;
                    cx.notify();
                });
                window.resize(size(px(1000.), px(680.)));
                window.bounds_changed(cx);
                window.render_frame(cx);
                let workspace = window.find("native-workspace").bounds();
                let chat = window.find("native-chat-pane").bounds();
                let work = window.find("native-work-pane").bounds();
                assert_eq!(chat.left(), work.left());
                assert_eq!(chat.size.width, work.size.width);
                assert!(
                    chat.bottom() <= work.top(),
                    "Stacked panes overlap: {chat:?}, {work:?}"
                );
                assert!((work.size.height - workspace.size.height / 2.).abs() <= px(1.));
                assert!(work.bottom() <= workspace.bottom());
                assert!(window.try_find("work-resize").is_none());
                let message = window.find_by_label("Message").bounds();
                assert!(
                    message.top() >= chat.top() && message.bottom() <= chat.bottom(),
                    "Home composer must remain usable above work: {message:?}, {chat:?}"
                );
                assert_eq!(view.read(cx).settings.ui.work_panel_width, 600.);
                window.resize(size(px(1920.), px(880.)));
                window.bounds_changed(cx);
                window.render_frame(cx);
                let chat = window.find("native-chat-pane").bounds();
                let work = window.find("native-work-pane").bounds();
                assert!(chat.right() <= work.left());
                assert_eq!(chat.top(), work.top());
                assert_eq!(work.size.width, px(600.));
                assert!(window.try_find("work-resize").is_some());
                let explorer = window.find("git-changes-explorer").bounds();
                let diff = window.find("git-diff-pane").bounds();
                assert!(explorer.size.width <= work.size.width * 0.45);
                assert!(diff.size.width >= work.size.width * 0.5);
                assert_eq!(view.read(cx).settings.ui.work_panel_width, 600.);
                assert_eq!(view.read(cx).settings.ui.work_panel, "changes");
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn full_pending_diff_keeps_tail_beyond_preview_and_original_payload(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                window.resize(size(px(1380.), px(2400.))); window.bounds_changed(cx);
                let diff = format!("{}+FINAL_DIFF_SENTINEL\n", "+long proposed addition\n".repeat(1800));
                let request = serde_json::json!({"path":"large.txt","content":"original executable payload"});
                let presentation = serde_json::json!({"path":"large.txt","diff":diff,"diff_truncated":false,"request":request});
                view.update(cx, |view, cx| {
                    let turn = view.turns.entry(view.session).or_default();
                    turn.apply(EngineEvent::Started);
                    turn.apply(EngineEvent::ApprovalRequired {parent_id: None, rule_key: None, reason: None, action_id:uuid::Uuid::new_v4(), name:"file_write".into(), arguments:presentation.clone(), rule:None});
                    view.invalidate_live_history(); view.sync_history_list(cx); cx.notify();
                });
                window.render_frame(cx);
                window.click(("approval-copy", 0usize), cx);
                assert_eq!(serde_json::from_str::<serde_json::Value>(&cx.read_from_clipboard().unwrap().text().unwrap()).unwrap(), presentation);
                window.click(("approval-inspect", 0usize), cx); window.render_frame(cx);
                assert!(window.find(("approval-full-diff", 0usize)).bounds().size.height > px(0.));
                window.click(("approval-copy-diff", 0usize), cx);
                assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), diff);
            }).unwrap();
        }
        #[gpui_kit::test]
        fn long_pending_action_and_completed_arguments_have_full_inspection(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                let arguments = serde_json::json!({"command":format!("{}FINAL_COMMAND_SENTINEL", "echo long command; ".repeat(700))});
                let id = uuid::Uuid::new_v4();
                view.update(cx, |view, cx| {
                    let turn = view.turns.entry(view.session).or_default();
                    turn.apply(EngineEvent::Started);
                    turn.apply(EngineEvent::ApprovalRequired {parent_id: None, rule_key: None, reason: None, action_id:id, name:"shell_command".into(), arguments:arguments.clone(), rule:None});
                    view.invalidate_live_history(); view.sync_history_list(cx); cx.notify();
                });
                window.render_frame(cx);
                window.click(("approval-copy", 0usize), cx);
                assert_eq!(serde_json::from_str::<serde_json::Value>(&cx.read_from_clipboard().unwrap().text().unwrap()).unwrap(), arguments);
                window.click(("approval-inspect", 0usize), cx);
                window.render_frame(cx);
                assert!(window.try_find(("approval-full", 0usize)).is_some());
                view.update(cx, |view, cx| {
                    view.turns.get_mut(&view.session).unwrap().apply(EngineEvent::ToolFinished { parent_id: None, action_id:id, name:"shell_command".into(), result:"done".into(), success:true });
                    view.expanded_tools.insert(id);
                    view.invalidate_live_history(); view.sync_history_list(cx); cx.notify();
                });
                window.render_frame(cx);
                window.click(("tool-copy-input", 0usize), cx);
                assert_eq!(serde_json::from_str::<serde_json::Value>(&cx.read_from_clipboard().unwrap().text().unwrap()).unwrap(), arguments);
            }).unwrap();
        }
        #[gpui_kit::test]
        fn queued_messages_stay_above_composer_and_with_their_chat(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    model(view, cx);
                    view.error = None;
                    view.settings_open = false;
                    view.workspace = Some(std::path::PathBuf::from("/fixture"));
                    view.validation_draft_surface(None, false, window, cx);
                    view.error = None;
                    view.messages.push(super::message(
                        view.session,
                        "user",
                        "Work on this project".into(),
                    ));
                    view.turns
                        .entry(view.session)
                        .or_default()
                        .apply(EngineEvent::Started);
                    let turn = xylo_core::TurnRequest {
                        approved_rules: Vec::new(),
                        session_id: view.session,
                        turn_id: uuid::Uuid::new_v4(),
                        workspace: view.workspace.clone().unwrap(),
                        prompt: "Then check the tests and summarize changes. ".repeat(10),
                        provider: view.settings.provider.clone(),
                        permissions: view.settings.permissions,
                        attachments: vec![],
                    };
                    view.turn_queue.items.insert(
                        view.session,
                        vec![xylo_runtime::turn_queue::QueuedTurn {
                            id: turn.turn_id,
                            turn,
                            paused: false,
                            reason: None,
                        }],
                    );
                    view.composer
                        .update(cx, |input, cx| input.set_value("A new message", window, cx));
                    cx.notify();
                });
                for (width, height) in [(1000., 680.), (1380., 880.), (1720., 1040.)] {
                    window.resize(size(px(width), px(height)));
                    window.bounds_changed(cx);
                    window.render_frame(cx);
                    let queue = window.find("composer-pending").bounds();
                    let composer = window.find("image-drop-target").bounds();
                    assert!(
                        queue.bottom() <= composer.top(),
                        "Pending messages belong above the composer"
                    );
                    assert!(
                        composer.bottom() <= px(height) && composer.bottom() >= px(height - 35.),
                        "Conversation composer stays bottom anchored"
                    );
                    assert!(window.find(("remove-queued", 0usize)).visible());
                    assert!(
                        window.try_find("saved-drafts").is_none(),
                        "Routine queued status must not add a duplicate footer"
                    );
                    assert!(window.find("steer-prompt").visible());
                    assert_eq!(window.find("send-prompt").label(), Some("Queue message"));
                }
                view.update(cx, |view, cx| {
                    view.session = uuid::Uuid::new_v4();
                    cx.notify();
                });
                window.render_frame(cx);
                assert!(
                    window.try_find("composer-pending").is_none(),
                    "Another chat cannot show the previous chat's queue"
                );
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn busy_composer_keeps_stop_while_composing_guidance(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.turns
                        .entry(view.session)
                        .or_default()
                        .apply(EngineEvent::Started);
                    cx.notify();
                });
                window.render_frame(cx);
                assert!(window.try_find("cancel-task").is_some());
                assert!(window.try_find("send-prompt").is_none());
                view.update(cx, |view, cx| {
                    view.composer.update(cx, |input, cx| {
                        input.set_value("Adjust this while working", window, cx)
                    })
                });
                window.render_frame(cx);
                assert!(window.try_find("send-prompt").is_some());
                assert_eq!(window.find("send-prompt").label(), Some("Queue message"));
                assert!(
                    window.try_find("steer-prompt").is_some(),
                    "Busy composer needs an explicit Steer control"
                );
                assert!(
                    window.try_find("cancel-task").is_some(),
                    "Stop must remain available alongside guidance"
                );
                view.update(cx, |view, cx| {
                    view.settings.ui.sidebar_width = 222.;
                    view.settings.ui.work_panel_open = true;
                    view.settings.ui.work_panel_width = 460.;
                    cx.notify();
                });
                window.resize(size(px(1000.), px(680.)));
                window.bounds_changed(cx);
                window.render_frame(cx);
                let surface = window.find_by_label("Message").bounds();
                for id in ["cancel-task", "steer-prompt", "send-prompt"] {
                    let bounds = window.find(id).bounds();
                    assert!(
                        bounds.left() >= surface.left()
                            && bounds.right() <= surface.right() + px(4.),
                        "{id} must remain within narrow composer: {bounds:?} vs {surface:?}"
                    );
                }
            })
            .unwrap();
        }
    }
}
