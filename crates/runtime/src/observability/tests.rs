use super::*;
#[test]
fn rolling_retention_only_removes_owned_logs_older_than_seven_days() {
    let root = tempfile::tempdir().unwrap();
    for name in [
        "xylo-1.log",
        "xylo-2.log",
        "xylo-8.log",
        "private.log",
        "xylo-invalid.log",
    ] {
        std::fs::write(root.path().join(name), "test").unwrap();
    }
    clean_logs(root.path(), 8).unwrap();
    assert!(!root.path().join("xylo-1.log").exists());
    for name in [
        "xylo-2.log",
        "xylo-8.log",
        "private.log",
        "xylo-invalid.log",
    ] {
        assert!(root.path().join(name).exists());
    }
}
#[test]
fn log_writer_creates_private_files() {
    let root = tempfile::tempdir().unwrap();
    let file = LogFile::open(root.path().to_owned()).unwrap();
    let mut writer = LogWriter(Arc::new(Mutex::new(file)));
    writer.write_all(b"message\n").unwrap();
    writer.flush().unwrap();
    let path = root
        .path()
        .join(format!("xylo-{}.log", unix_seconds() / DAY));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "message\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
