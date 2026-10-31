//! Xylo's own design system: tokens and components for the native workspace.
//!
//! Every visual decision (palette, type, radii, spacing, states) lives here, in
//! application code. Native primitives from the toolkit are reused only for
//! behaviour: text editing, focus, keyboard and accessibility semantics.
//!
//! The look is Xylo's own. Cool, quiet neutrals keep long sessions restful;
//! a single indigo accent (trust, focus) marks the one thing to act on, with
//! mint reserved for success so progress reads at a glance. Display text is a
//! tight sans; generous radii soften every control the user touches.
use crate::icons::AppIcon;
use gpui_kit::component::Icon;
use gpui_kit::{prelude::*, *};
use std::sync::atomic::{AtomicBool, Ordering};

/// Xylo's mark drawn directly in Rust for the window icon: a white prompt
/// chevron with a spark, on an indigo tile.
pub fn brand_pixels(size: u32) -> image::RgbaImage {
    let s = size as f32;
    let stroke = (s / 11.).max(1.);
    // Chevron ">" in the lower-left two thirds; spark centre upper-right.
    let (ax, ay) = (s * 0.22, s * 0.30);
    let (bx, by) = (s * 0.46, s * 0.52);
    let (cx, cy) = (s * 0.22, s * 0.74);
    let (sx, sy, sr) = (s * 0.72, s * 0.30, s * 0.17);
    let segment = |x: f32, y: f32, x1: f32, y1: f32, x2: f32, y2: f32| {
        let (dx, dy) = (x2 - x1, y2 - y1);
        let t = (((x - x1) * dx + (y - y1) * dy) / (dx * dx + dy * dy)).clamp(0., 1.);
        let (px, py) = (x1 + t * dx, y1 + t * dy);
        ((x - px).powi(2) + (y - py).powi(2)).sqrt()
    };
    image::RgbaImage::from_fn(size, size, |x, y| {
        let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
        let chevron = segment(fx, fy, ax, ay, bx, by) <= stroke / 2.
            || segment(fx, fy, bx, by, cx, cy) <= stroke / 2.;
        let underline = segment(fx, fy, s * 0.50, s * 0.80, s * 0.82, s * 0.80) <= stroke / 2.;
        // Four-point spark: |dx|^0.5 + |dy|^0.5 <= r^0.5 gives the concave star.
        let (dx, dy) = ((fx - sx).abs(), (fy - sy).abs());
        let spark = dx.sqrt() + dy.sqrt() <= sr.sqrt();
        let mark = chevron || underline || spark;
        image::Rgba(if mark {
            [0xff, 0xff, 0xff, 255]
        } else {
            [0x5b, 0x5f, 0xe6, 255]
        })
    })
}

// ---------------------------------------------------------------------------
// Theme mode
// ---------------------------------------------------------------------------

static DARK: AtomicBool = AtomicBool::new(false);
pub fn set_dark(dark: bool) {
    DARK.store(dark, Ordering::Relaxed);
}
pub fn is_dark() -> bool {
    DARK.load(Ordering::Relaxed)
}
fn color(light: u32, dark: u32) -> u32 {
    if is_dark() { dark } else { light }
}

// ---------------------------------------------------------------------------
// Colour tokens
// ---------------------------------------------------------------------------
//
// Surfaces are a warm neutral scale. `bg` is the conversation canvas, `panel`
// is anything raised above it (composer, cards, menus) and `sidebar` sits
// slightly behind the canvas.

