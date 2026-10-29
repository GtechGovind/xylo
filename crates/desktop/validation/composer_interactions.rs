//! Production composer controls with no live account, inference, or device use.
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
        fn compact_toolbar_places_permissions_left_and_unified_model_right(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| model(view, cx));
                window.render_frame(cx);
                let permission = window.find("composer-permissions").bounds();
                let context = window.find("composer-context").bounds();
                let model = window.find("composer-model").bounds();
                let send = window.find("send-prompt").bounds();
                assert!(
                    permission.origin.x < context.origin.x
                        && context.origin.x < model.origin.x
                        && model.origin.x < send.origin.x
                );
                assert!(
                    window.try_find("composer-reasoning").is_none(),
                    "Effort belongs in the unified control, not another toolbar selector"
                );
                window.click("composer-model", cx);
                assert!(window.try_find("composer-model-options").is_some());
                assert!(window.try_find(("composer-effort", 1usize)).is_some());
                window.click(("composer-effort", 1usize), cx);
                assert_eq!(
                    view.read(cx).settings.provider.reasoning_effort.as_deref(),
                    Some("high")
                );
                window.click("composer-effort-default", cx);
                assert_eq!(view.read(cx).settings.provider.reasoning_effort, None);
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
                    let item=window.find(id).bounds();
                    assert!(item.left()>=right && item.right()<=surface.right()+px(4.),"{id} clips or overlaps in the actual split workspace: {item:?}, surface {surface:?}");right=item.right();
                }
            }).unwrap();
        }
        #[gpui_kit::test]
        fn unified_model_switching_honors_explicit_unsupported_metadata(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| model(view, cx));
                window.render_frame(cx);
                window.click("composer-model", cx);
                window.click("composer-model-options", cx);
                window.click(("composer-model-choice", 1usize), cx);
                assert_eq!(view.read(cx).settings.provider.model, "future-fast");
                window.render_frame(cx);
                assert!(
                    window.try_find(("composer-effort", 0usize)).is_none(),
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
        fn busy_composer_uses_stop_when_empty_and_send_for_new_guidance(cx: &mut TestAppContext) {
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
                assert!(window.try_find("cancel-task").is_none());
            })
            .unwrap();
        }
    }
}
