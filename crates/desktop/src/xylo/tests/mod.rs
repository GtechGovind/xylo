use super::{language, park_terminal, truncate};
use std::{collections::BTreeMap, path::Path};
#[test]
fn leaving_a_project_parks_its_terminal_under_the_root() {
    let mut cache = BTreeMap::new();
    park_terminal(&mut cache, Some("/project".into()), Some("shell"));
    assert_eq!(cache.get(Path::new("/project")), Some(&"shell"));
    park_terminal(&mut cache, None, Some("orphan"));
    assert_eq!(cache.len(), 1);
}
#[test]
fn truncation_is_unicode_safe() {
    assert_eq!(truncate("界界界", 2), "界界\n… (truncated for display)");
}
#[test]
fn language_uses_extension() {
    assert_eq!(language(Path::new("src/lib.rs")), "rust");
}
