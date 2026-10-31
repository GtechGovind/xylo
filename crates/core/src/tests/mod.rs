use super::*;
#[test]
fn worker_credential_debug_is_redacted() {
    let credential = WorkerCredential(Some("fixture-secret".into()));
    assert!(!format!("{credential:?}").contains("fixture-secret"));
    let decoded: WorkerCredential =
        serde_json::from_str(&serde_json::to_string(&credential).unwrap()).unwrap();
    assert_eq!(decoded.0, credential.0);
    let command = WorkerCommand::ProviderCredentialUpdated {
        request_id: Uuid::new_v4(),
        result: Ok(ProviderCredentialUpdate {
            generation: 1,
            credential,
        }),
    };
    assert!(!format!("{command:?}").contains("fixture-secret"));
    let decoded: WorkerCommand =
        serde_json::from_str(&serde_json::to_string(&command).unwrap()).unwrap();
    assert!(matches!(
        decoded,
        WorkerCommand::ProviderCredentialUpdated {
            result: Ok(ProviderCredentialUpdate { generation: 1, .. }),
            ..
        }
    ));
}
#[test]
fn incompatible_protocol_is_rejected() {
    let mut message = Envelope::new(Uuid::new_v4(), None, 0, WorkerCommand::Hello);
    message.version = PROTOCOL_VERSION + 1;
    assert!(message.validate().is_err());
}
#[test]
fn context_and_endpoint_are_validated() {
    let mut config = ProviderConfig::default();
    assert!(config.validate().is_err());
    config.model = "user-selected-model".into();
    assert!(config.validate().is_ok());
    config.output_tokens = 9000;
    assert!(config.validate().is_err());
}

mod permission;

mod reasoning;

mod native_workspace_preferences;
