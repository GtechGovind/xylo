use super::*;

#[test]
fn live_runs_are_refused_without_the_opt_in() {
    if std::env::var(LIVE_ENV).as_deref() == Ok("1") {
        return; // The developer opted in for this shell; nothing to check.
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("models.json");
    std::fs::write(&path, "[]").unwrap();
    let error = load(&path).unwrap_err().to_string();
    assert!(error.contains(LIVE_ENV), "{error}");
}

#[test]
fn credentials_never_appear_in_debug_output() {
    let model = LiveModel {
        label: "m".into(),
        provider: ProviderConfig::default(),
        credential: Some("sk-secret".into()),
    };
    let shown = format!("{model:?}");
    assert!(!shown.contains("sk-secret"));
    assert!(shown.contains("REDACTED"));
}