/// Conversation canvas.
pub fn bg() -> u32 {
    color(0xf6f7fa, 0x0f1117)
}
/// Raised surface: composer, cards, popovers, inputs.
pub fn panel() -> u32 {
    color(0xffffff, 0x181b23)
}
/// Navigation rail behind the canvas.
pub fn sidebar() -> u32 {
    color(0xeff1f6, 0x0b0d13)
}
/// Recessed surface inside a panel (code, tool output, user bubble).
pub fn well() -> u32 {
    color(0xeceef4, 0x222633)
}
/// Hairline separators.
pub fn border() -> u32 {
    color(0xe3e6ee, 0x272c38)
}
/// Emphasised separators and control outlines.
pub fn border_strong() -> u32 {
    color(0xcfd4e0, 0x353b4a)
}
/// Primary text.
pub fn text() -> u32 {
    color(0x15171d, 0xeceef4)
}
/// Secondary text that still carries content.
pub fn text_secondary() -> u32 {
    color(0x3b4049, 0xb9bfcc)
}
/// Captions, metadata and placeholders.
pub fn muted() -> u32 {
    color(0x646b78, 0x8f97a6)
}
/// Faint text for tertiary metadata.
pub fn faint() -> u32 {
    color(0x9aa1ad, 0x5f6675)
}
/// The single brand accent.
pub fn brand() -> u32 {
    color(0x5b5fe6, 0x8b88ff)
}
pub fn brand_hover() -> u32 {
    color(0x4a4ed0, 0x9b98ff)
}
pub fn brand_foreground() -> u32 {
    0xffffff
}
/// Accent tinted surface.
pub fn brand_soft() -> u32 {
    color(0xeceeff, 0x24264a)
}
/// Alias kept for existing call sites.
pub fn accent() -> u32 {
    brand()
}
/// Accent dark enough for small text on neutral surfaces.
pub fn accent_text() -> u32 {
    color(0x4a4ed0, 0xa5a3ff)
}
/// Alias kept for existing call sites.
pub fn lavender() -> u32 {
    brand_soft()
}
pub fn selected() -> u32 {
    color(0xe3e6f3, 0x242a3a)
}
pub fn hover() -> u32 {
    color(0xe6e9f1, 0x1c2029)
}
/// Hover on the raised `panel` surface.
pub fn hover_panel() -> u32 {
    color(0xf2f4f8, 0x20242e)
}
/// Pressed state on neutral surfaces.
pub fn pressed() -> u32 {
    color(0xdadfeb, 0x2a3040)
}
/// Status and change colours; each meets 4.5:1 against its panel in both themes.
pub fn success() -> u32 {
    color(0x117a63, 0x4fd1b0)
}
pub fn danger() -> u32 {
    color(0xc8362b, 0xff8a7a)
}
pub fn warning() -> u32 {
    color(0x9a6200, 0xf5c04f)
}
pub fn warning_bg() -> u32 {
    color(0xfff6e0, 0x2e2816)
}
pub fn danger_bg() -> u32 {
    color(0xfdeeec, 0x3a2224)
}
pub fn success_bg() -> u32 {
    color(0xe6f7f2, 0x16302a)
}
pub fn code_bg() -> u32 {
    color(0xeff1f6, 0x0b0d13)
}
pub fn diff_added_bg() -> u32 {
    color(0xe4f6ef, 0x16302a)
}
pub fn diff_removed_bg() -> u32 {
    color(0xfdeceb, 0x3a2224)
}

// ---------------------------------------------------------------------------
// Typography
// ---------------------------------------------------------------------------

/// Line height for text clipped to one line. Toolkit buttons use 1.25, and a
/// clipped line box that short cuts off descenders (g, y, p) at 13 px; 1.45
/// keeps the whole glyph inside the box.
pub const SINGLE_LINE_HEIGHT: f32 = 1.45;

/// One line of text that ends in an ellipsis when it does not fit, with a line
/// box tall enough for descenders. Use it instead of `truncate()` for every
/// clipped label; place it in a row that centers items vertically so the
/// taller line box does not shift the text.
pub trait SingleLine: Styled + Sized {
    fn single_line(self) -> Self {
        self.truncate().line_height(relative(SINGLE_LINE_HEIGHT))
    }
}
impl<T: Styled> SingleLine for T {}

pub fn mono(cx: &App) -> SharedString {
    gpui_kit::component::Theme::global(cx)
        .mono_font_family
        .clone()
}

