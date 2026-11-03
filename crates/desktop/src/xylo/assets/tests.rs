use super::poll_delay;
use std::time::Duration;

#[test]
fn status_polling_backs_off_after_errors_and_stays_bounded() {
    assert_eq!(poll_delay(0), Duration::from_millis(800));
    assert_eq!(poll_delay(1), Duration::from_millis(1600));
    assert_eq!(poll_delay(10), Duration::from_secs(30));
    assert_eq!(poll_delay(u32::MAX), Duration::from_secs(30));
}

#[gpui_kit::test]
fn a_pending_asset_receipt_blocks_other_install_requests(cx: &mut gpui_kit::TestAppContext) {
    use crate::xylo::{Page, Xylo};
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext, WindowOptions};
    cx.update(gpui_kit::init);
    let (handle, view) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Isolated fixture")), window, cx))
        })
        .unwrap()
    });
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.navigate(Page::Assets, cx);
            view.assets.state = super::CatalogState::Ready;
            view.assets.catalog = serde_json::from_value(serde_json::json!([
                {"id":"first","name":"First","kind":"voice","supported":true},
                {"id":"second","name":"Second","kind":"voice","supported":true}
            ]))
            .unwrap();
            view.assets.pending_action = Some("first".into());
            cx.notify();
        });
        window.render_frame(cx);
        assert_eq!(
            window.find(("install-asset", 0usize)).disabled(),
            Some(true)
        );
        assert_eq!(
            window.find(("install-asset", 1usize)).disabled(),
            Some(true)
        );
        view.update(cx, |view, cx| {
            view.asset_error("helper_asset_install", "rejected fixture", cx);
        });
        window.render_frame(cx);
        assert_ne!(
            window.find(("install-asset", 1usize)).disabled(),
            Some(true)
        );
    })
    .unwrap();
}
