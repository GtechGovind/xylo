//! Native controlled state/accessibility and correlated result fixtures. These
//! tests have no HostClient and cannot reach notification permissions or OS APIs.
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
    mod notification_native_tests {
        use super::{Page, Xylo};
        use gpui_kit::{
            AnyWindowHandle, AppContext, Bounds, Context, Entity, Role, TestAppContext,
            WindowBounds, WindowOptions, px, size,
            test::{TestQueryExt, TestWindowExt},
        };
        use serde_json::{Value, json};
        use uuid::Uuid;

        fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Xylo>) {
            cx.update(gpui_kit::init);
            cx.update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Default::default(),
                            size: size(px(1300.), px(1100.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| {
                        cx.new(|cx| {
                            Xylo::new(
                                Err(anyhow::anyhow!("Isolated native notification fixture")),
                                window,
                                cx,
                            )
                        })
                    },
                )
                .unwrap()
            })
        }
        fn snapshot(sequence: u64, enabled: bool, permission: &str) -> Value {
            json!({"sequence":sequence,"loaded":true,"preferences":{"enabled":enabled,"completion":true,"approval":true,"failure":true,"schedule":true,"background_only":true},"permission":permission,"capabilities":{"request_permission":true,"live_activation":true,"cold_activation":false,"retraction":true},"message":null})
        }
        fn response(view: &mut Xylo, name: &str, value: Value, cx: &mut Context<Xylo>) {
            let id = Uuid::new_v4();
            view.notifications.pending.insert(id, name.into());
            assert!(view.notification_result(id, &value, cx));
        }
        #[gpui_kit::test]
        fn loading_and_disabled_preferences_cannot_enable_native_actions(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, _| view.page = Page::Notifications);
                window.render_frame(cx);
                assert_eq!(window.find("notifications-enabled").checked(), Some(false));
                assert_eq!(window.find("notifications-enabled").disabled(), Some(true));
                assert_eq!(window.find("notifications-test").disabled(), Some(true));
                assert!(window.try_find_by_label("Allow notifications").is_none());
                view.update(cx, |view, cx| {
                    response(
                        view,
                        "notifications_status",
                        snapshot(1, false, "not_determined"),
                        cx,
                    )
                });
                window.render_frame(cx);
                assert_ne!(window.find("notifications-enabled").disabled(), Some(true));
                assert_eq!(window.find("notifications-allow").disabled(), Some(true));
                assert_eq!(window.find("notifications-test").disabled(), Some(true));
                assert_eq!(
                    window.find("notifications-completion").disabled(),
                    Some(true)
                );
                assert_eq!(
                    window.find("notifications-enabled").role(),
                    Some(Role::Switch)
                );
                assert_eq!(
                    window
                        .find_by_label("Enable desktop notifications")
                        .checked(),
                    Some(false)
                );
                assert!(view.read(cx).notifications.pending.is_empty());
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn enabled_status_does_not_request_permission_or_send_test(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.page = Page::Notifications;
                    response(
                        view,
                        "notifications_status",
                        snapshot(2, true, "not_determined"),
                        cx,
                    );
                });
                window.render_frame(cx);
                assert_eq!(window.find("notifications-enabled").checked(), Some(true));
                assert_ne!(window.find("notifications-allow").disabled(), Some(true));
                assert_eq!(window.find("notifications-test").disabled(), Some(true));
                assert!(
                    view.read(cx).notifications.pending.is_empty(),
                    "A status response initiated an OS action"
                );
                view.update(cx, |view, cx| {
                    response(
                        view,
                        "notifications_refresh",
                        snapshot(3, true, "denied"),
                        cx,
                    )
                });
                window.render_frame(cx);
                assert!(window.try_find_by_label("Allow notifications").is_none());
                assert_eq!(window.find("notifications-test").disabled(), Some(true));
                assert!(
                    view.read(cx)
                        .notifications
                        .notice
                        .contains("system notification settings")
                );
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn stale_or_uncorrelated_results_cannot_revive_disabled_state_or_replace_notice(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, _, cx| {
                view.update(cx, |view, cx| {
                    response(
                        view,
                        "notifications_status",
                        snapshot(20, false, "denied"),
                        cx,
                    );
                    view.notifications.notice = "Notifications were turned off.".into();
                    response(
                        view,
                        "notifications_test",
                        json!({"snapshot":snapshot(19,true,"allowed"),"outcome":"submitted"}),
                        cx,
                    );
                    assert_eq!(view.notifications.status["preferences"]["enabled"], false);
                    assert_eq!(view.notifications.notice, "Notifications were turned off.");
                    assert!(!view.notification_result(
                        Uuid::new_v4(),
                        &snapshot(21, true, "allowed"),
                        cx
                    ));
                    assert_eq!(view.notifications.sequence, 20);
                    assert!(view.notifications.pending.is_empty());
                })
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn preference_enqueue_failure_keeps_controlled_switch_off(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.page = Page::Notifications;
                    response(
                        view,
                        "notifications_status",
                        snapshot(1, false, "allowed"),
                        cx,
                    );
                });
                window.render_frame(cx);
                window.click("notifications-enabled", cx);
                assert_eq!(window.find("notifications-enabled").checked(), Some(false));
                assert_eq!(
                    view.read(cx).notifications.status["preferences"]["enabled"],
                    false
                );
                assert!(view.read(cx).notifications.pending.is_empty());
                assert!(view.read(cx).error.is_some());
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn cancelled_permission_reply_is_consumed_without_claiming_permission(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                let id = Uuid::new_v4();
                view.update(cx, |view, cx| {
                    view.page = Page::Notifications;
                    response(
                        view,
                        "notifications_status",
                        snapshot(1, true, "not_determined"),
                        cx,
                    );
                    view.notifications
                        .pending
                        .insert(id, "notifications_request_permission".into());
                });
                window.render_frame(cx);
                window.click("notifications-cancel", cx);
                view.update(cx, |view, cx| {
                    assert!(view.notifications.pending.is_empty());
                    assert!(view.notifications.ignored.contains(&id));
                    assert!(view.notification_result(
                        id,
                        &json!({"snapshot":snapshot(2,true,"allowed")}),
                        cx
                    ));
                    assert_eq!(view.notifications.status["permission"], "not_determined");
                    assert!(!view.notifications.ignored.contains(&id));
                });
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn test_acceptance_reports_center_submission_and_is_not_display_evidence(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, _, cx| {
                view.update(cx, |view, cx| {
                    response(
                        view,
                        "notifications_test",
                        json!({"snapshot":snapshot(4,true,"allowed"),"outcome":"submitted"}),
                        cx,
                    );
                    assert!(
                        view.notifications
                            .notice
                            .starts_with("Test sent to your notification center.")
                    );
                    assert!(view.notifications.notice.contains("Focus"));
                    assert!(!view.notifications.notice.contains("displayed"));
                })
            })
            .unwrap();
        }
    }
}
