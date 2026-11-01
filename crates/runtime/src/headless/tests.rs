use super::*;
#[test]
fn profiles_are_exclusive_and_resume_must_exist() {
    let root = tempfile::tempdir().unwrap();
    let paths = AppPaths::at(root.path().to_owned()).unwrap();
    let profile = Profile::open(paths.clone()).unwrap();
    assert!(Profile::open(paths.clone()).is_err());
    assert!(
        profile
            .conversation(Some(uuid::Uuid::new_v4()), false, root.path())
            .is_err()
    );
    assert!(profile.conversation(None, true, root.path()).is_err());
    let chat = profile.conversation(None, false, root.path()).unwrap();
    assert_eq!(
        profile
            .conversation(Some(chat.id), false, root.path())
            .unwrap()
            .id,
        chat.id
    );
    drop(profile);
    assert!(Profile::open(paths).is_ok());
}
