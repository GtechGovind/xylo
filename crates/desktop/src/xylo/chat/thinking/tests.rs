//! Rendered disclosure behavior for saved reasoning.
use crate::state::message;
use crate::xylo::{Page, Xylo};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AppContext, ElementId, TestAppContext, WindowOptions};
use uuid::Uuid;
use xylo_core::ProviderConfig;

#[gpui_kit::test]
fn saved_thinking_is_hidden_until_clicked_and_can_be_closed(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Isolated fixture")), window, cx))
        })
        .unwrap()
    });
    cx.update_window(window, |_, window, cx| {
        let key = view.update(cx, |view, cx| {
            view.error = None;
            let mut reply = message(view.session, "assistant", "Answer".into());
            reply.reasoning = Some("Consider this".into());
            let key = Uuid::new_v5(&reply.id, b"thinking");
            view.messages.push(reply);
            view.sync_history_list(cx);
            cx.notify();
            key
        });
        let toggle = ElementId::Name(format!("thinking-toggle-{key}").into());
        let body = ElementId::Name(format!("thinking-body-{key}").into());
        window.render_frame(cx);
        assert!(window.try_find(toggle.clone()).is_some());
        assert!(window.try_find(body.clone()).is_none());
        window.click(toggle.clone(), cx);
        window.render_frame(cx);
        assert!(window.try_find(body.clone()).is_some());
        window.click(toggle, cx);
        window.render_frame(cx);
        assert!(window.try_find(body).is_none());
    })
    .unwrap();
}

fn open_model_settings(
    cx: &mut TestAppContext,
) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<Xylo>) {
    cx.update(gpui_kit::init);
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Isolated fixture")), window, cx))
        })
        .unwrap()
    });
    cx.update_window(window, |_, _, cx| {
        view.update(cx, |view, cx| {
            view.error = None;
            let primary = ProviderConfig {
                name: "Fixture".into(),
                model: "primary".into(),
                ..Default::default()
            };
            let mut backup = primary.clone();
            backup.model = "backup".into();
            view.settings.provider = primary.clone();
            view.settings.providers = vec![primary, backup];
            view.settings_open = true;
            view.page = Page::Providers;
            cx.notify();
        });
    })
    .unwrap();
    (window, view)
}

#[gpui_kit::test]
fn thinking_settings_provider_fallback_picker_updates_the_whole_connection(
    cx: &mut TestAppContext,
) {
    let (window, view) = open_model_settings(cx);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("provider-fallback-picker", cx);
        window.within("popup-menu").click(1usize, cx);
        let settings = &view.read(cx).settings;
        assert_eq!(settings.provider.fallback_model.as_deref(), Some("backup"));
        assert!(
            settings
                .providers
                .iter()
                .all(|provider| provider.fallback_model.as_deref() == Some("backup"))
        );
        assert!(settings.fallback_model.is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn thinking_settings_global_fallback_picker_applies_to_turn_defaults(cx: &mut TestAppContext) {
    let (window, view) = open_model_settings(cx);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("global-fallback-picker", cx);
        window.within("popup-menu").click(1usize, cx);
        let settings = &view.read(cx).settings;
        assert_eq!(settings.fallback_model.as_deref(), Some("backup"));
        assert!(settings.provider.fallback_model.is_none());
        assert_eq!(
            settings.turn_provider().fallback_model.as_deref(),
            Some("backup")
        );
    })
    .unwrap();
}
