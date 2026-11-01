//! Explicit plan decisions with local feedback and a separate allowed-mode choice.
use super::*;
use crate::state::ToolCard;
use xylo_core::InteractionAnswer;

impl Xylo {
    pub(super) fn plan_decision_card(
        &self,
        i: usize,
        tool: &ToolCard,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = tool.id;
        let session = self.session;
        let pending = self.interaction_pending.contains(&(session, id));
        let (allowed, scheduled): (Vec<PermissionMode>, bool) = self
            .interaction_allowed_modes
            .get(&(session, id))
            .cloned()
            .unwrap_or_else(|| {
                (
                    serde_json::from_value(tool.input["allowed_modes"].clone()).unwrap_or_default(),
                    tool.input["scheduled"].as_bool().unwrap_or(false),
                )
            });
        let allowed: Vec<_> = allowed
            .into_iter()
            .filter(|mode| {
                !scheduled
                    || xylo_runtime::scheduler::wakeup_mode(*mode, self.settings.permissions)
                        == *mode
            })
            .collect();
        let selected = self
            .interaction_plan_modes
            .borrow()
            .get(&(session, id))
            .copied()
            .filter(|mode| allowed.contains(mode))
            .or_else(|| {
                allowed
                    .contains(&self.current_permission_mode())
                    .then(|| self.current_permission_mode())
            });
        use gpui_kit::component::button::{Button as MenuButton, ButtonVariants};
        let weak = cx.entity().downgrade();
        let modes = MenuButton::new(("plan-permissions", i))
            .ghost()
            .disabled(pending || allowed.is_empty())
            .border_1()
            .border_color(rgb(border()))
            .rounded(px(6.))
            .px(px(6.))
            .py(px(6.))
            .text_size(px(12.))
            .child(
                selected
                    .map(|mode| mode.label())
                    .unwrap_or("Choose permissions"),
            )
            .child(icons::small(AppIcon::ChevronDown))
            .dropdown_menu(move |mut menu, _, _| {
                for (index, mode) in allowed.clone().into_iter().enumerate() {
                    let weak = weak.clone();
                    menu = menu.item(
                        PopupMenuItem::element(move |_, _| {
                            div()
                                .id(ElementId::Name(format!("plan-mode-{i}-{index}").into()))
                                .test_support()
                                .child(mode.label())
                                .into_any_element()
                        })
                        .checked(selected == Some(mode))
                        .on_click(move |_, _, cx| {
                            let _ = weak.update(cx, |view, cx| {
                                view.interaction_plan_modes
                                    .borrow_mut()
                                    .insert((session, id), mode);
                                view.invalidate_live_history();
                                view.sync_history_list(cx);
                                cx.notify();
                            });
                        }),
                    );
                }
                menu
            });
        let input = self.interaction_input(session, id, usize::MAX, window, cx);
        div()
            .p(px(20.))
            .my(px(20.))
            .border_1()
            .border_color(rgb(border_strong()))
            .rounded(px(14.))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .text_size(px(12.))
                    .text_color(rgb(muted()))
                    .child(icons::small(AppIcon::Plan).size(px(14.)))
                    .child("Your plan is ready"),
            )
            .child(
                div()
                    .text_size(px(15.))
                    .whitespace_normal()
                    .child(tool.input["plan"].as_str().unwrap_or_default().to_owned()),
            )
            .when(selected.is_none(), |card| {
                card.child(subtitle("Choose a permission mode to start this plan."))
            })
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .child(subtitle("Run with").text_size(px(12.)).flex_shrink_0())
                    .child(modes)
                    .child(div().flex_1())
                    .child(
                        primary(
                            ("start-plan", i),
                            if pending {
                                "Sending decision…"
                            } else {
                                "Start this plan"
                            },
                        )
                        .disabled(pending || selected.is_none())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(mode) = selected {
                                this.answer_interaction(
                                    id,
                                    InteractionAnswer::ApprovePlan { mode },
                                    cx,
                                );
                            }
                        })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .mt(px(3.))
                    .child(
                        button(("toggle-plan-feedback", i), "Suggest changes")
                            .aria_expanded(
                                self.interaction_plan_feedback
                                    .borrow()
                                    .contains(&(session, id)),
                            )
                            .on_click(cx.listener(move |view, _, _, cx| {
                                let mut expanded = view.interaction_plan_feedback.borrow_mut();
                                if !expanded.remove(&(session, id)) {
                                    expanded.insert((session, id));
                                }
                                drop(expanded);
                                view.invalidate_live_history();
                                cx.notify();
                            })),
                    )
                    .child(
                        button(("reject-plan", i), "Reject")
                            .disabled(pending)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.answer_interaction(id, InteractionAnswer::Reject, cx)
                            })),
                    ),
            )
            .when(
                self.interaction_plan_feedback
                    .borrow()
                    .contains(&(session, id)),
                |card| {
                    card.child(
                        Textarea::new(&input)
                            .text_size(px(13.))
                            .aria_label("Changes to the plan")
                            .disabled(pending),
                    )
                    .child(
                        secondary(("keep-planning", i), "Send feedback")
                            .disabled(pending)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.submit_local_interaction(session, id, usize::MAX, cx)
                            })),
                    )
                },
            )
            .into_any_element()
    }
}
