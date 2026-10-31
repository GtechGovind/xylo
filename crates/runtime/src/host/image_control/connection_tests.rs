use super::*;
fn provider() -> ImageGenerationProvider {
    ImageGenerationProvider {
        name: "Fixture image API".into(),
        protocol: ImageGenerationProtocol::OpenAiImages,
        endpoint: "http://127.0.0.1:8181/v1".into(),
        model: "explicit-image-model".into(),
        credential_ref: Some("image:fixture".into()),
        headers: Default::default(),
        timeout_seconds: 180,
    }
}
#[test]
fn image_connection_validates_before_secret_and_publishes_only_after_secret_save() {
    let directory = tempfile::tempdir().unwrap();
    let service = ImageGenerationService::new(
        directory.path().join("data"),
        directory.path().join("artifacts"),
    )
    .unwrap();
    let mut invalid = provider();
    invalid.model.clear();
    assert!(
        save_connection(
            service.clone(),
            invalid,
            Some("fixture-api-key".into()),
            |_, _| panic!("Invalid metadata reached secret store")
        )
        .is_err()
    );
    assert!(
        save_connection(
            service.clone(),
            provider(),
            Some("fixture-api-key".into()),
            |_, _| Err(anyhow::anyhow!("Fixture vault failure"))
        )
        .is_err()
    );
    assert!(service.configured_provider().unwrap().is_none());
    let saved = save_connection(
        service.clone(),
        provider(),
        Some("fixture-api-key".into()),
        |reference, secret| {
            assert_eq!(reference, "image:fixture");
            assert_eq!(secret, "fixture-api-key");
            assert!(
                service.configured_provider().unwrap().is_none(),
                "Connection published before its key was saved"
            );
            Ok(())
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(service.configured_provider().unwrap(), Some(saved.clone()));
    assert!(
        !serde_json::to_string(&saved)
            .unwrap()
            .contains("fixture-api-key")
    );
}
