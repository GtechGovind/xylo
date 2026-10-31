use super::{SINGLE_LINE_HEIGHT, SingleLine};
use gpui_kit::{Styled, div, relative};

#[test]
fn single_line_clips_with_a_line_box_tall_enough_for_descenders() {
    let mut label = div().single_line();
    let style = label.style();
    assert_eq!(style.text.line_height, Some(relative(SINGLE_LINE_HEIGHT)));
    assert!(style.text.text_overflow.is_some());
}
