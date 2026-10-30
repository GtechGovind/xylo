use super::*;
use std::fs;

fn skill(root: &std::path::Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[test]
fn forked_skill_applies_named_agent_instructions_and_intersects_tool_access() {
    let source = tempfile::tempdir().unwrap();
    skill(
        source.path(),
        "skills/review/SKILL.md",
        "---\nname: review\ncontext: fork\nagent: reviewer\nallowed-tools: Read, Write\n---\nReview $ARGUMENTS.",
    );
    skill(
        source.path(),
        "agents/reviewer.md",
        "---\nname: reviewer\ntools: Read, Grep\nmodel: inherit\n---\nReport evidence before conclusions.",
    );
    let home = tempfile::tempdir().unwrap();
    let service = ExtensionService::new(home.path());
    let package = service.install_local(source.path()).unwrap();
    service.set_enabled(&package.id, true).unwrap();
    let loaded = service
        .load_skill(&format!("{}:review", package.id), "the diff", false)
        .unwrap();
    assert!(loaded.fork && loaded.tools_restricted);
    assert!(loaded.body.contains("Report evidence before conclusions."));
    assert!(loaded.body.contains("Review the diff."));
    assert_eq!(loaded.allowed_tools, vec!["Read"]);
    assert_eq!(loaded.model.as_deref(), Some("inherit"));
}

#[test]
fn missing_named_agent_is_an_explicit_unsupported_requirement() {
    let source = tempfile::tempdir().unwrap();
    skill(
        source.path(),
        "SKILL.md",
        "---\nname: review\ncontext: fork\nagent: unavailable-role\n---\nReview this.",
    );
    let home = tempfile::tempdir().unwrap();
    let service = ExtensionService::new(home.path());
    let package = service.install_local(source.path()).unwrap();
    assert_eq!(
        package.components[0].readiness,
        Readiness::UnsupportedRequirement
    );
    assert!(
        package.components[0]
            .requirements
            .iter()
            .any(|r| r.contains("unavailable-role"))
    );
}

#[test]
fn unsupported_nested_agent_is_propagated_to_referencing_skill_at_install() {
    let source = tempfile::tempdir().unwrap();
    skill(
        source.path(),
        "skills/review/SKILL.md",
        "---\nname: review\ncontext: fork\nagent: reviewer\n---\nReview.",
    );
    skill(
        source.path(),
        "agents/reviewer.md",
        "---\nname: reviewer\ncontext: fork\nagent: other\n---\nDelegate again.",
    );
    let home = tempfile::tempdir().unwrap();
    let package = ExtensionService::new(home.path())
        .install_local(source.path())
        .unwrap();
    assert!(
        package
            .components
            .iter()
            .all(|c| c.readiness == Readiness::UnsupportedRequirement)
    );
}

#[test]
fn codex_metadata_disables_implicit_invocation_and_reports_required_integrations() {
    let source = tempfile::tempdir().unwrap();
    skill(
        source.path(),
        "SKILL.md",
        "---\nname: explicit\ndescription: Only when requested\n---\nReview changes.",
    );
    skill(
        source.path(),
        "agents/openai.yaml",
        "interface:\n  display_name: Review\npolicy:\n  allow_implicit_invocation: false\n",
    );
    let home = tempfile::tempdir().unwrap();
    let service = ExtensionService::new(home.path());
    let package = service.install_local(source.path()).unwrap();
    service.set_enabled(&package.id, true).unwrap();
    assert!(!service.skill_metadata().unwrap()[0].automatic);
    assert!(
        service
            .load_skill(&format!("{}:explicit", package.id), "", true)
            .is_err()
    );
    assert!(
        service
            .load_skill(&format!("{}:explicit", package.id), "", false)
            .is_ok()
    );
    skill(
        source.path(),
        "agents/openai.yaml",
        "dependencies:\n  tools:\n    - type: mcp\n      value: hosted-github\n      url: https://example.invalid/mcp\n",
    );
    let package = service.install_local(source.path()).unwrap();
    assert_eq!(
        package.components[0].readiness,
        Readiness::UnsupportedRequirement
    );
    assert!(
        package.components[0]
            .requirements
            .iter()
            .any(|r| r.contains("hosted-github"))
    );
}

#[test]
fn discovery_reads_frontmatter_without_loading_the_instruction_body() {
    let source = tempfile::tempdir().unwrap();
    let mut bytes = b"---\nname: lazy\ndescription: Metadata stays available\n---\n".to_vec();
    bytes.extend_from_slice(&[0xff, 0xfe]);
    fs::write(source.path().join("SKILL.md"), bytes).unwrap();
    let home = tempfile::tempdir().unwrap();
    let service = ExtensionService::new(home.path());
    let package = service.install_local(source.path()).unwrap();
    service.set_enabled(&package.id, true).unwrap();
    assert_eq!(service.skill_metadata().unwrap()[0].name, "lazy");
    assert!(
        service
            .load_skill(&format!("{}:lazy", package.id), "", false)
            .is_err()
    );
}

#[test]
fn untrusted_yaml_expansion_is_bounded_but_small_aliases_remain_usable() {
    let valid: serde_json::Value = super::manifest::parse_yaml("name: &name review\ncopy: *name\n").unwrap();
    assert_eq!(valid["copy"], "review");
    let amplified = format!("name: test\nmetadata:\n  anchor: &big {}\n  copies: [{}]\n", "a".repeat(8192), vec!["*big"; 1000].join(","));
    assert!(amplified.len() < 256 * 1024);
    assert!(super::manifest::parse_yaml::<serde_json::Value>(&amplified).is_err());
    let nested = format!("{}0{}", "[".repeat(64), "]".repeat(64));
    assert!(super::manifest::parse_yaml::<serde_json::Value>(&nested).is_err());
    assert!(super::manifest::parse_yaml::<serde_json::Value>("private: !include /etc/passwd").is_err());
}

#[test]
fn github_directory_sources_keep_legacy_skill_urls_and_filter_discovery() {
    let source = ExtensionSource::from_input(
        "https://github.com/openai/skills/tree/main/skills/.system/skill-creator",
    )
    .unwrap();
    assert_eq!(source.repository, "openai/skills");
    assert_eq!(source.revision, "main");
    assert_eq!(source.subdirectory, "skills/.system/skill-creator");
    let tree = serde_json::json!({"truncated":false,"tree":[{"path":"skills/.system/skill-creator/SKILL.md"},{"path":"skills/other/SKILL.md"}]});
    let packages = super::sources::packages_from_tree(
        &tree,
        &source,
        "1234567890123456789012345678901234567890",
    )
    .unwrap();
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0].path, source.subdirectory);
    let containing_plugin = serde_json::json!({"truncated":false,"tree":[{"path":".codex-plugin/plugin.json"},{"path":"skills/.system/skill-creator/SKILL.md"},{"path":"skills/other/SKILL.md"}]});
    let selected = super::sources::packages_from_tree(&containing_plugin, &source, "1234567890123456789012345678901234567890").unwrap();
    assert_eq!(selected.len(), 1, "A directory URL can select a standalone skill inside a larger package");
    assert_eq!(selected[0].path, source.subdirectory);
    assert_eq!(
        ExtensionSource::from_input("owner/repository.git")
            .unwrap()
            .revision,
        "HEAD"
    );
    assert!(ExtensionSource::from_input("https://github.com.evil.invalid/owner/repo").is_err());
    assert!(ExtensionSource::from_input("https://secret@github.com/owner/repo").is_err());
}

