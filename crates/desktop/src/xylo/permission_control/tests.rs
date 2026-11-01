use super::permission_glyph;
use xylo_core::PermissionMode;

#[test]
fn every_preset_has_a_distinct_icon() {
    let glyphs: Vec<_> = PermissionMode::ALL
        .iter()
        .map(|mode| format!("{:?}", permission_glyph(*mode)))
        .collect();
    let mut unique = glyphs.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), glyphs.len());
}
