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

/// Full access removes every guard, so it is always drawn as a warning.
fn permission_color(mode: PermissionMode) -> u32 {
    if mode == PermissionMode::FullAccess {
        warning()
    } else {
        text_secondary()
    }
}

/// One menu row: icon, label and the preset's one-line description.
fn permission_row(mode: PermissionMode) -> Div {
    let color = if mode == PermissionMode::FullAccess {
        warning()
    } else {
        text()
    };
    div()
        .w(px(300.))
        .flex()
        .items_start()
        .gap_2()
        .py(px(2.))
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
                        .child(mode.label()),
                )
                .child(
                    div()
                        .text_size(px(11.5))
                        .line_height(relative(1.4))
                        .text_color(rgb(muted()))
                        .whitespace_normal()
                        .child(mode.description()),
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
                icons::small(permission_glyph(mode))
                    .flex_shrink_0()
                    .text_color(rgb(permission_color(mode))),
            )
            .child(div().min_w_0().single_line().child(mode.label()))
            .max_w(px(170.))
            .h(px(30.))
            .rounded_full()
            .text_size(px(12.5))
            .text_color(rgb(permission_color(mode)))
            .tooltip(mode.description())
            .dropdown_menu(move |mut menu, _, cx| {
                let Ok(active) = weak.update(cx, |view, _| view.current_permission_mode()) else {
                    return menu;
                };
                menu = menu.label("How should Xylo's actions be approved?");
                for mode in PermissionMode::ALL {
                    let view = weak.clone();
                    menu = menu.item(
                        PopupMenuItem::element(move |_, _| permission_row(mode))
                            .checked(active == mode)
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

#[cfg(test)]
#[path = "permission_control/tests.rs"]
mod tests;
