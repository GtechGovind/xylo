use super::*;
impl Xylo {
    pub(super) fn diagnostics_page(&self, _: &mut Context<Self>) -> AnyElement {
        let mut page = div()
            .id("activity-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(heading(
                "Activity",
                "Track your conversations and task progress.",
            ));
        let active = self.turns.values().filter(|t| t.running).count();
        page = page.child(
            card()
                .child(div().text_lg().child(if active == 0 {
                    "All caught up".into()
                } else {
                    format!(
                        "{active} task{} in progress",
                        if active == 1 { "" } else { "s" }
                    )
                }))
                .child(subtitle(format!(
                    "{} saved conversations",
                    self.conversations.len()
                ))),
        );
        if let Some(turn) = self.turns.get(&self.session) {
            page = page.child(
                card()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(
                        if turn.running {
                            "Working on your request"
                        } else {
                            "Latest task"
                        },
                    ))
                    .child(subtitle(turn.status.clone()))
                    .child(subtitle(format!(
                        "{} input tokens · {} output tokens",
                        turn.usage.input_tokens, turn.usage.output_tokens
                    ))),
            );
        }
        page.into_any_element()
    }
}
