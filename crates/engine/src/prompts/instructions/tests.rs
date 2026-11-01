//! User-level precedence and import bounds are independent of the real home directory.
use super::*;
#[test]
fn user_files_precede_project_and_imports_stop_after_five_levels() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("repo");
    let home = temp.path().join("home");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(home.join(".xylo")).unwrap();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(home.join(".xylo/AGENTS.md"), "User convention").unwrap();
    std::fs::write(home.join(".claude/CLAUDE.md"), "Other user convention").unwrap();
    std::fs::write(home.join("shared.md"), "Home import").unwrap();
    std::fs::write(
        workspace.join("AGENTS.md"),
        "Project convention. See @1.md and @~/shared.md",
    )
    .unwrap();
    for index in 1..=6 {
        std::fs::write(
            workspace.join(format!("{index}.md")),
            format!("Level{index}\n@{}.md", index + 1),
        )
        .unwrap();
    }
    let text = project_instructions_with_home(&workspace, Some(&home), 4096);
    assert!(text.find("User convention").unwrap() < text.find("Project convention").unwrap());
    assert!(text.contains("Home import"));
    assert!(text.contains("Level5"));
    assert!(!text.contains("Level6"));
}
#[test]
fn rule_globs_keep_brace_alternatives_and_obey_the_budget() {
    assert!(
        matching_rule(
            "---\npaths: [\"src/**/*.{rs,toml}\", \"tests/**\"]\n---\nRule",
            "src/lib.rs"
        )
        .is_some()
    );
    assert!(
        matching_rule(
            "---\npaths: [\"src/**/*.{rs,toml}\"]\n---\nRule",
            "readme.md"
        )
        .is_none()
    );
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join(".xylo/rules")).unwrap();
    std::fs::write(temp.path().join("AGENTS.md"), "Root convention ".repeat(30)).unwrap();
    std::fs::write(
        temp.path().join(".xylo/rules/all.md"),
        "Scoped convention ".repeat(100),
    )
    .unwrap();
    std::fs::write(temp.path().join("readme.md"), "").unwrap();
    let startup = project_instructions(temp.path(), 1024);
    let scoped = ScopedInstructions::default().touch(temp.path(), Path::new("readme.md"), 1024);
    assert!(startup.len() + scoped.len() <= 1024);
}
