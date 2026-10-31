use super::super::tests::fixture;
use super::*;
use xylo_core::PermissionMode;

const PROJECT: Access = Access {
    outside: false,
    sandboxed: true,
    network: false,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_preview_is_bounded_and_never_repeats_the_content() {
    let (directory, broker, _events) = fixture(PermissionMode::AskForApproval);
    let content = format!(
        "{}\nFINAL_ACTION_SENTINEL\n",
        "long change line\n".repeat(3000)
    );
    let args = json!({"path":"new.txt", "content":content});
    let preview = change_preview(&broker, "file_write", &args, PROJECT).unwrap();
    assert!(
        preview["diff"]
            .as_str()
            .unwrap()
            .contains("+FINAL_ACTION_SENTINEL")
    );
    assert_eq!(preview["diff_truncated"], false);
    assert_eq!(
        preview["request"],
        json!({"path":"new.txt","bytes":content.len()})
    );
    assert!(!directory.path().join("new.txt").exists());
    let huge = "x\n".repeat(WRITE_BYTES / 2 - 8);
    let preview = change_preview(
        &broker,
        "file_write",
        &json!({"path":"big.txt","content":huge}),
        PROJECT,
    )
    .unwrap();
    assert_eq!(preview["diff_truncated"], true);
    // JSON escaping can grow the bounded diff, never past twice its size.
    assert!(serde_json::to_vec(&preview).unwrap().len() < 2 * APPROVAL_DIFF_BYTES);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paths_outside_need_access_and_symlinks_cannot_hide_them() {
    let (directory, broker, _events) = fixture(PermissionMode::AskForApproval);
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), directory.path().join("escape")).unwrap();
    for requested in ["../outside-file", "escape/file"] {
        assert!(matches!(
            resolve(&broker, requested, true).unwrap(),
            Resolved::Outside(_)
        ));
        assert!(write_target(&broker, requested, PROJECT).is_err());
    }
    let approved = Access {
        outside: true,
        ..PROJECT
    };
    assert!(write_target(&broker, "escape/file", approved).is_ok());
    assert!(matches!(
        resolve(&broker, "new.rs", true).unwrap(),
        Resolved::Project(_)
    ));
    let data = broker.artifacts.parent().unwrap().join("xylo.db");
    let error = write_target(&broker, &data.to_string_lossy(), approved).unwrap_err();
    assert!(error.to_string().contains("application data"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_metadata_and_agent_configuration_are_never_written() {
    let (directory, broker, _events) = fixture(PermissionMode::FullAccess);
    let root = directory.path();
    for folder in [
        ".git/hooks",
        ".GIT",
        "vendor/lib/.git",
        ".agents/skills",
        ".claude",
        ".codex",
        "docs/.claude",
        ".github/workflows",
    ] {
        std::fs::create_dir_all(root.join(folder)).unwrap();
    }
    let approved = Access {
        outside: true,
        ..PROJECT
    };
    for requested in [
        ".git/hooks/pre-commit",
        ".git/config",
        ".git",
        ".GIT/config",
        "vendor/lib/.git/config",
        ".agents/skills/x.md",
        ".claude/settings.json",
        ".codex/config.toml",
    ] {
        let error = write_target(&broker, requested, approved).unwrap_err();
        assert!(
            error.to_string().contains("repository metadata"),
            "{requested}: {error}"
        );
    }
    // A link cannot hide the repository metadata.
    std::os::unix::fs::symlink(root.join(".git"), root.join("meta")).unwrap();
    assert!(write_target(&broker, "meta/config", approved).is_err());
    // Another repository's metadata stays closed even with outside access.
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(outside.path().join(".git/hooks")).unwrap();
    let hook = outside.path().join(".git/hooks/post-checkout");
    assert!(write_target(&broker, &hook.to_string_lossy(), approved).is_err());
    for requested in [
        ".gitignore",
        "docs/.claude/notes.md",
        ".github/workflows/ci.yml",
    ] {
        assert!(
            write_target(&broker, requested, PROJECT).is_ok(),
            "{requested}"
        );
    }
}
