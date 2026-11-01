//! Sources remain attached to expanded content and only applicable rules emit them.
use super::*;
#[test]
fn startup_sources_include_nested_home_imports_but_not_omitted_text() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let home = temp.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(root.join("AGENTS.md"), "@~/guide.md").unwrap();
    std::fs::write(home.join("guide.md"), "@nested.md").unwrap();
    std::fs::write(home.join("nested.md"), "External instruction").unwrap();
    let (text, sources) = project_instruction_sources_with_home(&root, Some(&home), 4096);
    assert!(text.contains("External instruction"));
    assert!(sources.contains(&home.join("nested.md").canonicalize().unwrap()));
    let (_, sources) = project_instruction_sources_with_home(&root, Some(&home), 0);
    assert!(sources.is_empty());
}
#[test]
fn only_matching_lazy_rules_emit_import_sources() {
    let temp = tempfile::tempdir().unwrap();
    let home = tempfile::Builder::new()
        .prefix("xylo-rule-sources-")
        .tempdir_in(std::env::var_os("HOME").unwrap())
        .unwrap();
    let source = home.path().join("guide.md");
    std::fs::write(&source, "Outside matched rule").unwrap();
    std::fs::create_dir_all(temp.path().join(".xylo/rules")).unwrap();
    std::fs::write(
        temp.path().join(".xylo/rules/rust.md"),
        format!("---\npaths: [*.rs]\n---\n@{}", source.display()),
    )
    .unwrap();
    std::fs::write(temp.path().join("readme.md"), "").unwrap();
    std::fs::write(temp.path().join("main.rs"), "").unwrap();
    let mut scoped = ScopedInstructions::default();
    assert!(
        scoped
            .touch_with_sources(temp.path(), Path::new("readme.md"), 4096)
            .1
            .is_empty()
    );
    let (text, sources) = scoped.touch_with_sources(temp.path(), Path::new("main.rs"), 4096);
    assert!(text.contains("Outside matched rule"));
    assert!(sources.contains(&source.canonicalize().unwrap()));
    assert!(
        scoped
            .touch_with_sources(temp.path(), Path::new("main.rs"), 4096)
            .1
            .is_empty()
    );
}