#[test]
fn install_keeps_resources_lazy_and_invocation_preserves_arguments() {
    let source = tempfile::tempdir().unwrap();
    skill(
        source.path(),
        "SKILL.md",
        "---\nname: review\ndescription: Review a patch\n---\nReview $ARGUMENTS using references/rules.md.",
    );
    skill(
        source.path(),
        "references/rules.md",
        "Secret body should not be in discovery",
    );
    let home = tempfile::tempdir().unwrap();
    let service = ExtensionService::new(home.path());
    let package = service.install_local(source.path()).unwrap();
    assert_eq!(package.components.len(), 1);
    assert_eq!(package.components[0].readiness, Readiness::Installed);
    service.set_enabled(&package.id, true).unwrap();
    let metadata = service.skill_metadata().unwrap();
    assert_eq!(metadata.len(), 1);
    assert!(
        !serde_json::to_string(&metadata)
            .unwrap()
            .contains("Secret body")
    );
    let loaded = service
        .load_skill(&metadata[0].id, "src/a.rs $HOME", false)
        .unwrap();
    assert!(loaded.body.contains("Review src/a.rs $HOME"));
    assert_eq!(
        service
            .read_resource(&package.id, "references/rules.md")
            .unwrap(),
        "Secret body should not be in discovery"
    );
    assert!(service.read_resource(&package.id, "../outside").is_err());
    service.set_enabled(&package.id, false).unwrap();
    assert!(service.skill_metadata().unwrap().is_empty());
    service.remove(&package.id).unwrap();
    assert!(service.list().unwrap().is_empty());
}

