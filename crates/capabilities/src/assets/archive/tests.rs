use super::*;
#[test]
fn archive_traversal_and_special_entries_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("unsafe.zip");
    let mut zip = zip::ZipWriter::new(File::create(&file).unwrap());
    zip.start_file("../escape", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"escape").unwrap();
    zip.finish().unwrap();
    let spec = Download {
        url: "https://example.com/fixture".into(),
        sha256: "a".repeat(64),
        size_bytes: 1,
        format: Format::Zip,
        destination: "content".into(),
    };
    assert!(extract(&file, &spec, root.path(), &CancellationToken::new()).is_err());
    assert!(!root.path().parent().unwrap().join("escape").exists());
}
