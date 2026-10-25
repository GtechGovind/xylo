//! Native split handles and durable product appearance preferences.
use super::*;
use gpui_kit::component::resizable::{HandleEdge, resize_handle};
use gpui_kit::component::{Theme, ThemeMode};
#[derive(Clone, Copy)]
pub(super) enum Panel {
    Sidebar,
    Files,
    Terminal,
}
#[derive(Clone)]
pub(super) struct ResizePanel(pub Panel);
impl Render for ResizePanel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}
impl Workbench {
    pub(super) fn resize_edge(&self, panel: Panel) -> AnyElement {
        let (id, axis) = match panel {
            Panel::Sidebar => ("sidebar-resize", Axis::Horizontal),
            Panel::Files => ("files-resize", Axis::Horizontal),
            Panel::Terminal => ("terminal-resize", Axis::Vertical),
        };
        resize_handle(id, axis)
            .inside(if matches!(panel, Panel::Terminal) {
                HandleEdge::Leading
            } else {
                HandleEdge::Trailing
            })
            .on_drag(ResizePanel(panel), |drag, _, _, cx| {
                cx.new(|_| drag.clone())
            })
            .into_any_element()
    }
    pub(super) fn persist_ui(&mut self, cx: &mut Context<Self>) {
        self.settings.ui.active_page = self.page.key().into();
        self.settings.ui.terminal_visible = self.terminal_visible;
        let settings = self.settings.clone();
        self.host_call(cx, |host| host.save_settings(settings));
    }
    pub(super) fn apply_theme(&self, window: &mut Window, cx: &mut Context<Self>) {
        let dark = self.settings.theme == "dark";
        set_dark(dark);
        Theme::change(
            if dark {
                ThemeMode::Dark
            } else {
                ThemeMode::Light
            },
            Some(window),
            cx,
        );
        cx.set_window_appearance(Some(if dark {
            WindowAppearance::Dark
        } else {
            WindowAppearance::Light
        }));
        cx.notify();
    }
    pub(super) fn appearance_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut themes = div().flex().gap_2();
        for (id, label) in [("light", "Light"), ("dark", "Dark")] {
            themes = themes.child(
                button(SharedString::from(format!("theme-{id}")), label)
                    .border_1()
                    .border_color(rgb(border()))
                    .bg(rgb(if self.settings.theme == id {
                        selected()
                    } else {
                        panel()
                    }))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.settings.theme = id.into();
                        view.apply_theme(window, cx);
                        view.persist_ui(cx);
                    })),
            );
        }
        let mut layout = card().child(subtitle(
            "Drag a panel edge to resize it, or use the controls below.",
        ));
        for (i, label, kind) in [
            (0, "Sidebar", Panel::Sidebar),
            (1, "File browser", Panel::Files),
            (2, "Terminal", Panel::Terminal),
        ] {
            let mut row = div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().flex_1().child(label));
            for (suffix, delta, glyph) in [("smaller", -24., "−"), ("larger", 24., "+")] {
                row = row.child(
                    button(SharedString::from(format!("layout-{i}-{suffix}")), glyph)
                        .accessibility_label(format!("Make {label} {suffix}"))
                        .on_click(cx.listener(move |view, _, _, cx| {
                            match kind {
                                Panel::Sidebar => {
                                    view.settings.ui.sidebar_width =
                                        (view.settings.ui.sidebar_width + delta).clamp(200., 400.)
                                }
                                Panel::Files => {
                                    view.settings.ui.files_width =
                                        (view.settings.ui.files_width + delta).clamp(180., 480.)
                                }
                                Panel::Terminal => {
                                    view.settings.ui.terminal_height =
                                        (view.settings.ui.terminal_height + delta).clamp(150., 520.)
                                }
                            }
                            view.persist_ui(cx);
                            cx.notify();
                        })),
                );
            }
            layout = layout.child(row);
        }
        div()
            .size_full()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(heading("Appearance", "Make your workspace comfortable."))
            .child(card().child(subtitle("THEME")).child(themes))
            .child(layout)
            .child(button("reset-layout", "Reset layout").on_click(cx.listener(
                |view, _, _, cx| {
                    view.settings.ui = UiPreferences::default();
                    view.persist_ui(cx);
                    cx.notify();
                },
            )))
            .into_any_element()
    }
}
