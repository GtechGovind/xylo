use super::super::*;

#[test]
fn profile_uses_xylo_identity_and_database() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("xylo");
    let paths = AppPaths::at(data.clone()).unwrap();
    assert_eq!(paths.data, data);
    assert_eq!(paths.database, data.join("xylo.db"));
    assert!(paths.artifacts.is_dir());
    assert!(paths.models.is_dir());
    assert!(
        !paths.database.exists(),
        "Path setup must not open a database"
    );
    assert_eq!(xylo_core::APPLICATION_NAME, "Xylo");
    assert_eq!(xylo_core::APPLICATION_ID, "com.qurkos.xylo");
    assert_eq!(xylo_core::CREDENTIAL_SERVICE, "com.qurkos.xylo");
}
