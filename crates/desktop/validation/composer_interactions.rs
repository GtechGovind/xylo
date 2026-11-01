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
            AnyWindowHandle, AppContext, Bounds, Context, Entity, Focusable, TestAppContext,
            WindowBounds, WindowOptions, px, size,
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
        fn local_interaction_fixture(
            view: &mut Xylo,
            window: &mut gpui_kit::Window,
            cx: &mut Context<Xylo>,
        ) -> (uuid::Uuid, xylo_core::ImageAttachment) {
            model(view, cx);
            view.messages
                .push(super::message(view.session, "user", "Fixture task".into()));
            view.turns
                .entry(view.session)
                .or_default()
                .apply(EngineEvent::Started);
            view.validation_draft_surface(None, false, window, cx);
            view.composer.update(cx, |input, cx| {
                input.set_value("Keep this main draft", window, cx)
            });
            view.draft_changed(cx);
            let image = xylo_core::ImageAttachment {
                id: uuid::Uuid::new_v4(),
                session_id: view.session,
                name: "draft.png".into(),
                media_type: "image/png".into(),
                sha256: "a".repeat(64),
                bytes: 16,
                width: 2,
                height: 2,
                source: xylo_core::AttachmentSource::Picker,
                imported_at: 1,
            };
            view.validation_draft_images(vec![image.clone()], vec![]);
            (uuid::Uuid::new_v4(), image)
        }

        fn local_key(cx: &mut TestAppContext, handle: AnyWindowHandle, key: &str) {
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.press(key, cx);
            })
            .unwrap();
            cx.run_until_parked();
        }
        fn local_value(
            cx: &mut TestAppContext,
            handle: AnyWindowHandle,
            view: &Entity<Xylo>,
            id: uuid::Uuid,
            slot: usize,
            text: &str,
        ) {
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    let input = view
                        .interaction_inputs
                        .borrow()
                        .get(&(view.session, id, slot))
                        .unwrap()
                        .0
                        .clone();
                    input.update(cx, |input, cx| {
                        input.set_value(text.to_owned(), window, cx);
                        input.focus(window, cx);
                    });
                })
            })
            .unwrap();
            cx.run_until_parked();
        }

        #[gpui_kit::test]
        fn question_other_enter_answers_without_queueing(cx: &mut TestAppContext) {
            let (handle, view) = open(cx);
            let (sender, receiver) = std::sync::mpsc::sync_channel(16);
            let (id, image) = cx
                .update_window(handle, |_, window, cx| {
                    let fixture = view.update(cx, |view, cx| {
                        let fixture = local_interaction_fixture(view, window, cx);
                        view.validation_worker_commands = Some(sender);
                        let questions = vec![
                            xylo_core::UserQuestion {
                                header: "Scope".into(),
                                question: "Which scope?".into(),
                                options: vec![xylo_core::QuestionOption {
                                    label: "Small".into(),
                                    description: "One file".into()
                                }],
                                multi_select: false
                            };
                            2
                        ];
                        view.turns.get_mut(&view.session).unwrap().apply(
                            EngineEvent::QuestionRequired {
                                action_id: fixture.0,
                                questions,
                            },
                        );
                        cx.notify();
                        fixture
                    });
                    window.render_frame(cx);
                    window.click("question-other-0-0", cx);
                    window.render_frame(cx);
                    fixture
                })
                .unwrap();
            local_value(cx, handle, &view, id, 0, "Local answer");
            local_key(cx, handle, "shift-enter");
            assert!(receiver.try_recv().is_err());
            local_key(cx, handle, "enter");
            assert!(
                receiver.try_recv().is_err(),
                "The second question is unanswered"
            );
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.click("question-0-1-0", cx);
            })
            .unwrap();
            local_value(cx, handle, &view, id, 0, &"é".repeat(4097));
            local_key(cx, handle, "enter");
            assert!(receiver.try_recv().is_err());
            local_value(cx, handle, &view, id, 0, "Local answer");
            local_key(cx, handle, "enter");
            let (session, command) = receiver.try_recv().unwrap();
            assert!(
                matches!(command, super::WorkerCommand::Answer { action_id, answer: xylo_core::InteractionAnswer::Questions { ref answers } } if action_id == id && answers.len() == 2 && answers[0].other.as_deref() == Some("Local answer") && answers[1].selected == vec![0])
            );
            local_key(cx, handle, "enter");
            assert!(
                receiver.try_recv().is_err(),
                "Pending acknowledgement prevents duplicate answers"
            );
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    assert_eq!(
                        view.composer.read(cx).value().as_ref(),
                        "Keep this main draft"
                    );
                    assert_eq!(view.draft_attachments()[0].id, image.id);
                    assert!(
                        view.turn_queue
                            .items
                            .get(&session)
                            .is_none_or(Vec::is_empty)
                    );
                    view.event(
                        super::HostEvent::InteractionReply {
                            session,
                            action_id: id,
                            result: Err("Please try again".into()),
                        },
                        window,
                        cx,
                    );
                    assert!(
                        view.interaction_inputs
                            .borrow()
                            .contains_key(&(session, id, 0))
                    );
                    assert!(!view.interaction_pending.contains(&(session, id)));
                    view.session = uuid::Uuid::new_v4();
                    view.event(
                        super::HostEvent::InteractionReply {
                            session,
                            action_id: id,
                            result: Ok(()),
                        },
                        window,
                        cx,
                    );
                    assert!(
                        !view
                            .interaction_inputs
                            .borrow()
                            .contains_key(&(session, id, 0))
                    );
                    assert!(!view.turns[&session].tools[0].approval);
                    assert_eq!(
                        view.composer.read(cx).value().as_ref(),
                        "Keep this main draft"
                    );
                })
            })
            .unwrap();
        }

        #[gpui_kit::test]
        fn plan_feedback_preserves_main_draft(cx: &mut TestAppContext) {
            let (handle, view) = open(cx);
            let (sender, receiver) = std::sync::mpsc::sync_channel(16);
            let (id, image) = cx
                .update_window(handle, |_, window, cx| {
                    let fixture = view.update(cx, |view, cx| {
                        let fixture = local_interaction_fixture(view, window, cx);
                        view.validation_worker_commands = Some(sender);
                        view.event(
                            super::HostEvent::PlanModes {
                                session: view.session,
                                action_id: fixture.0,
                                modes: vec![PermissionMode::FullAccess],
                                scheduled: false,
                            },
                            window,
                            cx,
                        );
                        view.turns.get_mut(&view.session).unwrap().apply(
                            EngineEvent::PlanRequired {
                                action_id: fixture.0,
                                plan: "Inspect, change, verify.".into(),
                            },
                        );
                        cx.notify();
                        fixture
                    });
                    window.render_frame(cx);
                    assert!(window.find(("plan-permissions", 0usize)).visible());
                    assert_eq!(
                        window.find(("start-plan", 0usize)).label(),
                        Some("Start this plan")
                    );
                    fixture
                })
                .unwrap();
            cx.update_window(handle, |_, window, cx| {
                window.click(("toggle-plan-feedback", 0usize), cx);
                window.render_frame(cx);
            })
            .unwrap();
            local_value(cx, handle, &view, id, usize::MAX, "");
            local_key(cx, handle, "enter");
            assert!(receiver.try_recv().is_err());
            local_value(cx, handle, &view, id, usize::MAX, &"é".repeat(4097));
            local_key(cx, handle, "enter");
            assert!(receiver.try_recv().is_err());
            local_value(
                cx,
                handle,
                &view,
                id,
                usize::MAX,
                "Change only the requested screen",
            );
            local_key(cx, handle, "shift-enter");
            assert!(receiver.try_recv().is_err());
            local_key(cx, handle, "enter");
            let (session, command) = receiver.try_recv().unwrap();
            assert!(
                matches!(command, super::WorkerCommand::Answer { action_id, answer: xylo_core::InteractionAnswer::KeepPlanning { ref feedback } } if action_id == id && feedback.trim() == "Change only the requested screen")
            );
            assert!(receiver.try_recv().is_err());
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    assert_eq!(
                        view.composer.read(cx).value().as_ref(),
                        "Keep this main draft"
                    );
                    assert_eq!(view.draft_attachments()[0].id, image.id);
                    assert!(
                        view.turn_queue
                            .items
                            .get(&session)
                            .is_none_or(Vec::is_empty)
                    );
                    view.event(
                        super::HostEvent::InteractionReply {
                            session,
                            action_id: id,
                            result: Err("Not accepted".into()),
                        },
                        window,
                        cx,
                    );
                    assert!(view.interaction_inputs.borrow().contains_key(&(
                        session,
                        id,
                        usize::MAX
                    )));
                })
            })
            .unwrap();
        }

        #[gpui_kit::test]
        fn plan_modes_require_allowed_selection_and_keep_scheduled_restrictions(
            cx: &mut TestAppContext,
        ) {
            let (handle, view) = open(cx);
            let (sender, receiver) = std::sync::mpsc::sync_channel(16);
            cx.update_window(handle, |_, window, cx| {
                let id = view.update(cx, |view, cx| {
                    let (id, _) = local_interaction_fixture(view, window, cx);
                    view.validation_worker_commands = Some(sender);
                    view.settings.permissions = PermissionMode::Plan;
                    view.turns.get_mut(&view.session).unwrap().apply(EngineEvent::PlanRequired { action_id: id, plan: "Explicit plan".into() });
                    view.event(super::HostEvent::PlanModes {session: view.session, action_id: id, modes: vec![PermissionMode::FullAccess], scheduled: false}, window, cx);
                    id
                });
                window.render_frame(cx);
                window.click(("start-plan", 0usize), cx);
                assert!(receiver.try_recv().is_err(), "A disallowed current mode never chooses a more privileged mode");
                window.click(("plan-permissions", 0usize), cx);
                window.render_frame(cx);
                window.click("plan-mode-0-0", cx);
                window.render_frame(cx);
                assert_eq!(view.read(cx).interaction_plan_modes.borrow().get(&(view.read(cx).session, id)).copied(), Some(PermissionMode::FullAccess), "Native permission choice must update the plan");

                assert_ne!(window.find(("start-plan", 0usize)).disabled(), Some(true), "Chosen mode must enable start");
                for _ in 0..100 {
                    if window.find(("start-plan", 0usize)).focused() == Some(true) { break; }
                    window.focus_next(cx); window.render_frame(cx);
                }
                window.press("enter", cx);
                assert!(matches!(receiver.try_recv().unwrap().1, super::WorkerCommand::Answer { action_id, answer: xylo_core::InteractionAnswer::ApprovePlan {mode: PermissionMode::FullAccess} } if action_id == id));
                view.update(cx, |view, cx| {
                    view.event(super::HostEvent::InteractionReply {session: view.session, action_id: id, result: Err("Retry".into())}, window, cx);
                    view.event(super::HostEvent::PlanModes {session: view.session, action_id: id, modes: vec![PermissionMode::FullAccess], scheduled: true}, window, cx);
                });
                window.render_frame(cx);
                assert!(window.find(("plan-permissions", 0usize)).visible());
                window.click(("start-plan", 0usize), cx);
                assert!(receiver.try_recv().is_err());
            }).unwrap();
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
                assert!(send.bottom() - input.top() <= px(98.), "The approved composer reserves 53px input, an 8px gap and 36px controls: input={input:?}, send={send:?}");
                assert!(window.find("composer-context").visible());
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
                assert_eq!(window.find("composer-context").label(), Some("Context 17%. View context details"));
                assert!(window.find("usage-totals-button").visible());
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
                for id in ["composer-context-actions","composer-permissions","composer-model","record-voice-composer","send-prompt"] {
                    let item=window.find(id).bounds();
                    assert!(item.left()>=right && item.right()<=surface.right()+px(4.),"{id} clips or overlaps in the actual split workspace: {item:?}, surface {surface:?}");right=item.right();
                }
                let context=window.find("composer-context").bounds();
                assert!(context.left()>=surface.left()-px(4.) && context.right()<=surface.right()+px(4.));
                assert!(context.top()>=window.find("send-prompt").bounds().bottom());
            }).unwrap();
        }
        #[gpui_kit::test]
        fn large_model_catalog_keeps_rows_readable_and_keyboard_choices_visible(
            cx: &mut TestAppContext,
        ) {
            let (handle, view) = open(cx);
            cx.update_window(handle, |_, window, cx| {
                cx.set_reduce_motion(true);
                view.update(cx, |view, cx| {
                    model(view, cx);
                    view.settings.providers = (0..32)
                        .map(|index| ProviderConfig {
                            name: if index < 20 { "Alpha" } else { "Beta" }.into(),
                            endpoint: format!(
                                "https://{}.example.invalid/v1",
                                if index < 20 { "alpha" } else { "beta" }
                            ),
                            model: format!("fixture-model-{index}"),
                            ..view.settings.provider.clone()
                        })
                        .collect();
                    view.settings.provider = view.settings.providers[0].clone();
                    view.settings.ui.recent_models = [0, 20]
                        .map(|index| xylo_core::RecentModel {
                            endpoint: view.settings.providers[index].endpoint.clone(),
                            model: view.settings.providers[index].model.clone(),
                        })
                        .into();
                    cx.notify();
                });
                for height in [880., 480.] {
                    window.resize(size(px(1000.), px(height)));
                    window.bounds_changed(cx);
                    window.render_frame(cx);
                    window.click("composer-model", cx);
                    window.render_frame(cx);
                    let panel = window.find("composer-model-panel").bounds();
                    let list = window.find("composer-model-list").bounds();
                    let search = window.find_by_label("Search models").bounds();
                    let footer = window.find("composer-manage-models").bounds();
                    assert!(
                        panel.top() >= px(0.) && panel.bottom() <= px(height),
                        "Picker must fit the viewport: {panel:?}"
                    );
                    assert!(
                        list.top() >= search.bottom() && list.bottom() <= footer.top(),
                        "The list must leave search and settings reachable"
                    );
                    let first = window.find(("composer-model-recent", 0usize)).bounds();
                    assert!(
                        first.size.height >= px(28.),
                        "Catalog size must not compress model rows: {first:?}"
                    );
                    // Moving backwards wraps to the final model, then scrolls
                    // each preceding choice fully into view before Enter.
                    for index in (0usize..32).rev() {
                        window.press("up", cx);
                        window.render_frame(cx);
                        let row = window.find(("composer-model-choice", index)).bounds();
                        let list = window.find("composer-model-list").bounds();
                        assert!(
                            row.size.height >= px(28.),
                            "Rows must remain readable after scrolling: {row:?}"
                        );
                        assert!(
                            row.top() >= list.top() && row.bottom() <= list.bottom(),
                            "Keyboard choice {index} is clipped: {row:?}, list={list:?}"
                        );
                    }
                    window.press("enter", cx);
                    assert!(!view.read(cx).model_picker.open);
                    assert_eq!(view.read(cx).settings.provider.model, "fixture-model-0");
                }
            })
            .unwrap();
        }

        #[gpui_kit::test]
        fn full_context_receipt_fits_a_short_window_and_scrolls(cx: &mut TestAppContext) {
            let (handle, view) = open(cx);
            cx.update_window(handle, |_, window, cx| {
                // Geometry assertions compare stable surfaces, not the
                // intentional few-pixel popover entrance animation.
                cx.set_reduce_motion(true);
                view.update(cx, |view, cx| {
                    model(view, cx);
                    let turn = view.turns.entry(view.session).or_default();
                    turn.context = Some(xylo_core::ContextUsage {
                        capacity: 8_192,
                        system: 1_700,
                        tools: 2_000,
                        tool_count: 8,
                        deferred_tool_count: 67,
                        messages: 475,
                        summary: 1_200,
                        compacted: true,
                        autocompact_threshold: 7_456,
                        ..Default::default()
                    });
                    turn.rate_limits = Some((
                        "fixture-model".into(),
                        xylo_core::RateLimits {
                            requests: Some(3_999),
                            tokens: Some(2_397_000),
                            input_tokens: Some(1_997_000),
                            output_tokens: Some(400_000),
                            observed_at_ms: super::now_ms(),
                        },
                    ));
                    view.usage_ui.report =
                        Some((view.session, xylo_runtime::UsageReport::default()));
                    cx.notify();
                });
                window.resize(size(px(1000.), px(480.)));
                window.bounds_changed(cx);
                window.render_frame(cx);
                window.click("composer-context", cx);
                window.render_frame(cx);
                let panel = window.find("composer-context-panel").bounds();
                let content = window.find("composer-context-content").bounds();
                assert!(
                    panel.top() >= px(0.) && panel.bottom() <= px(480.),
                    "A full context and quota receipt must fit the viewport: {panel:?}"
                );
                assert!(
                    panel.size.height <= px(440.),
                    "The popover must leave an outer margin: {panel:?}"
                );
                window.scroll(
                    "composer-context-panel",
                    gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(-600.))),
                    cx,
                );
                window.render_frame(cx);
                let after = window.find("composer-context-panel").bounds();
                assert_eq!(panel, after, "Scrolling details must not move the popover");
                let scrolled = window.find("composer-context-content").bounds();
                assert!(
                    scrolled.top() < content.top(),
                    "The overflow must actually scroll"
                );
                assert!(
                    scrolled.bottom() <= panel.bottom(),
                    "The end of the receipt must be reachable"
                );
                window.press("escape", cx);
                view.update(cx, |view, cx| {
                    view.usage_ui.report_error =
                        Some("Usage details could not be refreshed. ".repeat(30));
                    cx.notify();
                });
                window.render_frame(cx);
                window.click("usage-totals-button", cx);
                let panel = window.find("usage-details").bounds();
                let content = window.find("usage-content").bounds();
                assert!(
                    panel.top() >= px(0.) && panel.bottom() <= px(480.),
                    "Standalone usage details must also fit: {panel:?}"
                );
                window.scroll(
                    "usage-details",
                    gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(-2_000.))),
                    cx,
                );
                let scrolled = window.find("usage-content").bounds();
                assert!(
                    scrolled.top() < content.top() && scrolled.bottom() <= panel.bottom(),
                    "All usage and quota details must remain reachable"
                );
            })
            .unwrap();
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
        fn composer_reverse_tab_preserves_permissions(cx: &mut TestAppContext) {
            let (handle, view) = open(cx);
            for preset in PermissionMode::ALL {
                let (preceding, settings) = cx
                    .update_window(handle, |_, window, cx| {
                        view.update(cx, |view, cx| {
                            model(view, cx);
                            view.set_permission_mode(preset, cx);
                            view.composer.update(cx, |input, cx| {
                                input.set_value("Keep this draft", window, cx);
                                input.focus(window, cx);
                            });
                        });
                        window.render_frame(cx);
                        let composer = window.focused(cx);
                        window.focus_prev(cx);
                        let preceding = window.focused(cx);
                        assert_ne!(
                            preceding, composer,
                            "Composer must have a preceding tab stop"
                        );
                        view.update(cx, |view, cx| {
                            view.composer
                                .update(cx, |input, cx| input.focus(window, cx))
                        });
                        let settings = serde_json::to_value(&view.read(cx).settings).unwrap();
                        window.render_frame(cx);
                        window.press("shift-tab", cx);
                        (preceding, settings)
                    })
                    .unwrap();
                cx.run_until_parked();
                cx.update_window(handle, |_, window, cx| {
                    window.render_frame(cx);
                    assert_eq!(window.focused(cx), preceding);
                    assert_eq!(
                        serde_json::to_value(&view.read(cx).settings).unwrap(),
                        settings
                    );
                    assert_eq!(view.read(cx).current_permission_mode(), preset);
                    assert_eq!(view.read(cx).settings.permissions, preset);
                    assert_eq!(
                        view.read(cx).composer.read(cx).value().as_str(),
                        "Keep this draft"
                    );
                })
                .unwrap();
            }
        }

        #[gpui_kit::test]
        fn composer_toolbar_tabs_both_directions_and_escape_closes_popovers(
            cx: &mut TestAppContext,
        ) {
            let (handle, view) = open(cx);
            cx.update_window(handle, |_, window, cx| {
                cx.set_reduce_motion(true);
                view.update(cx, |view, cx| {
                    model(view, cx);
                    view.composer.update(cx, |input, cx| {
                        input.set_value("Keep this draft", window, cx);
                        input.focus(window, cx);
                    });
                });
                window.render_frame(cx);
                for id in [
                    "composer-context-actions",
                    "composer-permissions",
                    "composer-model",
                ] {
                    window.focus_next(cx);
                    window.render_frame(cx);
                    window.render_frame(cx);
                    assert_eq!(window.find(id).focused(), Some(true), "{id}");
                }
                for id in ["composer-permissions", "composer-context-actions"] {
                    window.press("shift-tab", cx);
                    window.render_frame(cx);
                    assert_eq!(window.find(id).focused(), Some(true), "{id}");
                }
                window.press("shift-tab", cx);
                assert!(
                    view.read(cx)
                        .composer
                        .read(cx)
                        .focus_handle(cx)
                        .contains_focused(window, cx)
                );
            })
            .unwrap();
            for id in [
                "composer-context-actions",
                "composer-permissions",
                "composer-model",
            ] {
                cx.update_window(handle, |_, window, cx| window.click(id, cx))
                    .unwrap();
                cx.run_until_parked();
                let panel = if id == "composer-model" {
                    "composer-model-panel"
                } else {
                    "popup-menu"
                };
                cx.update_window(handle, |_, window, cx| {
                    window.render_frame(cx);
                    assert!(window.try_find(panel).is_some());
                    // Native menus use arrows to traverse their choices.
                    window.press("down", cx);
                    window.press("up", cx);
                    window.press("escape", cx);
                })
                .unwrap();
                cx.run_until_parked();
                cx.update_window(handle, |_, window, cx| {
                    window.render_frame(cx);
                    assert!(window.try_find(panel).is_none(), "Escape closes {id}");
                    assert_eq!(
                        view.read(cx).composer.read(cx).value().as_str(),
                        "Keep this draft"
                    );
                })
                .unwrap();
            }
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
        fn review_populated_panel_controls_fit_minimum_width(cx: &mut TestAppContext) {
            use xylo_runtime::workspace::git::{GitChange, GitStatus};
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.settings.ui.sidebar_visible = false;
                    view.settings.ui.work_panel_open = true;
                    view.settings.ui.work_panel = "details".into();
                    view.settings.ui.work_panel_width = 320.;
                    view.workspace = Some("/fixture".into());
                    view.git_panel = super::work_panel::GitPanel::prepare(
                        "/fixture".into(),
                        GitStatus {
                            branch: "main".into(),
                            truncated: false,
                            changes: vec![GitChange {
                                path: "src/chat.rs".into(),
                                original: None,
                                index: ' ',
                                worktree: 'M',
                            }],
                        },
                    );
                    view.turns.entry(view.session).or_default().shell_jobs.push(
                        xylo_core::ShellJob {
                            action_id: None,
                            background: true,
                            id: uuid::Uuid::new_v4(),
                            turn_id: uuid::Uuid::new_v4(),
                            command: "cargo test".into(),
                            cwd: "/fixture".into(),
                            log_path: "/fixture/log".into(),
                            running: true,
                            exit_code: None,
                            output: String::new(),
                            error: None,
                        },
                    );
                    cx.notify();
                });
                window.render_frame(cx);
                let pane = window.find("native-work-pane").bounds();
                for id in [
                    "work-tab-details",
                    "work-tab-changes",
                    "work-tab-jobs",
                    "close-work-panel",
                ] {
                    assert!(
                        window.find(id).bounds().right() <= pane.right(),
                        "Control exceeds the panel: {id}"
                    );
                }
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn review_open_activity_refreshes_after_file_edit(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.settings.ui.work_panel_open = true;
                    view.settings.ui.work_panel = "details".into();
                    view.workspace = Some("/fixture".into());
                    view.engine_event(
                        view.session,
                        EngineEvent::ToolFinished {
                            parent_id: None,
                            action_id: uuid::Uuid::new_v4(),
                            name: "file_edit".into(),
                            result: "{}".into(),
                            success: true,
                        },
                        window,
                        cx,
                    );
                    assert!(
                        view.git_panel.request.is_some(),
                        "The visible file list must refresh after an edit"
                    );
                })
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn transcript_subagent_result_is_compact_and_opens_its_output(cx: &mut TestAppContext) {
            let (handle, view) = open(cx);
            for width in [420., 1000.] {
                let id = uuid::Uuid::new_v4();
                cx.update_window(handle, |_, window, cx| {
                    window.resize(size(px(width), px(740.)));
                    window.bounds_changed(cx);
                    view.update(cx, |view, cx| {
                        view.settings.ui.sidebar_visible = false;
                        view.settings.ui.work_panel_open = false;
                        view.messages.clear();
                        view.turns.remove(&view.session);
                        view.composer.update(cx, |input, cx| input.set_value("Keep this draft", window, cx));
                        let turn = view.turns.entry(view.session).or_default();
                        turn.apply(EngineEvent::Started);
                        turn.apply(EngineEvent::ToolStarted {
                            action_id: id, name: "agent_delegate".into(),
                            arguments: serde_json::json!({"task":"Read-only code review of chat rendering and subagent result presentation"}), parent_id: None,
                        });
                        turn.apply(EngineEvent::ToolFinished {
                            action_id: id, name: "agent_delegate".into(),
                            result: serde_json::json!({"summary":"Review complete. The detailed result is available here."}).to_string(),
                            success: true, parent_id: None,
                        });
                        view.invalidate_live_history();
                        view.sync_history_list(cx);
                        cx.notify();
                    });
                    window.render_frame(cx);
                    let row = window.find(format!("subagent-row-{id}")).bounds();
                    let action = window.find(("subagent-open-result", 0usize)).bounds();
                    assert!(action.size.width < px(130.), "Result action stretched to {}", action.size.width);
                    assert!(action.left() - row.left() < px(30.));
                    assert!(action.right() <= row.right());
                    window.click(("subagent-open-result", 0usize), cx);
                    assert!(view.read(cx).settings.ui.work_panel_open);
                    assert!(view.read(cx).expanded_tools.contains(&uuid::Uuid::new_v5(&id, b"panel-output")));
                    assert_eq!(view.read(cx).composer.read(cx).value().as_str(), "Keep this draft");
                }).unwrap();
            }
        }

        #[gpui_kit::test]
        fn task_results_expand_in_place_without_changing_the_draft(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            let id = uuid::Uuid::new_v4();
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.settings.ui.sidebar_visible = false;
                    view.settings.ui.work_panel_open = true;
                    view.settings.ui.work_panel = "details".into();
                    view.settings.ui.work_panel_width = 420.;
                    view.composer
                        .update(cx, |input, cx| input.set_value("Keep my draft", window, cx));
                    view.turns.entry(view.session).or_default().subagents.push(
                        crate::state::Subagent {
                            id,
                            task: "Review the composer".into(),
                            tools: vec![],
                            summary: Some("A real result with details. ".repeat(90).into()),
                            state: crate::state::SubagentState::Completed,
                            started_at: 1000,
                            finished_at: Some(2000),
                        },
                    );
                    cx.notify();
                });
                window.render_frame(cx);
                assert!(window.find(format!("subagent-result-{id}")).visible());
                assert!(window.try_find("task-details-empty").is_none());
                for tab in [
                    "work-tab-files",
                    "work-tab-terminal",
                    "work-tab-jobs",
                    "work-tab-changes",
                ] {
                    assert!(
                        window.try_find(tab).is_none(),
                        "No unrelated empty tabs: {tab}"
                    );
                }
                let before = window.find("native-chat-pane").bounds();
                window.click(format!("subagent-output-toggle-{id}"), cx);
                window.render_frame(cx);
                assert_eq!(window.find("native-chat-pane").bounds(), before);
                assert_eq!(
                    view.read(cx).composer.read(cx).value().as_str(),
                    "Keep my draft"
                );
                assert!(
                    view.read(cx)
                        .expanded_tools
                        .contains(&uuid::Uuid::new_v5(&id, b"panel-output"))
                );
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn file_changes_open_full_width_and_comments_use_original_patch_lines(
            cx: &mut TestAppContext,
        ) {
            use xylo_runtime::workspace::git::{GitChange, GitStatus};
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.workspace = Some("/fixture".into());
                    view.settings.ui.sidebar_visible = false;
                    view.settings.ui.work_panel_open = true;
                    view.settings.ui.work_panel = "changes".into();
                    view.settings.ui.work_panel_width = 600.;
                    view.git_panel = super::work_panel::GitPanel::prepare("/fixture".into(), GitStatus {
                        branch: "main".into(), truncated: false, changes: vec![GitChange {
                            path: "src/chat.rs".into(), original: None, index: 'M', worktree: 'M',
                        }],
                    });
                    view.composer.update(cx, |input, cx| input.set_value("My draft", window, cx));
                    cx.notify();
                });
                window.render_frame(cx);
                assert!(window.find(("changed-file", 0usize)).visible());
                assert!(window.try_find(("changed-file", 1usize)).is_none(), "Staged and working versions share one file row");
                assert!(window.try_find("git-diff-pane").is_none());
                window.click(("changed-file", 0usize), cx);
                view.update(cx, |view, cx| {
                    let request = view.git_panel.diff_request.unwrap();
                    view.git_diff_received(request, "/fixture".into(), Ok("diff --git a/src/chat.rs b/src/chat.rs\nindex a..b 100644\n--- a/src/chat.rs\n+++ b/src/chat.rs\n@@ -1,2 +1,2 @@\n same\n-old\n+new\n".into()), cx);
                });
                window.render_frame(cx);
                assert!(window.try_find("changed-files-list").is_none());
                let pane = window.find("git-diff-pane").bounds();
                let work = window.find("native-work-pane").bounds();
                assert!(pane.size.width >= work.size.width - px(2.));
                assert!(window.find("switch-change-version").visible());
                assert_eq!(window.find(("comment-line", 7usize)).label(), Some("Comment on line 2 (new)"));
                window.hover(("diff-line", 7usize), cx);
                window.render_frame(cx);
                window.click(("comment-line", 7usize), cx);
                assert!(view.read(cx).composer.read(cx).value().starts_with("My draft\n\nReview src/chat.rs:2"));
                assert!(!view.read(cx).sending_turn());
                window.click("back-to-changed-files", cx);
                window.render_frame(cx);
                assert!(window.find(("changed-file", 0usize)).visible());
                assert!(window.try_find("git-diff-pane").is_none());
            }).unwrap();
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
                assert!(
                    window.try_find("git-diff-pane").is_none(),
                    "An empty changes panel must not reserve an empty diff column"
                );
                assert!(window.find("file-changes-empty").visible());
                assert!(window.try_find("work-tab-terminal").is_none());
                assert!(window.try_find("work-tab-jobs").is_none());
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
        fn queue_restore_ack_clears_undo_only_after_success(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                let item = uuid::Uuid::new_v4();
                view.update(cx, |view, cx| {
                    model(view, cx);
                    view.error = None;
                    view.settings_open = false;
                    view.validation_draft_surface(None, false, window, cx);
                    view.validation_queue_restore_ack(
                        view.session,
                        item,
                        Err("queue full".into()),
                        window,
                        cx,
                    );
                });
                window.render_frame(cx);
                assert!(
                    window.find("undo-remove-queued").visible(),
                    "Failure must retain Undo for retry"
                );
                view.update(cx, |view, cx| {
                    view.validation_queue_restore_ack(
                        uuid::Uuid::new_v4(),
                        uuid::Uuid::new_v4(),
                        Ok(vec![]),
                        window,
                        cx,
                    );
                });
                window.render_frame(cx);
                assert!(
                    window.find("undo-remove-queued").visible(),
                    "Another chat's acknowledgement must preserve this Undo"
                );
                view.update(cx, |view, cx| {
                    view.validation_queue_restore_ack(view.session, item, Ok(vec![]), window, cx);
                });
                window.render_frame(cx);
                assert!(
                    window.try_find("undo-remove-queued").is_none(),
                    "Successful matching restore must clear the removed state"
                );
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn two_blocked_queue_edits_keep_both_recovery_paths(cx: &mut TestAppContext) {
            let (handle, view) = open(cx);
            cx.update_window(handle, |_, window, cx| {
                let ids = view.update(cx, |view, cx| {
                    model(view, cx);
                    view.workspace = Some("/fixture".into());
                    view.validation_draft_surface(None, true, window, cx);
                    let ids = [uuid::Uuid::new_v4(), uuid::Uuid::new_v4()];
                    for (index, id) in ids.into_iter().enumerate() {
                        let item = xylo_runtime::turn_queue::QueuedTurn {
                            id, paused: false, reason: None,
                            turn: xylo_core::TurnRequest {
                                session_id: view.session, turn_id: id,
                                workspace: view.workspace.clone().unwrap(),
                                prompt: format!("Keep queued message {index}"), attachments: vec![],
                                provider: view.settings.provider.clone(), permissions: view.settings.permissions,
                            },
                        };
                        view.validation_queue_edit_ack(item, Ok(vec![]), window, cx);
                    }
                    for id in ids { assert!(view.validation_recovered_queue_ids().contains(&id)); }
                    assert_eq!(view.composer.read(cx).value().as_ref(), "Keep the approved colors and refine the composer spacing.");
                    ids
                });
                window.render_frame(cx);
                for id in ids { assert!(window.find(gpui_kit::ElementId::Name(format!("recover-queue-edit-{id}").into())).visible()); }
            }).unwrap();
        }
        #[gpui_kit::test]
        fn queue_edit_start_race_recovers_payload_without_submission(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    model(view, cx);
                    view.workspace = Some(std::path::PathBuf::from("/fixture"));
                    let (_, image) = local_interaction_fixture(view, window, cx);
                    let session = view.session;
                    let id = uuid::Uuid::new_v4();
                    let item = xylo_runtime::turn_queue::QueuedTurn {
                        id,
                        paused: false,
                        reason: None,
                        turn: xylo_core::TurnRequest {
                            session_id: session,
                            turn_id: id,
                            workspace: view.workspace.clone().unwrap(),
                            prompt: "Recover complete queued input".into(),
                            attachments: vec![image.clone()],
                            provider: view.settings.provider.clone(),
                            permissions: view.settings.permissions,
                        },
                    };
                    view.validation_queue_edit_ack(
                        item,
                        Err("Message already started; stop the active task".into()),
                        window,
                        cx,
                    );
                    let recovered = view.validation_recovered_queue_edit(session).unwrap();
                    assert_eq!(recovered.text, "Recover complete queued input");
                    assert_eq!(recovered.attachments, vec![image]);
                    assert!(view.running(), "Editing must preserve the existing task");
                    assert_eq!(
                        view.messages.len(),
                        1,
                        "Editing never appends an automatic duplicate submission"
                    );
                    assert!(
                        view.validation_recovered_queue_edit(uuid::Uuid::new_v4())
                            .is_none()
                    );
                });
                window.render_frame(cx);
                assert!(window.find("queue-edit-recovery").visible());
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn queued_guidance_receipts_update_live_history_without_touching_main_draft(
            cx: &mut TestAppContext,
        ) {
            let (handle, view) = open(cx);
            cx.update_window(handle, |_, window, cx| {
                let (id, mut outcome) = view.update(cx, |view, cx| {
                    model(view, cx);
                    view.workspace = Some("/fixture".into());
                    view.validation_draft_surface(None, false, window, cx);
                    view.composer.update(cx, |input, cx| {
                        input.set_value("Unrelated main draft", window, cx)
                    });
                    let id = uuid::Uuid::new_v4();
                    let active = uuid::Uuid::new_v4();
                    let item = xylo_runtime::turn_queue::QueuedTurn {
                        id,
                        paused: false,
                        reason: None,
                        turn: xylo_core::TurnRequest {
                            session_id: view.session,
                            turn_id: id,
                            workspace: view.workspace.clone().unwrap(),
                            prompt: "Queued task direction".into(),
                            attachments: vec![],
                            provider: view.settings.provider.clone(),
                            permissions: view.settings.permissions,
                        },
                    };
                    view.validation_track_queued_guidance(item);
                    let outcome = xylo_runtime::steering::SteeringOutcome {
                        request: xylo_core::SteeringRequest {
                            id,
                            session_id: view.session,
                            turn_id: active,
                            text: "Queued task direction".into(),
                        },
                        receipt: xylo_core::SteeringReceipt {
                            id,
                            session_id: view.session,
                            turn_id: active,
                            state: xylo_core::SteeringState::Queued,
                            reason: None,
                        },
                        draft: xylo_runtime::drafts::DraftSnapshot {
                            key: xylo_runtime::drafts::DraftKey {
                                session_id: view.session,
                                workspace: view.workspace.clone(),
                            },
                            text: "Unrelated main draft".into(),
                            attachments: vec![],
                            revision: 7,
                            updated_at: 1,
                        },
                        cleared_revision: None,
                    };
                    view.steering_result(id, Ok(outcome.clone()), window, cx);
                    (id, outcome)
                });
                window.render_frame(cx);
                assert!(
                    window
                        .find(gpui_kit::SharedString::from(format!(
                            "pending-queued-guidance-{id}"
                        )))
                        .visible()
                );
                outcome.receipt.state = xylo_core::SteeringState::Applied;
                view.update(cx, |view, cx| {
                    view.steering_result(id, Ok(outcome.clone()), window, cx);
                    view.steering_result(id, Ok(outcome.clone()), window, cx);
                    assert_eq!(
                        view.composer.read(cx).value().as_ref(),
                        "Unrelated main draft"
                    );
                    assert_eq!(
                        view.messages
                            .iter()
                            .filter(|message| message.content == "Queued task direction")
                            .count(),
                        1
                    );
                });
                window.render_frame(cx);
                assert!(
                    window
                        .try_find(gpui_kit::SharedString::from(format!(
                            "pending-queued-guidance-{id}"
                        )))
                        .is_none()
                );
                view.update(cx, |view, cx| {
                    let failed = uuid::Uuid::new_v4();
                    let item = xylo_runtime::turn_queue::QueuedTurn {
                        id: failed,
                        paused: false,
                        reason: None,
                        turn: xylo_core::TurnRequest {
                            session_id: view.session,
                            turn_id: failed,
                            workspace: view.workspace.clone().unwrap(),
                            prompt: "Queued task direction".into(),
                            attachments: vec![],
                            provider: view.settings.provider.clone(),
                            permissions: view.settings.permissions,
                        },
                    };
                    view.validation_track_queued_guidance(item);
                    outcome.request.id = failed;
                    outcome.receipt.id = failed;
                    outcome.receipt.state = xylo_core::SteeringState::Cancelled;
                    outcome.receipt.reason =
                        Some("The task ended before guidance was added".into());
                    view.steering_result(failed, Ok(outcome.clone()), window, cx);
                    assert_eq!(
                        view.composer.read(cx).value().as_ref(),
                        "Unrelated main draft"
                    );
                });
                window.render_frame(cx);
                assert!(window.find("queue-notice").visible());
            })
            .unwrap();
        }

        #[gpui_kit::test]
        fn six_queued_messages_remain_reviewable(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    model(view, cx);
                    view.error = None;
                    view.settings_open = false;
                    view.workspace = Some(std::path::PathBuf::from("/fixture"));
                    view.validation_draft_surface(None, false, window, cx);
                    let items = (0..6)
                        .map(|index| {
                            let id = uuid::Uuid::new_v4();
                            xylo_runtime::turn_queue::QueuedTurn {
                                id,
                                paused: true,
                                reason: Some("Previous task stopped".into()),
                                turn: xylo_core::TurnRequest {
                                    session_id: view.session,
                                    turn_id: id,
                                    workspace: view.workspace.clone().unwrap(),
                                    prompt: format!(
                                        "Message {index}: {}",
                                        "Complete long payload. ".repeat(30)
                                    ),
                                    provider: view.settings.provider.clone(),
                                    permissions: view.settings.permissions,
                                    attachments: vec![xylo_core::ImageAttachment {
                                        id: uuid::Uuid::new_v4(),
                                        session_id: view.session,
                                        name: format!("image-{index}.png"),
                                        media_type: "image/png".into(),
                                        sha256: "a".repeat(64),
                                        bytes: 16,
                                        width: 2,
                                        height: 2,
                                        source: xylo_core::AttachmentSource::Picker,
                                        imported_at: 1,
                                    }],
                                },
                            }
                        })
                        .collect();
                    view.turn_queue.items.insert(view.session, items);
                    view.composer.update(cx, |input, cx| {
                        input.set_value("Unrelated main draft", window, cx)
                    });
                    cx.notify();
                });
                window.resize(size(px(1380.), px(880.)));
                window.bounds_changed(cx);
                window.render_frame(cx);
                assert!(window.find("show-all-queued").visible());
                assert!(window.try_find(("preview-queued", 1usize)).is_none());
                let queue = window.find("composer-pending").bounds();
                let paused = window.find("queue-paused").bounds();
                assert!(queue.size.height <= px(135.));
                assert!(
                    paused.top() >= queue.bottom(),
                    "Pause reason must remain outside scrolling rows"
                );
                assert!(window.find("resume-queue").visible());
                // Reach Review through native focus traversal, then activate with Enter.
                for _ in 0..100 {
                    if window.find(("preview-queued", 0usize)).focused() == Some(true) {
                        break;
                    }
                    window.focus_next(cx);
                    window.render_frame(cx);
                }
                assert_eq!(
                    window.find(("preview-queued", 0usize)).focused(),
                    Some(true)
                );
                window.press("enter", cx);
                assert!(window.find("queued-message-dialog").visible());
                use gpui_kit::component::WindowExt;
                window.close_dialog(cx);
                window.render_frame(cx);
                window.click("show-all-queued", cx);
                window.render_frame(cx);
                for index in 0..6usize {
                    window.scroll(
                        "composer-pending",
                        gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(10000.))),
                        cx,
                    );
                    window.render_frame(cx);
                    for _ in 0..100 {
                        let target = window.find(("preview-queued", index)).bounds();
                        let viewport = window.find("composer-pending").bounds();
                        if target.top() >= viewport.top() && target.bottom() <= viewport.bottom() {
                            break;
                        }
                        window.scroll(
                            "composer-pending",
                            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(-40.))),
                            cx,
                        );
                        window.render_frame(cx);
                    }
                    window.click(("preview-queued", index), cx);
                    window.render_frame(cx);
                    assert!(window.find("queued-message-dialog").visible());
                    assert!(window.find("edit-queued-dialog").visible());
                    let image_id = view.read(cx).turn_queue.items[&view.read(cx).session][index]
                        .turn
                        .attachments[0]
                        .id;
                    assert!(
                        window
                            .try_find(gpui_kit::SharedString::from(format!(
                                "image-preview-{image_id}"
                            )))
                            .is_some()
                    );
                    window.close_dialog(cx);
                    window.render_frame(cx);
                    assert!(window.find("resume-queue").visible());
                }
                {
                    let view = view.read(cx);
                    assert_eq!(
                        view.composer.read(cx).value().as_ref(),
                        "Unrelated main draft"
                    );
                    assert_eq!(view.turn_queue.items.get(&view.session).unwrap().len(), 6);
                    assert!(
                        view.turn_queue
                            .items
                            .get(&view.session)
                            .unwrap()
                            .iter()
                            .all(|item| item.turn.attachments.len() == 1
                                && item.turn.prompt.contains("Complete long payload."))
                    );
                }
            })
            .unwrap();
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
                    let status = window.find("composer-status-line").bounds();
                    assert!(
                        queue.bottom() <= composer.top(),
                        "Pending messages belong above the composer"
                    );
                    assert!(
                        composer.bottom() <= status.top(),
                        "The status row belongs below the composer"
                    );
                    assert!(
                        status.bottom() <= px(height) && status.bottom() >= px(height - 35.),
                        "Conversation composer and status stay bottom anchored"
                    );
                    assert!(window.find(("remove-queued", 0usize)).visible());
                    assert!(
                        window.try_find("saved-drafts").is_none(),
                        "Routine queued status must not add a duplicate footer"
                    );
                    assert!(window.find("steer-prompt").visible());
                    assert_eq!(window.find("send-prompt").label(), Some("Send next"));
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
        fn empty_model_picker_has_one_compact_setup_action(cx: &mut TestAppContext) {
            let (handle, view) = open(cx);
            cx.update_window(handle, |_, window, cx| {
                cx.set_reduce_motion(true);
                view.update(cx, |view, cx| {
                    view.settings.providers.clear();
                    view.set_model_picker_open(true, window, cx);
                });
                window.render_frame(cx);
                assert!(window.find("composer-connect-provider").visible());
                assert!(window.try_find("composer-manage-models").is_none());
                assert!(window.find_all_by_label("Search models").is_empty());
                assert!(window.find("composer-model-panel").bounds().size.height <= px(40.));
                window.press("escape", cx);
                window.render_frame(cx);
                assert!(!view.read(cx).model_picker.open);
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn live_activity_hides_empty_reasoning_and_keeps_disclosures_compact(
            cx: &mut TestAppContext,
        ) {
            let (handle, view) = open(cx);
            cx.update_window(handle, |_, window, cx| {
                cx.set_reduce_motion(true);
                view.update(cx, |view, cx| {
                    model(view,cx);
                    let turn=view.turns.entry(view.session).or_default();
                    turn.apply(EngineEvent::Started);
                    turn.apply(EngineEvent::ThinkingDelta{text:" \n\t".into()});
                    view.invalidate_live_history();view.sync_history_list(cx);cx.notify();
                });
                window.render_frame(cx);
                assert!(window.find_all_by_label("Approach. Show details").is_empty());
                let key=view.update(cx, |view,cx| {
                    let turn=view.turns.get_mut(&view.session).unwrap();
                    turn.apply(EngineEvent::Delta{text:"Checking the project.".into()});
                    for (name,args) in [("files_list",serde_json::json!({"path":"."})),("file_read",serde_json::json!({"path":"README.md"}))] {
                        let id=uuid::Uuid::new_v4();
                        turn.apply(EngineEvent::ToolStarted{action_id:id,name:name.into(),arguments:args,parent_id:None});
                        turn.apply(EngineEvent::ToolFinished{action_id:id,name:name.into(),result:"{}".into(),success:true,parent_id:None});
                    }
                    turn.apply(EngineEvent::ThinkingDelta{text:"Review the existing components.".into()});
                    turn.apply(EngineEvent::Delta{text:"The controls need a spacing pass.".into()});
                    turn.apply(EngineEvent::ThinkingDelta{text:"Then verify the narrow layout.".into()});
                    let first=turn.segments.iter().position(|s|matches!(s,crate::state::Segment::Thinking(t) if !t.trim().is_empty())).unwrap();
                    let key=uuid::Uuid::new_v5(&view.session,format!("thinking-{}-{first}",turn.started_at.unwrap_or_default()).as_bytes());
                    view.invalidate_live_history();view.sync_history_list(cx);cx.notify();key
                });
                window.render_frame(cx);
                assert_eq!(window.find_all_by_label("Approach. Show details").len(),1);
                let toggle=gpui_kit::ElementId::Name(format!("thinking-toggle-{key}").into());
                assert!(window.find(toggle.clone()).bounds().size.width < px(180.));
                assert!(window.find(("tool-group",0usize)).bounds().size.width < px(400.));
                window.click(toggle,cx);window.render_frame(cx);
                assert!(window.find(gpui_kit::ElementId::Name(format!("thinking-body-{key}").into())).visible());
            }).unwrap();
        }
        #[gpui_kit::test]
        fn find_in_chat_enter_never_activates_another_chat(cx: &mut TestAppContext) {
            let (handle, view) = open(cx);
            cx.update_window(handle, |_, window, cx| {
                cx.set_reduce_motion(true);
                let owner = view.read(cx).session;
                view.update(cx, |view, cx| {
                    view.conversations.push(xylo_core::Conversation {
                        id: uuid::Uuid::new_v4(),
                        title: "Other recent chat".into(),
                        workspace: std::path::PathBuf::from("/tmp"),
                        updated_at: 1,
                        title_source: Default::default(),
                        pinned: false,
                        archived: false,
                    });
                    view.open_chat_search(window, cx);
                });
                window.render_frame(cx);
                window.press("enter", cx);
                window.render_frame(cx);
                assert_eq!(view.read(cx).session, owner);
                assert!(window.find("chat-search-results").visible());
                window.press("escape", cx);
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn full_output_is_modal_and_escape_preserves_task_and_draft(cx: &mut TestAppContext) {
            let (handle, view) = open(cx);
            cx.update_window(handle, |_, window, cx| {
                cx.set_reduce_motion(true);
                view.update(cx, |view, cx| {
                    model(view, cx);
                    view.composer.update(cx, |input, cx| {
                        input.set_value("Unrelated draft", window, cx)
                    });
                    view.turns
                        .entry(view.session)
                        .or_default()
                        .apply(EngineEvent::Started);
                    view.show_action_output(
                        view.session,
                        uuid::Uuid::new_v4(),
                        "Command output".into(),
                        "retained output\n".repeat(2000),
                        cx,
                    );
                });
                window.render_frame(cx);
                assert!(window.find("chat-full-output").visible());
                let popup = window.find("chat-full-output").bounds();
                assert!(popup.left() >= px(0.) && popup.right() <= window.viewport_size().width);
                window.press("escape", cx);
                window.render_frame(cx);
                assert!(window.try_find("chat-full-output").is_none());
                assert!(view.read(cx).running());
                assert_eq!(
                    view.read(cx).composer.read(cx).value().as_str(),
                    "Unrelated draft"
                );
                assert!(
                    !view
                        .read(cx)
                        .turns
                        .get(&view.read(cx).session)
                        .unwrap()
                        .cancel_requested
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
                assert_eq!(window.find("send-prompt").disabled(), Some(true));
                assert_eq!(window.find("steer-prompt").disabled(), Some(true));
                view.update(cx, |view, cx| {
                    view.composer.update(cx, |input, cx| {
                        input.set_value("Adjust this while working", window, cx)
                    })
                });
                window.render_frame(cx);
                assert!(window.try_find("send-prompt").is_some());
                assert_eq!(window.find("send-prompt").label(), Some("Send next"));
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
                let stop = window.find("cancel-task").bounds();
                let task_status = window.find("current-task-status").bounds();
                assert!(
                    stop.left() >= task_status.left()
                        && stop.right() <= task_status.right()
                        && stop.top() >= task_status.top()
                        && stop.bottom() <= task_status.bottom(),
                    "Stop stays in the visible current-task row"
                );
                for id in ["steer-prompt", "send-prompt"] {
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
