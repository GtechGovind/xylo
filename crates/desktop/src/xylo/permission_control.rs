//! The composer's permission control. It shows the current preset with its
//! icon and switches presets from a menu. A running chat displays the effective
//! mode reported by the host; saved settings provide the default. Explicit
//! choices reach the worker even when the saved preset already has that value.
use super::*;
use crate::icons::{self, AppIcon};
use gpui_kit::component::{
    button::{Button as MenuButton, ButtonVariants},
    menu::{DropdownMenu, PopupMenuItem},
};

/// Icon that identifies a preset wherever it is shown.
pub(super) fn permission_glyph(mode: PermissionMode) -> AppIcon {
    match mode {
        PermissionMode::AskForApproval => AppIcon::PermissionAsk,
        PermissionMode::ApproveForMe => AppIcon::PermissionAuto,
        PermissionMode::FullAccess => AppIcon::PermissionFull,
        PermissionMode::ReadOnly => AppIcon::PermissionReadOnly,
        PermissionMode::Plan => AppIcon::Plan,
    }
}

/// A selected permission mode is ordinary context, not a persistent warning.
fn permission_color(_mode: PermissionMode) -> u32 {
    text_secondary()
}

/// One menu row: icon, label and the preset's one-line description.
fn permission_row(mode: PermissionMode, active: bool) -> impl IntoElement {
    let color = text();
    div()
        .id(ElementId::Name(
            format!("permission-choice-{}", mode.label()).into(),
        ))
        .role(gpui_kit::Role::MenuItem)
        .aria_label(format!("{}. {}", mode.label(), mode.description()))
        .w(px(284.))
        .rounded(px(8.))
        .px(px(8.))
        .when(active, |row| row.bg(rgb(crate::ui::selected())))
        .flex()
        .items_start()
        .gap_2()
        .py(px(8.))
        .child(
            icons::small(permission_glyph(mode))
                .mt(px(2.))
                .text_color(rgb(color)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(rgb(color))
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().child(mode.label()))
                        .child(
                            div()
                                .w(px(14.))
                                .flex_shrink_0()
                                .when(active, |slot| slot.child(icons::small(AppIcon::Done))),
                        ),
                )
                .child(
                    div()
                        .text_size(px(chat::tokens::META_SIZE))
                        .line_height(relative(1.4))
                        .text_color(rgb(muted()))
                        .whitespace_normal()
                        .child(match mode {
                            PermissionMode::AskForApproval => {
                                "Ask before working outside this project"
                            }
                            PermissionMode::ApproveForMe => {
                                "Ask only when an action needs your review"
                            }
                            PermissionMode::FullAccess => "Work with files and the internet freely",
                            PermissionMode::ReadOnly => "Explore files without making changes",
                            PermissionMode::Plan => "Prepare a plan before making changes",
                        }),
                ),
        )
}

impl Xylo {
    pub(super) fn current_permission_mode(&self) -> PermissionMode {
        self.effective_permissions
            .get(&self.session)
            .map_or(self.settings.permissions, |(_, mode)| *mode)
    }

    pub(super) fn set_permission_mode(&mut self, mode: PermissionMode, cx: &mut Context<Self>) {
        if self.settings.permissions == mode {
            if self.current_permission_mode() != mode {
                self.engine_command(WorkerCommand::SetPermissions { mode }, cx);
            }
            return;
        }
        self.settings.permissions = mode;
        let settings = self.settings.clone();
        self.host_call(cx, |host| host.save_settings(settings));
        cx.notify();
    }

    pub(super) fn composer_permissions(&self, cx: &Context<Self>) -> AnyElement {
        let weak = cx.entity().downgrade();
        let mode = self.current_permission_mode();
        MenuButton::new("composer-permissions")
            .ghost()
            .accessibility_label(mode.label())
            .gap_1()
            .child(
                icons::small(AppIcon::Permissions)
                    .size(px(17.))
                    .flex_shrink_0()
                    .text_color(rgb(permission_color(mode))),
            )
            .when(self.chat_width.get() >= 520., |button| {
                button.child(div().min_w_0().single_line().child(mode.label()))
            })
            .max_w(px(170.))
            .h(px(chat::tokens::CONTROL_SIZE))
            .rounded_full()
            .text_size(px(12.))
            .text_color(rgb(permission_color(mode)))
            .tooltip(mode.description())
            .dropdown_menu(move |mut menu, window, cx| {
                let Ok(active) = weak.update(cx, |view, _| view.current_permission_mode()) else {
                    return menu;
                };
                menu = menu
                    .label("Access")
                    .scrollable(true)
                    .max_h((window.viewport_size().height - px(64.)).max(px(160.)));
                for mode in PermissionMode::ALL {
                    let view = weak.clone();
                    menu = menu.item(
                        PopupMenuItem::element(move |_, _| permission_row(mode, active == mode))
                            .on_click(move |_, _, cx| {
                                let _ =
                                    view.update(cx, |view, cx| view.set_permission_mode(mode, cx));
                            }),
                    );
                }
                menu
            })
            .into_any_element()
    }
}

#[cfg(all(test, not(feature = "validation")))]
#[path = "permission_control/tests.rs"]
mod tests;
