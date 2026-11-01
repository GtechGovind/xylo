//! Provider reasoning is available on demand and folded by default.
use super::*;

impl Xylo {
    pub(in crate::xylo) fn thinking_block(
        &self,
        key: Uuid,
        thinking: SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.expanded_tools.contains(&key);
        div()
            .id(ElementId::Uuid(key))
            .test_support()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .id(ElementId::Name(format!("thinking-toggle-{key}").into()))
                    .test_support()
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .gap_2()
                    .aria_label(if open {
                        "Hide Thinking"
                    } else {
                        "Show Thinking"
                    })
                    .text_size(px(13.))
                    .text_color(rgb(text_secondary()))
                    .child(icons::small(if open {
                        AppIcon::ChevronDown
                    } else {
                        AppIcon::ChevronRight
                    }))
                    .child("Thinking")
                    .on_click(cx.listener(move |view, _, _, cx| view.toggle_expanded(key, cx))),
            )
            .when(open, |row| {
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

#[cfg(test)]
#[path = "thinking/tests.rs"]
mod tests;
