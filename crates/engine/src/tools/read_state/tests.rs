use super::*;
#[test]
fn observations_require_a_read_and_reject_external_changes_and_other_conversations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a");
    std::fs::write(&path, "one").unwrap();
    let state = ReadState::default();
    let session = Uuid::new_v4();
    assert!(state.require(session, dir.path(), &path).is_err());
    state
        .record(
            session,
            dir.path(),
            &path,
            Fingerprint::read(&path).unwrap(),
        )
        .unwrap();
    state.require(session, dir.path(), &path).unwrap();
    assert!(state.require(Uuid::new_v4(), dir.path(), &path).is_err());
    std::fs::write(&path, "two").unwrap();
    assert!(
        state
            .require(session, dir.path(), &path)
            .unwrap_err()
            .to_string()
            .contains("read it again")
    );
}

#[test]
fn prepared_write_rejects_changes_even_when_a_later_read_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a");
    let creation = WriteRevision::capture(&path).unwrap();
    std::fs::write(&path, "first").unwrap();
    assert!(creation.check().is_err());
    let prepared = WriteRevision::capture(&path).unwrap();
    std::fs::write(&path, "second").unwrap();
    assert!(prepared.check().is_err());
}

#[test]
fn either_metadata_or_content_change_invalidates_a_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a");
    std::fs::write(&path, "one").unwrap();
    let original = Fingerprint::read(&path).unwrap();
    let modified = original.modified.unwrap();
    let state = ReadState::default();
    let session = Uuid::new_v4();
    state
        .record(session, dir.path(), &path, original.clone())
        .unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new().set_modified(modified + std::time::Duration::from_secs(1)),
        )
        .unwrap();
    assert!(state.require(session, dir.path(), &path).is_err());
    std::fs::write(&path, "two").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    assert!(state.require(session, dir.path(), &path).is_err());
}