/// Large display text.
pub fn display(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(30.))
        .line_height(relative(1.3))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(crate::ui::text()))
        .child(text.into())
}
/// Page title, used at the top of settings and tool pages.
pub fn title(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(24.))
        .line_height(relative(1.3))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(crate::ui::text()))
        .child(text.into())
}
pub fn subtitle(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.5))
        .line_height(relative(1.45))
        .text_color(rgb(muted()))
        .child(text.into())
}
/// Body copy on pages.
pub fn body_text(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(14.))
        .line_height(relative(1.55))
        .text_color(rgb(text_secondary()))
        .child(text.into())
}
pub fn heading(title_text: impl Into<SharedString>, description: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .mb_6()
        .child(title(title_text))
        .child(body_text(description).max_w(px(640.)))
}

// ---------------------------------------------------------------------------
// Icons
// ---------------------------------------------------------------------------

/// Asset path of Xylo's own mark; `crate::assets::AppAssets` serves its bytes.
pub const XYLO_MARK: &str = "icons/xylo-mark.svg";
/// Lets `Icon::new(XyloMark)` resolve to the embedded product mark.
#[derive(Clone, Copy, Debug, Default)]
pub struct XyloMark;
impl gpui_kit::component::IconNamed for XyloMark {
    fn path(self) -> SharedString {
        XYLO_MARK.into()
    }
}
/// The product mark on its tile: prompt chevron and spark, white on indigo.
pub fn brand_mark(size: f32) -> Div {
    div()
        .size(px(size))
        .flex_shrink_0()
        .rounded(px(size * 0.3))
        .bg(rgb(brand()))
        .flex()
        .items_center()
        .justify_center()
        .text_color(rgb(brand_foreground()))
        .child(Icon::new(XyloMark).size(px(size * 0.72)))
}
/// Outline variant of the mark for inline use next to assistant turns.
pub fn brand_glyph(size: f32) -> Icon {
    Icon::new(XyloMark).size(px(size)).text_color(rgb(brand()))
}

/// Circular initial avatar for the person.
pub fn avatar(initial: impl Into<SharedString>, size: f32) -> Div {
    div()
        .size(px(size))
        .flex_shrink_0()
        .rounded_full()
        .bg(rgb(color(0x117a63, 0x2fb597)))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(size * 0.46))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(0xffffff))
        .child(initial.into())
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

