//! Native split handles and durable product appearance preferences.
use super::*;
use gpui_kit::component::resizable::{HandleEdge, resize_handle};
use gpui_kit::component::{Theme, ThemeMode};
#[derive(Clone, Copy)]
pub(super) enum Panel {
    Sidebar,
    Work,
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
impl Xylo {
    pub(super) fn resize_edge(&self, panel: Panel) -> AnyElement {
        let (id, axis) = match panel {
            Panel::Sidebar => ("sidebar-resize", Axis::Horizontal),
            Panel::Files => ("files-resize", Axis::Horizontal),
            Panel::Work => ("work-resize", Axis::Horizontal),
            Panel::Terminal => ("terminal-resize", Axis::Vertical),
        };
        resize_handle(id, axis)
            .inside(if matches!(panel, Panel::Terminal | Panel::Work) {
                HandleEdge::Leading
            } else {
                HandleEdge::Trailing
            })
            .on_drag(ResizePanel(panel), |drag, _, _, cx| {
                cx.new(|_| drag.clone())
            })
            .into_any_element()
    }
    pub(super) fn remember_work_panel(&mut self) {
        if self.workspace_ready
            && let Some(root) = &self.workspace
        {
            self.settings.ui.project_panels.insert(
                root.clone(),
                WorkspacePanelPreferences {
                    open: self.settings.ui.work_panel_open,
                    panel: self.settings.ui.work_panel.clone(),
                    width: self.settings.ui.work_panel_width,
                },
            );
        }
    }
    pub(super) fn persist_ui(&mut self, cx: &mut Context<Self>) {
        self.remember_work_panel();
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
        // Native primitives (inputs, menus, markdown, scrollbars) paint with
        // Xylo's palette rather than the toolkit's defaults.
        sync_toolkit_theme(cx);
        install_fonts(cx);
        cx.notify();
    }
}
