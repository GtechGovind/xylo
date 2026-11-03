//! A running chat must stay identifiable without continuously redrawing the workspace.
use super::sidebar_chat_button;
use gpui_kit::{Context, ParentElement, Render, Styled, TestAppContext, Window, div, px, size};

struct RunningChat;

impl Render for RunningChat {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui_kit::IntoElement {
        div().size_full().child(sidebar_chat_button(
            "running-chat",
            "Review changes".into(),
            false,
            true,
            false,
        ))
    }
}

#[gpui_kit::test]
fn running_chat_does_not_request_animation_frames(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.update(|cx| cx.set_reduce_motion(false));
    let window = cx.open_window(size(px(222.), px(64.)), |_, _| RunningChat);
    cx.run_until_parked();
    let frames = window
        .update(cx, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    assert_eq!(frames, 0, "a sidebar status must not redraw the workspace");
}
