use super::*;
use std::io::Read;

fn config(root: &Path) -> CapabilityConfig {
    let root = root.canonicalize().unwrap();
    std::fs::create_dir_all(root.join("artifacts")).unwrap();
    std::fs::create_dir_all(root.join("private")).unwrap();
    CapabilityConfig {
        workspace: root.clone(),
        artifacts: root.join("artifacts"),
        management_root: Some(root.join("private")),
        ..Default::default()
    }
}

fn read(input: InputFile) -> String {
    let mut text = String::new();
    { input.file }.read_to_string(&mut text).unwrap();
    text
}

#[test]
fn opens_workspace_and_artifact_files_once() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(directory.path());
    std::fs::create_dir(config.workspace.join("docs")).unwrap();
    std::fs::write(config.workspace.join("docs/notes.txt"), "notes").unwrap();
    std::fs::write(config.artifacts.join("take.wav"), "audio").unwrap();
    let input = open_input(&config, "docs/./notes.txt").unwrap();
    assert_eq!(input.root, InputRoot::Workspace);
    assert_eq!(input.path, config.workspace.join("docs/notes.txt"));
    assert_eq!(read(input), "notes");
    let absolute = config.artifacts.join("take.wav");
    let input = open_input(&config, &absolute.to_string_lossy()).unwrap();
    assert_eq!(input.root, InputRoot::Artifacts);
    assert_eq!(read(input), "audio");
}

#[cfg(unix)]
#[test]
fn links_traversal_and_private_files_are_refused() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(directory.path());
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "secret").unwrap();
    std::fs::write(config.workspace.join("inside.txt"), "inside").unwrap();
    std::fs::write(config.workspace.join("private/catalog.json"), "{}").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret"),
        config.workspace.join("file-link"),
    )
    .unwrap();
    // A link that stays inside is read and walked; see the test below.
    std::os::unix::fs::symlink(
        config.workspace.join("inside.txt"),
        config.workspace.join("inner"),
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.path(), config.workspace.join("dir-link")).unwrap();
    std::fs::hard_link(outside.path().join("secret"), config.workspace.join("hard")).unwrap();
    std::fs::create_dir(config.workspace.join("fifo-dir")).unwrap();
    for requested in [
        "file-link",
        "dir-link/secret",
        "hard",
        "../secret",
        "private/catalog.json",
        "fifo-dir",
        "missing.txt",
    ] {
        assert!(open_input(&config, requested).is_err(), "{requested}");
    }
    let elsewhere = outside.path().join("secret");
    assert!(open_input(&config, &elsewhere.to_string_lossy()).is_err());
    assert!(open_input(&config, "inside.txt").is_ok());
    assert_eq!(read(open_input(&config, "inner").unwrap()), "inside");
}

#[cfg(unix)]
#[test]
fn links_are_walked_only_while_they_stay_beneath_the_root() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let config = config(directory.path());
    let workspace = &config.workspace;
    std::fs::create_dir_all(workspace.join("docs/deep")).unwrap();
    std::fs::write(workspace.join("docs/notes.txt"), "notes").unwrap();
    std::fs::write(workspace.join("private/catalog.json"), "{}").unwrap();
    // Relative targets, including `..` that stays inside.
    symlink("../notes.txt", workspace.join("docs/deep/up")).unwrap();
    symlink("docs", workspace.join("docs-link")).unwrap();
    // A chain through a directory link and a file link.
    symlink("docs-link/deep/up", workspace.join("chain")).unwrap();
    // Escapes and loops.
    symlink("../../outside", workspace.join("docs/escape")).unwrap();
    symlink("loop-b", workspace.join("loop-a")).unwrap();
    symlink("loop-a", workspace.join("loop-b")).unwrap();
    symlink("private/catalog.json", workspace.join("to-private")).unwrap();
    assert_eq!(read(open_input(&config, "docs/deep/up").unwrap()), "notes");
    assert_eq!(
        read(open_input(&config, "docs-link/notes.txt").unwrap()),
        "notes"
    );
    assert_eq!(read(open_input(&config, "chain").unwrap()), "notes");
    // The reported path is where the file really is, so drafts and
    // revisions of one file share a key however it was reached.
    assert_eq!(
        open_input(&config, "chain").unwrap().path,
        workspace.join("docs/notes.txt")
    );
    for refused in ["docs/escape", "loop-a", "to-private"] {
        assert!(open_input(&config, refused).is_err(), "{refused}");
    }
}

#[cfg(unix)]
#[test]
fn a_component_swapped_for_a_link_after_validation_cannot_redirect_the_open() {
    // The old check canonicalized, then reopened by path. A directory that
    // becomes a link is now refused at the component itself.
    let directory = tempfile::tempdir().unwrap();
    let config = config(directory.path());
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("notes.txt"), "secret").unwrap();
    std::fs::create_dir(config.workspace.join("docs")).unwrap();
    std::fs::write(config.workspace.join("docs/notes.txt"), "notes").unwrap();
    assert_eq!(
        read(open_input(&config, "docs/notes.txt").unwrap()),
        "notes"
    );
    std::fs::rename(
        config.workspace.join("docs"),
        config.workspace.join("moved"),
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.path(), config.workspace.join("docs")).unwrap();
    assert!(open_input(&config, "docs/notes.txt").is_err());
}
