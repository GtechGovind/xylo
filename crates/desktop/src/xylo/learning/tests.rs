use super::{BuiltInSkill, built_in_skill, built_in_skills, procedure_state};
use std::collections::BTreeSet;

#[test]
fn procedure_states_read_as_words() {
    assert_eq!(procedure_state("candidate"), "Candidate");
    assert_eq!(procedure_state("promoted"), "Promoted");
    assert_eq!(procedure_state("suspended"), "Suspended");
    assert_eq!(procedure_state("needs_review"), "Needs review");
}

#[test]
fn built_in_skills_read_name_and_description_from_frontmatter() {
    let skill = built_in_skill("---\nname: git-commit\ndescription: Write a commit.\n---\nBody\n");
    assert_eq!(
        skill,
        Some(BuiltInSkill {
            name: "git-commit".into(),
            description: "Write a commit.".into(),
        })
    );
    assert_eq!(built_in_skill("name: x\ndescription: y\n"), None);
    assert_eq!(built_in_skill("---\nname: x\n---\n"), None);
}

#[test]
fn every_engine_skill_is_listed_once() {
    // The validation crate builds these files from another manifest
    // directory; find `crates/` from wherever the manifest is.
    let skills = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .map(|dir| dir.join("engine/skills"))
        .find(|dir| dir.is_dir())
        .expect("crates/engine/skills");
    let folders: BTreeSet<String> = std::fs::read_dir(skills)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            entry
                .path()
                .join("SKILL.md")
                .is_file()
                .then(|| entry.file_name().to_string_lossy().into_owned())
        })
        .collect();
    let listed: BTreeSet<String> = built_in_skills()
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    assert_eq!(listed, folders);
}
