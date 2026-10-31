//! Native split handles and durable product appearance preferences.
use super::*;
use gpui_kit::component::resizable::{HandleEdge, resize_handle};
use gpui_kit::component::{Theme, ThemeMode};
#[derive(Clone, Copy)]
pub(super) enum Panel {
    Sidebar,
    Work,
    Files,
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
        };
        resize_handle(id, axis)
            .inside(if matches!(panel, Panel::Work) {
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
        let settings = self.settings.clone();
        self.host_call(cx, |host| host.save_settings(settings));
    }
    /// Applies the theme preference: "light", "dark", or "system", which
    /// follows the macOS appearance and keeps following it while chosen.
    pub(super) fn apply_theme(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.theme == "system" {
            // Hand the window back to the system appearance, so what it
            // reports (now and on every later change) is the Mac's choice.
            cx.set_window_appearance(None);
            follow_system_appearance(window, cx);
            self.paint_theme(system_is_dark(window), window, cx);
        } else {
            let dark = self.settings.theme == "dark";
            cx.set_window_appearance(Some(if dark {
                WindowAppearance::Dark
            } else {
                WindowAppearance::Light
            }));
            self.paint_theme(dark, window, cx);
        }
    }
    fn paint_theme(&self, dark: bool, window: &mut Window, cx: &mut Context<Self>) {
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
        // Native primitives (inputs, menus, markdown, scrollbars) paint with
        // Xylo's palette rather than the toolkit's defaults.
        sync_toolkit_theme(cx);
        cx.notify();
    }
}

fn system_is_dark(window: &Window) -> bool {
    matches!(
        window.appearance(),
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

/// Repaints whenever the Mac's appearance changes while "System" is chosen.
/// Registered once for the main window; a later Light or Dark choice makes
/// the observer a no-op rather than unregistering it.
fn follow_system_appearance(window: &mut Window, cx: &mut Context<Xylo>) {
    static FOLLOWING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if FOLLOWING.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    cx.observe_window_appearance(window, |view, window, cx| {
        if view.settings.theme == "system" {
            view.paint_theme(system_is_dark(window), window, cx);
        }
    })
    .detach();
}