/// Visible keyboard focus for every interactive control (WCAG 2.4.7).
pub fn focus_ring(button: gpui_kit::base::Button) -> gpui_kit::base::Button {
    button
        .styles(|states| states.disabled(|style| style.opacity(0.5).cursor_default()))
        .border_1()
        .border_color(transparent_black())
        .focus_visible(|style| style.border_color(rgb(brand())))
}
fn button_base(id: impl Into<ElementId>, label: impl Into<SharedString>) -> gpui_kit::base::Button {
    let label: SharedString = label.into();
    focus_ring(gpui_kit::base::Button::new(id))
        .accessibility_label(label.clone())
        .px(px(12.))
        .min_h(px(32.))
        .rounded(px(8.))
        .text_size(px(13.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(text()))
        .cursor_pointer()
        .child(label)
}
/// Quiet neutral action: transparent until hovered.
pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> gpui_kit::base::Button {
    button_base(id, label)
        .hover(|d| d.bg(rgb(hover())))
        .active(|d| d.bg(rgb(pressed())))
}
/// Outlined action on raised surfaces.
pub fn secondary(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui_kit::base::Button {
    button_base(id, label)
        .bg(rgb(panel()))
        .border_color(rgb(border_strong()))
        .focus_visible(|style| style.border_color(rgb(brand())))
        .hover(|d| d.bg(rgb(hover_panel())))
        .active(|d| d.bg(rgb(pressed())))
}
/// High-emphasis neutral action; inverted per theme so it always reads as primary.
pub fn primary(id: impl Into<ElementId>, label: impl Into<SharedString>) -> gpui_kit::base::Button {
    button_base(id, label)
        .self_start()
        .flex_shrink_0()
        .bg(rgb(color(0x15171d, 0xeceef4)))
        .text_color(rgb(color(0xffffff, 0x15171d)))
        .hover(|d| d.bg(rgb(color(0x2b2f3a, 0xd3d7e2))))
        .active(|d| d.bg(rgb(color(0x3b4049, 0xb9bfcc))))
}
/// Rounded chip used for composer controls and filters: icon, label, caret.
pub fn pill(id: impl Into<ElementId>, label: impl Into<SharedString>) -> gpui_kit::base::Button {
    let label: SharedString = label.into();
    focus_ring(gpui_kit::base::Button::new(id))
        .accessibility_label(label.clone())
        .h(px(30.))
        .px(px(10.))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .text_size(px(12.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(text_secondary()))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(hover_panel())).text_color(rgb(text())))
        .active(|style| style.bg(rgb(pressed())))
        .child(label)
}
/// Outlined suggestion chip with an optional leading glyph.
pub fn chip_with(
    id: impl Into<ElementId>,
    glyph: Option<Icon>,
    label: impl Into<SharedString>,
) -> gpui_kit::base::Button {
    let label: SharedString = label.into();
    focus_ring(gpui_kit::base::Button::new(id))
        .accessibility_label(label.clone())
        .h(px(36.))
        .px(px(14.))
        .rounded_full()
        .border_color(rgb(border_strong()))
        .focus_visible(|style| style.border_color(rgb(brand())))
        .bg(rgb(panel()))
        .flex()
        .items_center()
        .gap(px(8.))
        .text_size(px(13.))
        .text_color(rgb(text_secondary()))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(hover_panel())).text_color(rgb(text())))
        .when_some(glyph, |button, glyph| {
            button.child(glyph.size(px(14.)).text_color(rgb(accent_text())))
        })
        .child(label)
}
/// Circular brand-coloured send control.
pub fn send_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    disabled: bool,
) -> gpui_kit::base::Button {
    let label: SharedString = label.into();
    focus_ring(gpui_kit::base::Button::new(id))
        .accessibility_label(label)
        .size(px(32.))
        .rounded_full()
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        // Disabled stays discoverable: a neutral fill with the arrow at
        // secondary-text contrast; the brand fill marks a sendable message.
        .bg(rgb(if disabled { hover() } else { brand() }))
        .text_color(rgb(if disabled {
            text_secondary()
        } else {
            brand_foreground()
        }))
        .cursor_pointer()
        .disabled(disabled)
        .hover(move |style| {
            if disabled {
                style
            } else {
                style.bg(rgb(brand_hover()))
            }
        })
}
/// Circular neutral stop control shown while a task runs.
pub fn stop_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui_kit::base::Button {
    let label: SharedString = label.into();
    focus_ring(gpui_kit::base::Button::new(id))
        .accessibility_label(label)
        .size(px(32.))
        .rounded_full()
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgb(color(0x15171d, 0xeceef4)))
        .text_color(rgb(color(0xffffff, 0x15171d)))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(color(0x2b2f3a, 0xd3d7e2))))
        .child(crate::icons::small(AppIcon::Stop))
}
/// Segmented choice (theme, timeouts, modes).
pub fn segment(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
) -> gpui_kit::base::Button {
    let label: SharedString = label.into();
    focus_ring(gpui_kit::base::Button::new(id))
        .accessibility_label(label.clone())
        .h(px(30.))
        .px(px(12.))
        .rounded(px(7.))
        .text_size(px(12.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(if selected { text() } else { muted() }))
        .bg(rgb(if selected { panel() } else { well() }))
        .when(selected, |style| style.shadow_sm())
        .cursor_pointer()
        .hover(move |style| {
            if selected {
                style
            } else {
                style.text_color(rgb(text()))
            }
        })
        .child(label)
}
/// Container for [`segment`] buttons.
pub fn segments() -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(2.))
        .p(px(3.))
        .rounded(px(10.))
        .bg(rgb(well()))
}

// ---------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------

