//! Native presentation tokens aligned with AionUi's default light theme.
//! Reference: iOfficeAI/AionUi default-color-scheme.css (Apache-2.0).
use gpui_kit::component::{Icon, IconName};
use gpui_kit::{prelude::*, *};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

/// Decode the embedded 64 px mark once, before its first paint. A ready render
/// image avoids the asynchronous asset loader and retains one texture identity.
pub fn brand_image() -> Arc<RenderImage> {
    static IMAGE: OnceLock<Arc<RenderImage>> = OnceLock::new();
    IMAGE
        .get_or_init(|| {
            let mut pixels = image::load_from_memory_with_format(
                include_bytes!("../../../packaging/icons/sidebar.png"),
                image::ImageFormat::Png,
            )
            .expect("committed Xylo sidebar PNG is valid")
            .into_rgba8();
            // GPUI's texture atlas consumes BGRA, matching its own PNG loader.
            for pixel in pixels.pixels_mut() {
                pixel.0.swap(0, 2);
            }
            Arc::new(RenderImage::new(vec![image::Frame::new(pixels)]))
        })
        .clone()
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
pub fn message_bg() -> u32 {
    color(0xe9eaef, 0x292c34)
}
fn button_base(id: impl Into<ElementId>, label: impl Into<SharedString>) -> gpui_kit::base::Button {
    let label: SharedString = label.into();
    gpui_kit::base::Button::new(id)
        .accessibility_label(label.clone())
        .px_3()
        .py_1()
        .min_h(px(30.))
        .rounded_md()
        .text_sm()
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
    gpui_kit::base::Button::new(id)
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
pub fn compact_selector(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui_kit::base::Button {
    let label = label.into();
    gpui_kit::base::Button::new(id)
        .accessibility_label(label.clone())
        .flex()
        .items_center()
        .gap_1()
        .min_w_0()
        .h(px(28.))
        .px_2()
        .rounded_md()
        .text_xs()
        .text_color(rgb(muted()))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(hover())).text_color(rgb(text())))
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(label),
        )
        .child(icon(IconName::ChevronDown).size(px(12.)))
}
pub fn primary(id: impl Into<ElementId>, label: impl Into<SharedString>) -> gpui_kit::base::Button {
    button_base(id, label)
        .bg(rgb(0x1d2129))
        .text_color(rgb(0xffffff))
        .hover(|d| d.bg(rgb(0x454d5f)))
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
    gpui_kit::base::Button::new(id)
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
        .text_sm()
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
