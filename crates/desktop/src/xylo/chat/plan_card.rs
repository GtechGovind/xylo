//! Plan review with an explicit continuation mode and feedback.
use super::*;
use crate::state::ToolCard;
use xylo_core::InteractionAnswer;

impl Xylo {
    pub(super) fn plan_decision_card(
        &self,
        i: usize,
        tool: &ToolCard,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = tool.id;
        let mut modes = div().flex().flex_wrap().gap_2();
        for (index, mode) in PermissionMode::ALL
            .into_iter()
            .filter(|mode| *mode != PermissionMode::Plan)
            .enumerate()
        {
            modes = modes.child(
                primary(
                    ElementId::Name(format!("approve-plan-{i}-{index}").into()),
                    format!("Approve · {}", mode.label()),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_permission_mode(mode, cx);
                    this.answer_interaction(id, InteractionAnswer::ApprovePlan { mode }, cx);
                })),
            );
        }
        div().p_3().flex().flex_col().gap_3()
            .child(div().whitespace_normal().child(tool.input["plan"].as_str().unwrap_or_default().to_owned()))
            .child(subtitle("Choose the permission mode to continue. To keep planning, write feedback in the composer."))
            .child(modes)
            .child(div().flex().gap_2()
                .child(secondary(("keep-planning", i), "Keep planning with feedback").disabled(self.composer.read(cx).value().trim().is_empty()).on_click(cx.listener(move |this, _, _, cx| {
                    let feedback = this.composer.read(cx).value().trim().to_owned();
                    if !feedback.is_empty() { this.answer_interaction(id, InteractionAnswer::KeepPlanning { feedback }, cx); }
                })))
                .child(secondary(("reject-plan", i), "Reject").on_click(cx.listener(move |this, _, _, cx| this.answer_interaction(id, InteractionAnswer::Reject, cx)))))
            .into_any_element()
    }
}
