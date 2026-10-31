use super::*;
use std::io::{Read, Write};

#[test]
fn nested_directory_authority_keeps_ancestor_rename_fences() {
    let temporary = tempfile::tempdir().unwrap();
    let nested = temporary.path().join("ancestor/child");
    std::fs::create_dir_all(&nested).unwrap();
    let root = Authority::open(temporary.path()).unwrap();
    let relative = root
        .child("ancestor", false)
        .unwrap()
        .child("child", false)
        .unwrap();
    let absolute = Authority::open(&nested).unwrap();
    assert!(relative.same(&absolute).unwrap());
    assert!(absolute.child("..", false).is_err());
    absolute
        .file("captured.bin", true)
        .unwrap()
        .write_all(b"bounded output")
        .unwrap();
    let moved = temporary.path().join("moved");
    assert!(std::fs::rename(temporary.path().join("ancestor"), &moved).is_err());
    assert_eq!(
        std::fs::read(nested.join("captured.bin")).unwrap(),
        b"bounded output"
    );
    drop(absolute);
    drop(relative);
    std::fs::rename(temporary.path().join("ancestor"), &moved).unwrap();
    assert_eq!(
        std::fs::read(moved.join("child/captured.bin")).unwrap(),
        b"bounded output"
    );
}

#[test]
fn published_stream_can_be_verified_while_original_handle_is_live() {
    let temporary = tempfile::tempdir().unwrap();
    let authority = Authority::open(temporary.path()).unwrap();
    let source = authority.child("private", true).unwrap();
    let target = authority.child("public", true).unwrap();
    let mut file = source.file("stream.part", true).unwrap();
    file.write_all(b"native captured bytes").unwrap();
    file.sync_all().unwrap();
    source
        .rename(&file, "stream.part", &target, "stream.bin", false)
        .unwrap();
    let mut published = target.file("stream.bin", false).unwrap();
    assert!(same_file(&file, &published).unwrap());
    let mut bytes = Vec::new();
    published.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"native captured bytes");
}

#[test]
fn atomic_publication_never_replaces_unrelated_existing_output() {
    let temporary = tempfile::tempdir().unwrap();
    let authority = Authority::open(temporary.path()).unwrap();
    let source = authority.child("private", true).unwrap();
    let target = authority.child("public", true).unwrap();
    let mut original = source.file("stream.part", true).unwrap();
    original.write_all(b"new bytes").unwrap();
    target
        .file("stream.bin", true)
        .unwrap()
        .write_all(b"existing bytes")
        .unwrap();
    assert!(
        source
            .rename(&original, "stream.part", &target, "stream.bin", false)
            .is_err()
    );
    assert_eq!(
        std::fs::read(temporary.path().join("public/stream.bin")).unwrap(),
        b"existing bytes"
    );
}

#[test]
fn existing_private_staging_reopens_with_flush_authority() {
    let temporary = tempfile::tempdir().unwrap();
    let authority = Authority::open(temporary.path()).unwrap();
    let data = authority.child("data", true).unwrap();
    data.file("stream.part", true)
        .unwrap()
        .write_all(b"retained prefix")
        .unwrap();
    data.file_for_sync("stream.part")
        .unwrap()
        .sync_all()
        .unwrap();
}

#[test]
fn hardlinked_files_are_never_output_authorities() {
    let temporary = tempfile::tempdir().unwrap();
    let authority = Authority::open(temporary.path()).unwrap();
    let data = authority.child("data", true).unwrap();
    data.file("one.bin", true)
        .unwrap()
        .write_all(b"original")
        .unwrap();
    std::fs::hard_link(
        temporary.path().join("data/one.bin"),
        temporary.path().join("data/two.bin"),
    )
    .unwrap();
    assert!(data.file("one.bin", false).is_err());
    assert!(data.recovery_file("two.bin").is_err());
}

#[test]
fn lease_handles_allow_a_second_open_instead_of_a_sharing_error() {
    let temporary = tempfile::tempdir().unwrap();
    let authority = Authority::open(temporary.path()).unwrap();
    let first = authority.lock_file("storage.lock").unwrap();
    let second = authority.lock_file("storage.lock").unwrap();
    assert!(same_file(&first, &second).unwrap());
}
