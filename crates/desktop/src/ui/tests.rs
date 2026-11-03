use super::{SINGLE_LINE_HEIGHT, SingleLine};
use gpui_kit::{Styled, div, relative};

#[test]
fn single_line_clips_with_a_line_box_tall_enough_for_descenders() {
    let mut label = div().single_line();
    let style = label.style();
    assert_eq!(style.text.line_height, Some(relative(SINGLE_LINE_HEIGHT)));
    assert!(style.text.text_overflow.is_some());
}

#[test]
fn native_icons_use_the_packaged_logo_at_every_shell_size() {
    let source = image::load_from_memory_with_format(
        include_bytes!("../../assets/xylo-icon.png"),
        image::ImageFormat::Png,
    )
    .unwrap()
    .to_rgba8();
    for size in [16, 24, 32, 48, 64, 128, 256, 512] {
        let expected =
            image::imageops::resize(&source, size, size, image::imageops::FilterType::Lanczos3);
        assert!(
            super::brand_pixels(size) == expected,
            "logo differs at {size}px"
        );
    }
}