/// Sidebar destination: icon, label, quiet until hovered or active.
pub fn nav_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    name: AppIcon,
) -> gpui_kit::base::Button {
    nav_item(id, label, name, false)
}
pub fn nav_item(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    name: AppIcon,
    active: bool,
) -> gpui_kit::base::Button {
    let label: SharedString = label.into();
    focus_ring(gpui_kit::base::Button::new(id))
        .accessibility_label(label.clone())
        .w_full()
        .flex()
        .items_center()
        .gap(px(10.))
        .justify_start()
        .h(px(34.))
        .overflow_hidden()
        .whitespace_nowrap()
        .px(px(10.))
        .rounded(px(8.))
        .text_size(px(13.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(if active { text() } else { text_secondary() }))
        .bg(rgb(if active { selected() } else { sidebar() }))
        .cursor_pointer()
        .hover(|d| d.bg(rgb(hover())).text_color(rgb(text())))
        .active(|d| d.bg(rgb(pressed())))
        .child(crate::icons::icon(name).text_color(rgb(if active {
            crate::icons::hover_color()
        } else {
            crate::icons::rest_color()
        })))
        .child(div().flex_1().min_w_0().single_line().child(label))
}
/// Sidebar row for a conversation: title only, quietly truncated.
pub fn recent_item(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: bool,
) -> gpui_kit::base::Button {
    let label: SharedString = label.into();
    focus_ring(gpui_kit::base::Button::new(id))
        .accessibility_label(label.clone())
        .w_full()
        .min_w_0()
        .h(px(32.))
        .px(px(10.))
        .rounded(px(8.))
        .font_weight(FontWeight::NORMAL)
        .text_size(px(13.))
        .text_color(rgb(if active { text() } else { text_secondary() }))
        .bg(rgb(if active { selected() } else { sidebar() }))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(hover())).text_color(rgb(text())))
        .flex()
        .items_center()
        .gap_2()
        .justify_start()
        .child(div().flex_1().min_w_0().single_line().child(label))
}
/// Caption above a group of sidebar rows.
pub fn sidebar_label(label: impl Into<SharedString>) -> Div {
    div()
        .px(px(10.))
        .h(px(28.))
        .flex_shrink_0()
        .min_w_0()
        .single_line()
        .flex()
        .items_center()
        .text_size(px(11.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(muted()))
        .child(label.into())
}

// ---------------------------------------------------------------------------
// Surfaces
// ---------------------------------------------------------------------------

/// Raised card with a hairline border.
pub fn card() -> Div {
    div()
        .p(px(18.))
        .rounded(px(12.))
        .bg(rgb(panel()))
        .border_1()
        .border_color(rgb(border()))
        .flex()
        .flex_col()
        .gap_3()
}
/// Hairline separator.
pub fn divider() -> Div {
    div().w_full().h(px(1.)).bg(rgb(border())).flex_shrink_0()
}
/// Inline keyboard hint.
pub fn kbd(text: impl Into<SharedString>) -> Div {
    div()
        .px(px(6.))
        .py(px(1.))
        .rounded(px(5.))
        .border_1()
        .border_color(rgb(border_strong()))
        .bg(rgb(well()))
        .text_size(px(11.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(muted()))
        .child(text.into())
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BadgeTone {
    Neutral,
    Success,
    Warning,
    Danger,
}
/// Small status label.
pub fn badge(text: impl Into<SharedString>, tone: BadgeTone) -> Div {
    let (background, foreground) = match tone {
        BadgeTone::Neutral => (well(), text_secondary()),
        BadgeTone::Success => (success_bg(), success()),
        BadgeTone::Warning => (warning_bg(), warning()),
        BadgeTone::Danger => (danger_bg(), danger()),
    };
    div()
        .px(px(8.))
        .h(px(22.))
        .flex()
        .items_center()
        .rounded_full()
        .bg(rgb(background))
        .text_size(px(11.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(foreground))
        .child(text.into())
}
/// Centered empty state with a title and description.
pub fn empty_state(
    title_text: impl Into<SharedString>,
    description: impl Into<SharedString>,
) -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .items_center()
        .text_center()
        .gap_2()
        .py(px(48.))
        .child(
            div()
                .text_size(px(20.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(text()))
                .child(title_text.into()),
        )
        .child(subtitle(description).max_w(px(420.)))
}
/// Inline notice: a tinted band with an icon and message.
pub fn notice(tone: BadgeTone, message: impl Into<SharedString>) -> Div {
    let (background, foreground, glyph) = match tone {
        BadgeTone::Success => (success_bg(), success(), AppIcon::Success),
        BadgeTone::Warning => (warning_bg(), warning(), AppIcon::Warning),
        BadgeTone::Danger => (danger_bg(), danger(), AppIcon::Error),
        BadgeTone::Neutral => (well(), text_secondary(), AppIcon::Info),
    };
    div()
        .w_full()
        .flex()
        .items_start()
        .gap_2()
        .px(px(14.))
        .py(px(10.))
        .rounded(px(10.))
        .bg(rgb(background))
        .text_size(px(13.))
        .line_height(relative(1.5))
        .text_color(rgb(foreground))
        .child(
            crate::icons::small(glyph)
                .mt(px(3.))
                .text_color(rgb(foreground)),
        )
        .child(div().flex_1().min_w_0().child(message.into()))
}

// ---------------------------------------------------------------------------
// Tooltip
// ---------------------------------------------------------------------------

/// Xylo owns tooltip presentation while GPUI owns placement and hover timing.
pub struct XyloTooltip(pub SharedString);
impl Render for XyloTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui_kit::base::Tooltip::new("xylo-tooltip")
            .px(px(10.))
            .py(px(6.))
            .max_w(px(420.))
            .rounded(px(8.))
            .bg(rgb(color(0x15171d, 0xeceef4)))
            .text_color(rgb(color(0xffffff, 0x15171d)))
            .text_size(px(12.))
            .shadow_md()
            .child(self.0.clone())
    }
}

// ---------------------------------------------------------------------------
// Switch
// ---------------------------------------------------------------------------

/// Compact Xylo switch. The unstyled native primitive retains keyboard and
/// accessibility semantics; the track, thumb and label are application-owned.
#[derive(IntoElement)]
pub struct XyloSwitch {
    id: ElementId,
    label: SharedString,
    /// Render only the track; the label stays the accessible name.
    label_hidden: bool,
    checked: bool,
    disabled: bool,
    change: Option<SwitchHandler>,
}
type SwitchHandler = std::rc::Rc<dyn Fn(&bool, &mut Window, &mut App)>;
impl XyloSwitch {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            label: "".into(),
            label_hidden: false,
            checked: false,
            disabled: false,
            change: None,
        }
    }
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = label.into();
        self
    }
    /// Shows only the track, for rows whose own text names the setting. The
    /// label is still the switch's accessible name.
    pub fn hide_label(mut self) -> Self {
        self.label_hidden = true;
        self
    }
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
    pub fn on_change(mut self, change: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.change = Some(std::rc::Rc::new(change));
        self
    }
}
impl RenderOnce for XyloSwitch {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let checked = self.checked;
        let disabled = self.disabled;
        let change = self.change;
        let visible_label = (!self.label_hidden).then(|| self.label.clone());
        gpui_kit::base::Switch::new(self.id)
            .checked(checked)
            .disabled(disabled)
            .accessibility_label(self.label.clone())
            .flex()
            .items_center()
            .gap_2()
            .min_h(px(28.))
            .rounded(px(8.))
            .border_1()
            .border_color(transparent_black())
            .focus_visible(|s| s.border_color(rgb(brand())))
            .text_size(px(13.))
            .text_color(rgb(if disabled { muted() } else { text() }))
            .cursor_pointer()
            .styles(|s| s.disabled(|s| s.cursor_default().opacity(0.5)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .w(px(34.))
                    .h(px(20.))
                    .px(px(2.))
                    .rounded_full()
                    .bg(rgb(if checked { brand() } else { border_strong() }))
                    .when(checked, |s| s.justify_end())
                    .child(
                        div()
                            .size(px(16.))
                            .rounded_full()
                            .bg(rgb(0xffffff))
                            .shadow_sm(),
                    ),
            )
            .children(visible_label)
            .on_change(move |checked, _, window, cx| {
                if let Some(change) = &change {
                    change(&checked, window, cx);
                }
            })
    }
}

