use super::{AppAssets, SOFT_STROKE_WIDTH, with_root_stroke_width};
use crate::icons::AppIcon;
use crate::ui::XYLO_MARK;
use gpui_kit::AssetSource;
use gpui_kit::component::IconNamed;

fn rewrite(svg: &str) -> Option<String> {
    with_root_stroke_width(svg.as_bytes(), "1.5").map(|bytes| String::from_utf8(bytes).unwrap())
}

#[test]
fn rewrites_only_the_root_stroke_width() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" stroke-width="2" stroke-linecap="round"><path stroke-width="2" d="M5 12h14"/></svg>"#;
    assert_eq!(
        rewrite(svg).unwrap(),
        r#"<svg xmlns="http://www.w3.org/2000/svg" stroke-width="1.5" stroke-linecap="round"><path stroke-width="2" d="M5 12h14"/></svg>"#
    );
}

#[test]
fn handles_multiline_roots_prologs_and_single_quotes() {
    let svg = "<?xml version=\"1.0\"?>\n<svg\n  width=\"24\"\n  stroke-width='2'\n>\n<path d=\"M0 0\"/></svg>";
    assert_eq!(
        rewrite(svg).unwrap(),
        "<?xml version=\"1.0\"?>\n<svg\n  width=\"24\"\n  stroke-width='1.5'\n>\n<path d=\"M0 0\"/></svg>"
    );
}

#[test]
fn ignores_lookalike_attributes_and_stroke_less_roots() {
    // The name inside another attribute's value is not the attribute.
    let lookalike =
        r#"<svg class="stroke-width" data-stroke-width="2"><g stroke-width="2"/></svg>"#;
    assert_eq!(rewrite(lookalike), None);
    let filled = r#"<svg viewBox="0 0 24 24"><path d="M0 0" fill="currentColor"/></svg>"#;
    assert_eq!(rewrite(filled), None);
    let already = r#"<svg stroke-width="1.5"></svg>"#;
    assert_eq!(rewrite(already), None);
    assert_eq!(with_root_stroke_width(&[0xff, 0xfe], "1.5"), None);
}

#[test]
fn every_served_outline_icon_is_soft_and_round() {
    let paths = AppAssets.list("icons/").unwrap();
    assert!(paths.len() > 100);
    for path in paths.iter().filter(|path| path.as_ref() != XYLO_MARK) {
        let svg = String::from_utf8(AppAssets.load(path).unwrap().unwrap().into_owned()).unwrap();
        let root = &svg[..svg.find('>').unwrap()];
        if !root.contains("stroke=\"currentColor\"") {
            continue;
        }
        assert!(
            root.contains(&format!("stroke-width=\"{SOFT_STROKE_WIDTH}\"")),
            "{path} keeps a heavy stroke"
        );
        assert!(
            root.contains("stroke-linecap=\"round\""),
            "{path} has square caps"
        );
        assert!(
            root.contains("stroke-linejoin=\"round\""),
            "{path} has sharp joins"
        );
    }
}

#[test]
fn repeated_loads_serve_the_same_cached_bytes() {
    let path = AppIcon::More.path();
    let first = AppAssets.load(&path).unwrap().unwrap();
    let second = AppAssets.load(&path).unwrap().unwrap();
    assert!(std::ptr::eq(first.as_ptr(), second.as_ptr()));
}

#[test]
fn every_app_icon_is_served() {
    for &concept in AppIcon::ALL {
        let path = concept.path();
        assert!(
            matches!(AppAssets.load(&path), Ok(Some(_))),
            "{concept:?} ({path}) is not embedded; add it to ExtraIcons"
        );
    }
}

#[test]
fn unknown_paths_keep_the_toolkit_result() {
    assert!(AppAssets.load("icons/not-an-icon.svg").is_err());
    assert!(matches!(AppAssets.load(""), Ok(None)));
}
