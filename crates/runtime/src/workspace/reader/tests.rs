use super::*;
use std::io::Read;
#[test]
fn final_symlink_substitution_is_refused_before_any_bytes_are_read() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(root.join("file"), "inside").unwrap();
    std::fs::write(outside.path().join("secret"), "outside").unwrap();
    let stop = CancellationToken::new();
    let reader = RootReader::open(&root, &stop, &stop).unwrap();
    assert!(
        reader
            .file_before(&root.join("file"), &stop, &stop, || {
                std::fs::remove_file(root.join("file"))?;
                std::os::unix::fs::symlink(outside.path().join("secret"), root.join("file"))?;
                Ok(())
            })
            .is_err()
    );
}
#[test]
fn swapped_ancestor_reads_the_original_authorized_handle_and_internal_links_work() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    std::fs::write(root.join("nested/file"), "inside").unwrap();
    std::fs::write(outside.path().join("file"), "outside").unwrap();
    std::os::unix::fs::symlink(root.join("nested/file"), root.join("internal")).unwrap();
    let stop = CancellationToken::new();
    let reader = RootReader::open(&root, &stop, &stop).unwrap();
    let mut original = reader.file(&root.join("internal"), &stop, &stop).unwrap();
    let mut text = String::new();
    original.read_to_string(&mut text).unwrap();
    assert_eq!(text, "inside");
    let mut file = reader
        .file_before(&root.join("nested/file"), &stop, &stop, || {
            std::fs::rename(root.join("nested"), root.join("moved"))?;
            std::os::unix::fs::symlink(outside.path(), root.join("nested"))?;
            Ok(())
        })
        .unwrap();
    let mut text = String::new();
    file.read_to_string(&mut text).unwrap();
    assert_eq!(text, "inside");
}
