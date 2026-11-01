//! Provider reasoning is available on demand and folded by default.
use super::*;
use crate::motion::Disclosure;

impl Xylo {
    pub(in crate::xylo) fn thinking_block(
        &self,
        key: Uuid,
        thinking: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if thinking.trim().is_empty() {
            return div().into_any_element();
        }
        let open = self.expanded_tools.contains(&key);
        let disclosure = Disclosure::new(ElementId::Uuid(key), open, window, cx);
        div()
            .id(ElementId::Uuid(key))
            .test_support()
            .w_full()
            .mb(px(6.))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                // Same disclosure row as the step and tool groups: a focusable
                // button with a hover fill and an animated chevron.
                focus_ring(gpui_kit::base::Button::new(ElementId::Name(
                    format!("thinking-toggle-{key}").into(),
                )))
                .self_start()
                .max_w_full()
                .accessibility_label(if open {
                    "Approach. Hide details"
                } else {
                    "Approach. Show details"
                })
                .mx(px(-8.))
                .px(px(8.))
                .h(px(28.))
                .rounded(px(8.))
                .flex()
                .items_center()
                .justify_start()
                .gap(px(4.))
                .cursor_pointer()
                .text_size(px(13.))
                .text_color(rgb(muted()))
                .hover(|style| style.bg(rgb(hover())).text_color(rgb(text_secondary())))
                .child("Approach")
                .child(disclosure.chevron().text_color(rgb(icons::rest_color())))
                .on_click(cx.listener(move |view, _, _, cx| view.toggle_expanded(key, cx))),
            )
            .when(disclosure.is_visible(), |row| {
                row.child(
                    div()
                        .id(ElementId::Name(format!("thinking-body-{key}").into()))
                        .test_support()
                        .pl_4()
                        .border_l_1()
                        .border_color(rgb(border()))
                        .text_color(rgb(text_secondary()))
                        .text_size(px(13.))
                        .child(TextView::markdown(key.to_string(), thinking).selectable(true)),
                )
            })
            .into_any_element()
    }
}

#[cfg(all(test, not(feature = "validation")))]
#[path = "thinking/tests.rs"]
mod tests;
