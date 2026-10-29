//! Native presentation tokens for the approved Xylo workspace design.
use gpui_kit::component::{Icon, IconName};
use gpui_kit::{prelude::*, *};
use std::sync::atomic::{AtomicBool, Ordering};

/// A small X monogram drawn directly in Rust, with no external branding assets.
pub fn brand_pixels(size: u32) -> image::RgbaImage {
    let margin = size / 5;
    let stroke = (size / 12).max(1);
    image::RgbaImage::from_fn(size, size, |x, y| {
        let inside = x >= margin && x < size - margin && y >= margin && y < size - margin;
        let mark = inside && (x.abs_diff(y) <= stroke || x.abs_diff(size - 1 - y) <= stroke);
        image::Rgba(if mark { [32, 35, 43, 255] } else { [255; 4] })
    })
}

static DARK: AtomicBool = AtomicBool::new(false);
pub fn set_dark(dark: bool) {
    DARK.store(dark, Ordering::Relaxed);
}
fn color(light: u32, dark: u32) -> u32 {
    if DARK.load(Ordering::Relaxed) {
        dark
    } else {
        light
    }
}
pub fn bg() -> u32 {
    color(0xf9fafb, 0x17191f)
}
pub fn panel() -> u32 {
    color(0xffffff, 0x20232b)
}
pub fn sidebar() -> u32 {
    color(0xf2f3f5, 0x1d2027)
}
pub fn border() -> u32 {
    color(0xe5e6eb, 0x363a46)
}
pub fn muted() -> u32 {
    color(0x687383, 0xb0b7c5)
}
pub fn text() -> u32 {
    color(0x000000, 0xf1f3f8)
}
pub fn accent() -> u32 {
    color(0x7583b2, 0xa7b8f0)
}
pub fn lavender() -> u32 {
    color(0xeaecf7, 0x2c3040)
}
pub fn selected() -> u32 {
    color(0xe5e7f0, 0x353c51)
}
pub fn hover() -> u32 {
    color(0xe9eaef, 0x353944)
}
/// Status and change colours; each meets 4.5:1 against its panel in both themes.
pub fn success() -> u32 {
    color(0x1a7f45, 0x6fd69a)
}
pub fn danger() -> u32 {
    color(0xb42318, 0xff8a80)
}
pub fn warning() -> u32 {
    color(0x9a5b00, 0xf5bd5c)
}
pub fn warning_bg() -> u32 {
    color(0xfff8eb, 0x2d2618)
}
pub fn danger_bg() -> u32 {
    color(0xfef3f2, 0x33201f)
}
pub fn code_bg() -> u32 {
    color(0xf6f7f9, 0x191b21)
}
pub fn diff_added_bg() -> u32 {
    color(0xe7f6ec, 0x1c3326)
}
pub fn diff_removed_bg() -> u32 {
    color(0xfdecea, 0x3a2224)
}
/// Visible keyboard focus for every interactive control (WCAG 2.4.7).
pub fn focus_ring(button: gpui_kit::base::Button) -> gpui_kit::base::Button {
    button
        .border_1()
        .border_color(transparent_black())
        .focus_visible(|style| style.border_color(rgb(accent())))
}
fn button_base(id: impl Into<ElementId>, label: impl Into<SharedString>) -> gpui_kit::base::Button {
    let label: SharedString = label.into();
    focus_ring(gpui_kit::base::Button::new(id))
        .accessibility_label(label.clone())
        .px_3()
        .py_1()
        .min_h(px(28.))
        .rounded_md()
        .text_size(px(12.))
        .text_color(rgb(text()))
        .cursor_pointer()
        .child(label)
}
pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> gpui_kit::base::Button {
    button_base(id, label).hover(|d| d.bg(rgb(hover())))
}
/// Compact toolbar action with an accessible name independent of its glyph.
pub fn icon_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    name: IconName,
) -> gpui_kit::base::Button {
    focus_ring(gpui_kit::base::Button::new(id))
        .accessibility_label(label)
        .size(px(28.))
        .flex_shrink_0()
        .rounded_md()
        .flex()
        .items_center()
        .justify_center()
        .text_color(rgb(muted()))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(hover())).text_color(rgb(text())))
        .child(icon(name))
}
pub fn primary(id: impl Into<ElementId>, label: impl Into<SharedString>) -> gpui_kit::base::Button {
    // Inverted in dark mode so the primary action keeps its contrast.
    button_base(id, label)
        .self_start()
        .flex_shrink_0()
        .bg(rgb(color(0x1d2129, 0xe8ebf3)))
        .text_color(rgb(color(0xffffff, 0x14161b)))
        .hover(|d| d.bg(rgb(color(0x454d5f, 0xc9cfdc))))
        .font_weight(FontWeight::MEDIUM)
}
pub fn icon(name: IconName) -> Icon {
    Icon::new(name).size(px(16.))
}
pub fn nav_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    name: IconName,
) -> gpui_kit::base::Button {
    let label: SharedString = label.into();
    focus_ring(gpui_kit::base::Button::new(id))
        .accessibility_label(label.clone())
        .w_full()
        .flex()
        .items_center()
        .gap_3()
        .justify_start()
        .h(px(34.))
        .overflow_hidden()
        .whitespace_nowrap()
        .px_3()
        .rounded_md()
        .text_size(px(12.))
        .text_color(rgb(text()))
        .hover(|d| d.bg(rgb(border())))
        .child(icon(name))
        .child(label)
}
pub fn subtitle(text: impl Into<SharedString>) -> Div {
    div().text_xs().text_color(rgb(muted())).child(text.into())
}
pub fn card() -> Div {
    div()
        .p_4()
        .rounded_lg()
        .bg(rgb(panel()))
        .border_1()
        .border_color(rgb(border()))
        .flex()
        .flex_col()
        .gap_3()
}
pub fn heading(title: impl Into<SharedString>, description: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .mb_4()
        .child(
            div()
                .text_2xl()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.into()),
        )
        .child(subtitle(description))
}
