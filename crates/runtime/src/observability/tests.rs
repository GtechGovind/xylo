use super::*;

fn clean_logs(directory: &Path, today: u64) -> Result<()> {
    clean_logs_except(directory, today, TOTAL_LOG_BYTES, None)
}

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

#[test]
fn oversized_writes_roll_at_ten_mib_and_restart_appends_without_truncating() {
    let root = tempfile::tempdir().unwrap();
    let bytes = vec![b'x'; 21 * 1024 * 1024];
    let mut writer = LogWriter(Arc::new(Mutex::new(
        LogFile::open(root.path().into()).unwrap(),
    )));
    writer.write_all(&bytes).unwrap();
    drop(writer);
    let mut writer = LogWriter(Arc::new(Mutex::new(
        LogFile::open(root.path().into()).unwrap(),
    )));
    writer.write_all(b"tail").unwrap();
    drop(writer);
    let mut paths: Vec<_> = std::fs::read_dir(root.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    paths.sort_by_key(|p| std::fs::metadata(p).unwrap().len());
    assert_eq!(paths.len(), 3, "oversized writes must span bounded files");
    let sizes: Vec<_> = paths
        .iter()
        .map(|p| std::fs::metadata(p).unwrap().len())
        .collect();
    assert_eq!(sizes, [1024 * 1024 + 4, 10 * 1024 * 1024, 10 * 1024 * 1024]);
    assert!(std::fs::read(&paths[0]).unwrap().ends_with(b"tail"));
}

#[test]
fn retention_limits_owned_logs_to_one_hundred_mib() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..12 {
        File::create(root.path().join(format!("xylo-100-{index}.log")))
            .unwrap()
            .set_len(10 * 1024 * 1024)
            .unwrap();
    }
    std::fs::write(root.path().join("notes.log"), "keep").unwrap();
    clean_logs(root.path(), 100).unwrap();
    assert!(!root.path().join("xylo-100-0.log").exists());
    assert!(!root.path().join("xylo-100-1.log").exists());
    assert!(root.path().join("xylo-100-2.log").exists());
    assert!(root.path().join("notes.log").exists());
}

#[test]
fn lifecycle_failure_is_logged_without_telemetry_or_error_content() {
    let root = tempfile::tempdir().unwrap();
    let file = File::create(root.path().join("capture")).unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(file)
        .finish();
    let state = MetricState {
        exporter: None,
        input_rate: None,
        output_rate: None,
        tools: Mutex::default(),
        turns: Mutex::default(),
    };
    let session = Uuid::new_v4();
    let turn = Uuid::new_v4();
    tracing::subscriber::with_default(subscriber, || {
        state.event(
            session,
            turn,
            &EngineEvent::Error {
                message: "private prompt Bearer secret-token".into(),
                retryable: true,
                kind: xylo_core::ErrorKind::Provider,
            },
        )
    });
    let output = std::fs::read_to_string(root.path().join("capture")).unwrap();
    assert!(output.contains(&session.to_string()), "{output}");
    assert!(output.contains(&turn.to_string()), "{output}");
    assert!(
        output.contains("Provider") && output.contains("retryable=true"),
        "{output}"
    );
    assert!(!output.contains("private prompt") && !output.contains("secret-token"));
}

#[cfg(unix)]
#[test]
fn existing_logs_are_made_private_and_symlinks_are_never_opened() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = tempfile::tempdir().unwrap();
    let path = root
        .path()
        .join(format!("xylo-{}.log", unix_seconds() / DAY));
    std::fs::write(&path, "existing").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    drop(LogFile::open(root.path().into()).unwrap());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::remove_file(&path).unwrap();
    let target = root.path().join("private.txt");
    std::fs::write(&target, "untouched").unwrap();
    symlink(&target, &path).unwrap();
    assert!(LogFile::open(root.path().into()).is_err());
    assert_eq!(std::fs::read_to_string(target).unwrap(), "untouched");
}

#[test]
fn retention_removes_expired_segments_but_keeps_unowned_names_and_directories() {
    let root = tempfile::tempdir().unwrap();
    for name in [
        "xylo-1-4.log",
        "xylo-2-4.log",
        "xylo-1-extra.log",
        "xylo-01.log",
        "xylo-1-01.log",
    ] {
        std::fs::write(root.path().join(name), "keep unless expired").unwrap();
    }
    std::fs::create_dir(root.path().join("xylo-1-5.log")).unwrap();
    clean_logs(root.path(), 8).unwrap();
    assert!(!root.path().join("xylo-1-4.log").exists());
    for name in [
        "xylo-2-4.log",
        "xylo-1-extra.log",
        "xylo-01.log",
        "xylo-1-01.log",
        "xylo-1-5.log",
    ] {
        assert!(root.path().join(name).exists(), "{name}");
    }
}

#[cfg(unix)]
#[test]
fn retention_never_follows_or_removes_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("private.txt");
    std::fs::write(&target, "untouched").unwrap();
    let link = root.path().join("xylo-1.log");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    clean_logs(root.path(), 8).unwrap();
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_to_string(target).unwrap(), "untouched");
}

#[test]
fn writes_roll_at_midnight_and_expire_older_days() {
    let root = tempfile::tempdir().unwrap();
    let mut log = LogFile::open_at(root.path().into(), 100).unwrap();
    assert_eq!(log.write_at(b"yesterday\n", 100).unwrap(), 10);
    assert_eq!(log.write_at(b"today\n", 101).unwrap(), 6);
    assert_eq!(
        std::fs::read(root.path().join("xylo-100.log")).unwrap(),
        b"yesterday\n"
    );
    assert_eq!(
        std::fs::read(root.path().join("xylo-101.log")).unwrap(),
        b"today\n"
    );
    log.write_at(b"next week\n", 108).unwrap();
    assert!(!root.path().join("xylo-100.log").exists());
    assert!(!root.path().join("xylo-101.log").exists());
    assert_eq!(
        std::fs::read(root.path().join("xylo-108.log")).unwrap(),
        b"next week\n"
    );
}

#[test]
fn restart_of_a_full_segment_rolls_and_enforces_total_cap_before_writing() {
    let root = tempfile::tempdir().unwrap();
    File::create(root.path().join("xylo-100.log"))
        .unwrap()
        .set_len(10 * 1024 * 1024)
        .unwrap();
    for index in 1..10 {
        File::create(root.path().join(format!("xylo-100-{index}.log")))
            .unwrap()
            .set_len(10 * 1024 * 1024)
            .unwrap();
    }
    let mut log = LogFile::open_at(root.path().into(), 100).unwrap();
    log.write_at(b"new", 100).unwrap();
    assert_eq!(
        std::fs::read(root.path().join("xylo-100-10.log")).unwrap(),
        b"new"
    );
    assert!(!root.path().join("xylo-100.log").exists());
    let total: u64 = std::fs::read_dir(root.path())
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();
    assert!(total <= 100 * 1024 * 1024);
}

#[test]
fn a_normal_diagnostic_rolls_as_one_chunk_instead_of_splitting_its_line() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("xylo-100.log");
    File::create(&first)
        .unwrap()
        .set_len(10 * 1024 * 1024 - 4)
        .unwrap();
    let mut log = LogFile::open_at(root.path().into(), 100).unwrap();
    let message = b"WARN provider failed\n";
    assert_eq!(log.write_at(message, 100).unwrap(), message.len());
    assert_eq!(
        std::fs::metadata(first).unwrap().len(),
        10 * 1024 * 1024 - 4
    );
    assert_eq!(
        std::fs::read(root.path().join("xylo-100-1.log")).unwrap(),
        message
    );
}