// ---------------------------------------------------------------------------
// Toolkit theme sync
// ---------------------------------------------------------------------------

/// Push Xylo's palette into the toolkit theme so native primitives (inputs,
/// menus, popovers, scrollbars, markdown) paint with the same surfaces.
pub fn sync_toolkit_theme(cx: &mut App) {
    use gpui_kit::component::Theme;
    let hsla = |hex: u32| -> Hsla { rgb(hex).into() };
    Theme::update(cx, |theme| {
        theme.background = hsla(bg());
        theme.foreground = hsla(text());
        theme.border = hsla(border());
        theme.input = hsla(border_strong());
        theme.ring = hsla(brand());
        theme.caret = hsla(brand());
        theme.selection = hsla(color(0xdcdcff, 0x343a6a));
        theme.popover = hsla(panel());
        theme.popover_foreground = hsla(text());
        theme.muted = hsla(well());
        theme.muted_foreground = hsla(muted());
        theme.accent = hsla(hover());
        theme.accent_foreground = hsla(text());
        theme.primary = hsla(color(0x15171d, 0xeceef4));
        theme.primary_foreground = hsla(color(0xffffff, 0x15171d));
        theme.primary_hover = hsla(color(0x2b2f3a, 0xd3d7e2));
        theme.primary_active = hsla(color(0x3b4049, 0xb9bfcc));
        theme.secondary = hsla(well());
        theme.secondary_foreground = hsla(text());
        theme.secondary_hover = hsla(hover());
        theme.secondary_active = hsla(pressed());
        theme.button = hsla(panel());
        theme.button_foreground = hsla(text());
        theme.button_hover = hsla(hover_panel());
        theme.button_active = hsla(pressed());
        theme.colors.list = hsla(panel());
        theme.list_hover = hsla(hover_panel());
        theme.list_active = hsla(selected());
        theme.list_active_border = hsla(border_strong());
        theme.sidebar = hsla(sidebar());
        theme.sidebar_foreground = hsla(text());
        theme.sidebar_border = hsla(border());
        theme.sidebar_accent = hsla(hover());
        theme.sidebar_accent_foreground = hsla(text());
        theme.sidebar_primary = hsla(brand());
        theme.sidebar_primary_foreground = hsla(brand_foreground());
        theme.link = hsla(accent_text());
        theme.link_hover = hsla(brand());
        theme.link_active = hsla(brand_hover());
        theme.danger = hsla(danger());
        theme.danger_foreground = hsla(0xffffff);
        theme.success = hsla(success());
        theme.warning = hsla(warning());
        theme.info = hsla(accent_text());
        theme.scrollbar = hsla(bg());
        theme.scrollbar_thumb = hsla(color(0xc9cedb, 0x353b4a));
        theme.scrollbar_thumb_hover = hsla(color(0xb0b7c8, 0x4a5163));
        theme.tab_bar = hsla(sidebar());
        theme.tab = hsla(sidebar());
        theme.tab_active = hsla(panel());
        theme.tab_foreground = hsla(muted());
        theme.tab_active_foreground = hsla(text());
        theme.title_bar = hsla(sidebar());
        theme.title_bar_border = hsla(border());
        theme.switch = hsla(border_strong());
        theme.switch_thumb = hsla(0xffffff);
        theme.skeleton = hsla(well());
        theme.progress_bar = hsla(brand());
        theme.slider_bar = hsla(brand());
        theme.slider_thumb = hsla(panel());
        theme.drag_border = hsla(brand());
        theme.drop_target = hsla(brand_soft());
        theme.radius = px(8.);
        theme.radius_lg = px(12.);
        theme.font_size = px(14.);
    });
}

#[cfg(test)]
#[path = "ui/tests.rs"]
mod tests;
