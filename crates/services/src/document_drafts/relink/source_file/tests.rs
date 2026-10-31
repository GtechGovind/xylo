use super::*;
#[test]
fn swapped_symlink_ancestor_is_not_followed() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let project = root.join("project");
    let private = root.join("private");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&private).unwrap();
    std::fs::write(private.join("original.txt"), "private").unwrap();
    let path = project.join("original.txt");
    std::fs::remove_dir(&project).unwrap();
    std::os::unix::fs::symlink(&private, &project).unwrap();
    assert!(open(&path).is_err());
}
#[test]
fn opened_descriptor_keeps_its_bytes_when_the_path_is_replaced() {
    use std::io::Read;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let path = root.join("original.txt");
    std::fs::write(&path, "original").unwrap();
    let mut file = open(&path).unwrap();
    std::fs::rename(&path, root.join("moved.txt")).unwrap();
    std::fs::write(&path, "replacement").unwrap();
    let mut text = String::new();
    file.read_to_string(&mut text).unwrap();
    assert_eq!(text, "original");
}
