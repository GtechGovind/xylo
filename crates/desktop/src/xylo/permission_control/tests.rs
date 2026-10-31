use super::{permission_color, permission_glyph};
use crate::ui::warning;
use xylo_core::PermissionMode;

#[test]
fn every_preset_has_a_distinct_icon_and_only_full_access_warns() {
    let glyphs: Vec<_> = PermissionMode::ALL
        .iter()
        .map(|mode| format!("{:?}", permission_glyph(*mode)))
        .collect();
    let mut unique = glyphs.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), glyphs.len());
    for mode in PermissionMode::ALL {
        assert_eq!(
            permission_color(mode) == warning(),
            mode == PermissionMode::FullAccess
        );
    }
}
