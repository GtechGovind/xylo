use super::{AppIcon, BASE, HIT_AREA, SMALL, icon, icon_button, small};
use gpui_kit::{Styled, px};
use std::collections::HashMap;

#[test]
fn each_glyph_means_one_concept() {
    let mut owners = HashMap::new();
    for &concept in AppIcon::ALL {
        if let Some(other) = owners.insert(concept.glyph(), concept) {
            panic!("{concept:?} and {other:?} share {:?}", concept.glyph());
        }
    }
}

#[test]
fn icons_use_the_two_size_tokens() {
    assert_eq!(SMALL, px(14.));
    assert_eq!(BASE, px(16.));
    assert_eq!(icon(AppIcon::Search).style().size.width, Some(BASE.into()));
    assert_eq!(
        small(AppIcon::Folder).style().size.width,
        Some(SMALL.into())
    );
}

#[test]
fn only_status_concepts_carry_a_tone() {
    let toned: Vec<_> = AppIcon::ALL
        .iter()
        .filter(|icon| icon.tone().is_some())
        .collect();
    assert_eq!(
        toned,
        [
            &AppIcon::Success,
            &AppIcon::Warning,
            &AppIcon::Error,
            &AppIcon::Info
        ]
    );
}

#[test]
fn icon_buttons_share_one_hit_area() {
    let mut button = icon_button("search", AppIcon::Search, "Search");
    let style = button.style();
    assert_eq!(style.size.width, Some(HIT_AREA.into()));
    assert_eq!(style.size.height, Some(HIT_AREA.into()));
}
