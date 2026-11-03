//! Chat presentation values. Reuse the application semantic color palette.
pub(in crate::xylo) const BODY_SIZE: f32 = 15.;
pub(in crate::xylo) const DETAIL_SIZE: f32 = 13.;
pub(in crate::xylo) const META_SIZE: f32 = 12.;
pub(in crate::xylo) const CONTROL_SIZE: f32 = 34.;
pub(in crate::xylo) const PRIMARY_CONTROL_SIZE: f32 = 36.;
pub(in crate::xylo) const COLUMN_WIDTH: f32 = 800.;
pub(in crate::xylo) const RESPONSE_LINE_HEIGHT: f32 = 1.75;
pub(in crate::xylo) const CHAT_INSET: f32 = 30.;
pub(in crate::xylo) const COMPOSER_RADIUS: f32 = 18.;
pub(in crate::xylo) const COMPOSER_INPUT_MIN_HEIGHT: f32 = 53.;

/// Markdown rhythm from the approved chat draft, scoped to conversation text.
pub(in crate::xylo) fn response_style() -> gpui_kit::base::text::TextViewStyle {
    use crate::ui;
    use gpui_kit::{FontWeight, StyleRefinement, Styled, px, rems, rgb};
    gpui_kit::base::text::TextViewStyle::default()
        .with_foreground(rgb(ui::text()).into())
        .with_muted_foreground(rgb(ui::text_secondary()).into())
        .with_link(rgb(ui::accent_text()).into())
        .with_code_background(rgb(ui::well()).into())
        // The default inline highlight carries a light-theme background of
        // its own; changing the fenced-code color does not replace it.
        .with_inline_code(gpui_kit::HighlightStyle {
            color: Some(rgb(ui::text()).into()),
            background_color: Some(rgb(ui::well()).into()),
            ..Default::default()
        })
        .with_border(rgb(ui::border_strong()).into())
        .with_dark(ui::is_dark())
        .with_selection(rgb(ui::selected()).into())
        .with_paragraph_gap(rems(16. / 14.))
        .with_heading(|level| {
            StyleRefinement::default()
                .text_size(px(match level {
                    1 => 24.,
                    2 => 20.,
                    _ => 15.,
                }))
                .font_weight(FontWeight::SEMIBOLD)
                .line_height(gpui_kit::relative(1.4))
        })
}
