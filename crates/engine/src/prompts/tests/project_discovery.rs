//! Project instruction discovery regressions.
use super::*;

#[test]
fn local_instructions_and_relative_imports_are_loaded() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("AGENTS.local.md"),
        "Local convention\n@details.md",
    )
    .unwrap();
    std::fs::write(temp.path().join("details.md"), "Imported convention").unwrap();
    let text = project_instructions(temp.path(), 4096);
    assert!(text.contains("Local convention"));
    assert!(text.contains("Imported convention"));
}

#[test]
fn path_rules_and_nested_instructions_load_only_once_on_matching_touch() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join(".claude/rules")).unwrap();
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(temp.path().join("src/app.rs"), "").unwrap();
    std::fs::write(temp.path().join("src/AGENTS.md"), "Nested convention").unwrap();
    std::fs::write(
        temp.path().join(".claude/rules/rust.md"),
        "---\npaths: [\"src/**/*.rs\"]\n---\nRust convention",
    )
    .unwrap();
    assert!(!project_instructions(temp.path(), 4096).contains("Rust convention"));
    let mut scoped = crate::prompts::ScopedInstructions::default();
    assert!(
        !scoped
            .touch(temp.path(), std::path::Path::new("README.md"), 4096)
            .contains("Rust convention")
    );
    let first = scoped.touch(temp.path(), std::path::Path::new("src/app.rs"), 4096);
    assert!(first.contains("Nested convention"));
    assert!(first.contains("Rust convention"));
    assert!(
        scoped
            .touch(temp.path(), std::path::Path::new("src/app.rs"), 4096)
            .is_empty()
    );
}

#[test]
fn instruction_imports_stop_cycles_and_outside_paths() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(temp.path().join("outside.md"), "Do not load this").unwrap();
    std::fs::write(root.join("AGENTS.md"), "Start\n@a.md\n@../outside.md").unwrap();
    std::fs::write(root.join("a.md"), "Imported\n@AGENTS.md").unwrap();
    let text = project_instructions(&root, 2048);
    assert_eq!(text.matches("Start").count(), 1);
    assert_eq!(text.matches("Imported").count(), 1);
    assert!(!text.contains("Do not load this"));
}