#[test]
fn mixed_package_exposes_portable_parts_and_reports_host_requirements() {
    let source = tempfile::tempdir().unwrap();
    skill(
        source.path(),
        ".codex-plugin/plugin.json",
        r#"{"name":"mixed","version":"1.2","skills":"./skills","apps":"./.app.json"}"#,
    );
    skill(
        source.path(),
        "skills/review/SKILL.md",
        "---\nname: review\ndescription: Review code\ndisable-model-invocation: true\n---\nReview $ARGUMENTS.",
    );
    skill(
        source.path(),
        ".app.json",
        r#"{"apps":{"drive":{"id":"host-only"}}}"#,
    );
    let home = tempfile::tempdir().unwrap();
    let service = ExtensionService::new(home.path());
    let p = service.install_local(source.path()).unwrap();
    assert_eq!(p.version, "1.2");
    assert!(p.components.iter().any(
        |c| c.kind == ComponentKind::HostedIntegration && c.readiness == Readiness::NeedsSetup
    ));
    service.set_enabled(&p.id, true).unwrap();
    let id = format!("{}:review", p.id);
    assert!(service.load_skill(&id, "", true).is_err());
    assert!(service.load_skill(&id, "", false).is_ok());
}

#[test]
fn update_changes_digest_and_clears_execution_evidence() {
    let source = tempfile::tempdir().unwrap();
    skill(
        source.path(),
        "SKILL.md",
        "---\nname: test\ndescription: test\n---\nFirst",
    );
    let home = tempfile::tempdir().unwrap();
    let service = ExtensionService::new(home.path());
    let first = service.install_local(source.path()).unwrap();
    service.set_enabled(&first.id, true).unwrap();
    service
        .record_execution(&first.id, &first.digest, "test", "fixture run")
        .unwrap();
    assert_eq!(
        service.list().unwrap()[0].components[0].readiness,
        Readiness::ExecutionVerified
    );
    skill(
        source.path(),
        "SKILL.md",
        "---\nname: test\ndescription: test\n---\nSecond",
    );
    let second = service.install_local(source.path()).unwrap();
    assert_ne!(first.digest, second.digest);
    assert!(second.enabled);
    assert_eq!(second.components[0].readiness, Readiness::Installed);
    assert!(
        service
            .record_execution(&first.id, &first.digest, "test", "stale run")
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn package_cannot_escape_through_symlink() {
    let source = tempfile::tempdir().unwrap();
    skill(
        source.path(),
        "SKILL.md",
        "---\nname: test\ndescription: test\n---\nText",
    );
    std::os::unix::fs::symlink("/etc", source.path().join("escape")).unwrap();
    let home = tempfile::tempdir().unwrap();
    assert!(
        ExtensionService::new(home.path())
            .install_local(source.path())
            .is_err()
    );
}

#[test]
fn legacy_command_and_agent_definitions_are_separate_components() {
    let source = tempfile::tempdir().unwrap();
    skill(
        source.path(),
        ".claude-plugin/plugin.json",
        r#"{"name":"helpers"}"#,
    );
    skill(
        source.path(),
        "commands/review.md",
        "---\ndescription: Review\n---\nReview $1 with $ARGUMENTS",
    );
    skill(
        source.path(),
        "agents/reviewer.md",
        "---\nname: reviewer\ndescription: Independent review\ntools: Read, Grep\n---\nFind defects.",
    );
    let home = tempfile::tempdir().unwrap();
    let service = ExtensionService::new(home.path());
    let p = service.install_local(source.path()).unwrap();
    assert!(
        p.components
            .iter()
            .any(|c| c.kind == ComponentKind::Command)
    );
    assert!(p.components.iter().any(|c| c.kind == ComponentKind::Agent));
}

#[test]
fn quoted_zero_based_arguments_and_tool_rules_preserve_meaning() {
    let source = tempfile::tempdir().unwrap();
    skill(
        source.path(),
        "SKILL.md",
        "---\nname: args\ndescription: arguments\nallowed-tools: Read, Bash(git status *)\n---\n$0|$1|$ARGUMENTS[0]|$ARGUMENTS",
    );
    let home = tempfile::tempdir().unwrap();
    let service = ExtensionService::new(home.path());
    let p = service.install_local(source.path()).unwrap();
    service.set_enabled(&p.id, true).unwrap();
    let loaded = service
        .load_skill(&format!("{}:args", p.id), "\"hello world\" second", false)
        .unwrap();
    assert_eq!(
        loaded.body,
        "hello world|second|hello world|\"hello world\" second"
    );
    assert_eq!(loaded.allowed_tools, vec!["Read", "Bash(git status *)"]);
}

#[cfg(unix)]
#[test]
fn executable_mode_and_display_name_updates_invalidate_snapshot_without_duplicate() {
    use std::os::unix::fs::PermissionsExt;
    let source = tempfile::tempdir().unwrap();
    skill(
        source.path(),
        ".codex-plugin/plugin.json",
        r#"{"name":"old"}"#,
    );
    skill(
        source.path(),
        "skills/a/SKILL.md",
        "---\nname: a\ndescription: a\n---\nA",
    );
    skill(source.path(), "script.sh", "exit 0");
    std::fs::set_permissions(
        source.path().join("script.sh"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    let service = ExtensionService::new(home.path());
    let first = service.install_local(source.path()).unwrap();
    std::fs::set_permissions(
        source.path().join("script.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let second = service.install_local(source.path()).unwrap();
    assert_ne!(first.digest, second.digest);
    skill(
        source.path(),
        ".codex-plugin/plugin.json",
        r#"{"name":"new"}"#,
    );
    let renamed = service.install_local(source.path()).unwrap();
    assert_eq!(first.id, renamed.id);
    assert_eq!(service.list().unwrap().len(), 1);
}

/// Explicit qualification against a public upstream. Ordinary tests stay offline;
/// this copies a pinned package into a temporary catalog and never executes it.
#[tokio::test]
#[ignore = "Reads public GitHub; run explicitly for remote installation qualification"]
async fn public_github_discovery_installs_a_real_commit_pinned_skill_snapshot() {
    let source = ExtensionSource::from_input("openai/skills").unwrap();
    let discovered = discover(&source).await.unwrap();
    let item = discovered
        .into_iter()
        .find(|item| item.name == "skill-creator")
        .expect("The public qualification source must contain skill-creator");
    let commit: serde_json::Value = reqwest::Client::new()
        .get(format!(
            "https://api.github.com/repos/{}/commits/{}",
            source.repository, item.revision
        ))
        .header("user-agent", "Xylo-extension-qualification")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(commit["sha"].as_str(), Some(item.revision.as_str()));
    assert_ne!(commit["commit"]["tree"]["sha"], commit["sha"]);
    let home = tempfile::tempdir().unwrap();
    let service = ExtensionService::new(home.path());
    let package = service.install_discovered(&item).await.unwrap();
    assert_eq!(package.revision.as_deref(), Some(item.revision.as_str()));
    assert!(package.source.contains("github.com/openai/skills"));
    assert!(package.source.ends_with(&item.path));
    assert_eq!(package.digest.len(), 64);
    assert!(!package.enabled);
    assert!(
        package
            .components
            .iter()
            .any(|component| component.kind == ComponentKind::Skill)
    );
    assert!(
        service
            .package_root(&package)
            .unwrap()
            .join("SKILL.md")
            .is_file()
    );
    assert_eq!(service.list().unwrap().len(), 1);
    println!(
        "Qualified public package: {} revision={} digest={}",
        package.source, item.revision, package.digest
    );
}

#[test]
fn source_discovery_preserves_repository_root_packages_and_commit_identity() {
    let source = ExtensionSource {
        name: "Fixture source".into(),
        repository: "fixture/root-plugin".into(),
        revision: "main".into(),
        subdirectory: String::new(),
    };
    let commit = "1234567890123456789012345678901234567890";
    for root_marker in [
        "SKILL.md",
        ".codex-plugin/plugin.json",
        ".claude-plugin/plugin.json",
        ".agent-plugin/plugin.json",
    ] {
        let tree = serde_json::json!({
            "sha": "different-tree-object-id",
            "truncated": false,
            "tree": [
                {"path": root_marker, "type": "blob"},
                {"path": "skills/nested/SKILL.md", "type": "blob"}
            ]
        });
        let packages = super::sources::packages_from_tree(&tree, &source, commit).unwrap();
        assert_eq!(packages.len(), 1, "Root marker {root_marker}");
        assert_eq!(packages[0].path, ".");
        assert_eq!(packages[0].name, "root-plugin");
        assert_eq!(packages[0].revision, commit);
    }
}

#[test]
fn source_discovery_groups_each_manifest_format_without_duplicate_nested_skills() {
    let source = ExtensionSource {
        name: "Fixture source".into(),
        repository: "fixture/monorepo".into(),
        revision: "main".into(),
        subdirectory: String::new(),
    };
    let tree = serde_json::json!({"truncated": false, "tree": [
        {"path": "plugins/a/.codex-plugin/plugin.json", "type": "blob"},
        {"path": "plugins/a/skills/review/SKILL.md", "type": "blob"},
        {"path": "plugins/b/.claude-plugin/plugin.json", "type": "blob"},
        {"path": "plugins/b/skills/review/SKILL.md", "type": "blob"},
        {"path": "plugins/c/.agent-plugin/plugin.json", "type": "blob"},
        {"path": "plugins/c/skills/review/SKILL.md", "type": "blob"},
        {"path": "skills/standalone/SKILL.md", "type": "blob"}
    ]});
    let packages = super::sources::packages_from_tree(
        &tree,
        &source,
        "1234567890123456789012345678901234567890",
    )
    .unwrap();
    assert_eq!(
        packages.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(),
        vec!["plugins/a", "plugins/b", "plugins/c", "skills/standalone"]
    );
}
