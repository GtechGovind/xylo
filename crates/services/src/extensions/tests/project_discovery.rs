//! Discovery is inert until snapshot approval and is isolated by workspace.
use super::*;

#[test]
fn discovered_packages_require_enablement_and_updates_revoke_it() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("repo");
    let skills = workspace.join(".claude/skills/check");
    std::fs::create_dir_all(&skills).unwrap();
    std::fs::write(
        skills.join("SKILL.md"),
        "---\nname: check\ndescription: Project check\n---\nInspect the project",
    )
    .unwrap();
    let service = ExtensionService::new(temp.path().join("catalog"));
    let packages = service.discover_project(&workspace, None).unwrap();
    assert_eq!(packages.len(), 1);
    assert!(!packages[0].enabled);
    assert!(service.skill_metadata().unwrap().is_empty());
    service.set_enabled(&packages[0].id, true).unwrap();
    assert_eq!(
        service
            .clone()
            .for_workspace(&workspace)
            .skill_metadata()
            .unwrap()
            .len(),
        1
    );
    let other = temp.path().join("other");
    std::fs::create_dir(&other).unwrap();
    assert!(
        service
            .clone()
            .for_workspace(&other)
            .skill_metadata()
            .unwrap()
            .is_empty()
    );
    std::fs::write(
        skills.join("SKILL.md"),
        "---\nname: check\ndescription: Changed check\n---\nChanged instructions",
    )
    .unwrap();
    assert!(!service.discover_project(&workspace, None).unwrap()[0].enabled);
}
