//! Qualifies the local request boundary without using a real credential store.
use super::*;
use tokio::net::TcpListener;

fn fixture(endpoint: String) -> (NativeConnectors, ConnectorConfig, CredentialVault) {
    let vault = CredentialVault::from_test_store(
        "native-dispatch-fixture",
        keyring_core::mock::Store::new().unwrap(),
    );
    let client = NativeConnectors::new(vault.clone()).unwrap();
    let config = ConnectorConfig {
        id: uuid::Uuid::new_v4(),
        name: "Offline fixture".into(),
        transport: "gmail".into(),
        endpoint,
        command: None,
        args: vec![],
        credential_ref: Some("fixture".into()),
    };
    (client, config, vault)
}

#[tokio::test]
async fn preparation_failures_are_typed_and_never_open_the_loopback_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    const SECRET: &str = "never-print-dispatch-fixture-token";
    let cases = [
        (None, false, false, "credential is missing"),
        (None, true, false, "Choose an OS credential"),
        (
            Some("bad\ncredential".to_owned()),
            false,
            false,
            "credential is invalid",
        ),
        (
            Some("{not-valid-json".to_owned()),
            false,
            false,
            "Invalid stored OAuth token",
        ),
        (
            Some(json!({"access_token":SECRET,"expires_at_ms":1}).to_string()),
            false,
            false,
            "OAuth token expired",
        ),
        (
            Some(format!("{SECRET}\u{100}")),
            false,
            false,
            "credential is invalid",
        ),
        (
            Some(format!("{SECRET} invalid-space")),
            false,
            false,
            "credential is invalid",
        ),
        (Some(SECRET.into()), false, true, "queries or fragments"),
    ];
    for (secret, missing_reference, invalid_endpoint, expected) in cases {
        let (client, mut config, vault) = fixture(endpoint.clone());
        if let Some(secret) = secret {
            vault.set("fixture", &secret).unwrap();
        }
        if missing_reference {
            config.credential_ref = None;
        }
        if invalid_endpoint {
            config.endpoint.push_str("?invalid=true");
        }
        let error = client
            .call(
                &config,
                "gmail_send_draft",
                json!({"draft_id":"draft"}),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            error.downcast_ref::<ConnectorNotDispatched>().is_some(),
            "{error:#}"
        );
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains(expected), "{diagnostic}");
        assert!(!diagnostic.contains(SECRET));
        assert!(
            error.source().is_none(),
            "Preparation must not retain a credential-bearing source"
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn cancellation_before_admission_and_after_full_build_are_definitive() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (client, config, vault) = fixture(endpoint);
    vault.set("fixture", "offline-fixture-token").unwrap();
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = client
        .call(
            &config,
            "gmail_send_draft",
            json!({"draft_id":"draft"}),
            &cancelled,
        )
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<ConnectorNotDispatched>().is_some());
    assert_eq!(client.capacity.available_permits(), 8);

    let cancel = CancellationToken::new();
    let prepared = client
        .prepare_request(
            &config,
            "gmail_send_draft",
            json!({"draft_id":"draft"}),
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(client.capacity.available_permits(), 7);
    cancel.cancel();
    let error = client
        .execute_prepared(&config, "gmail_send_draft", prepared, &cancel)
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<ConnectorNotDispatched>().is_some());
    assert_eq!(client.capacity.available_permits(), 8);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn an_attempted_connection_failure_is_not_a_definitive_preflight_rejection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let (client, config, vault) = fixture(endpoint);
    vault.set("fixture", "offline-fixture-token").unwrap();
    let error = client
        .call(
            &config,
            "gmail_send_draft",
            json!({"draft_id":"draft"}),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<ConnectorNotDispatched>().is_none());
    assert!(error.to_string().contains("unknown outcome"));
    assert!(!format!("{error:#}").contains("offline-fixture-token"));
}

struct SensitiveStoreFailure;
impl keyring_core::api::CredentialStoreApi for SensitiveStoreFailure {
    fn vendor(&self) -> String {
        "offline-fixture".into()
    }
    fn id(&self) -> String {
        "sensitive-error-fixture".into()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn build(
        &self,
        _: &str,
        _: &str,
        _: Option<&std::collections::HashMap<&str, &str>>,
    ) -> keyring_core::Result<keyring_core::Entry> {
        Err(keyring_core::Error::BadDataFormat(
            b"private-fixture-credential-bytes".to_vec(),
            Box::new(std::io::Error::other("private-fixture-credential-bytes")),
        ))
    }
}

#[tokio::test]
async fn credential_store_error_chains_never_escape_the_local_rejection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (_, config, _) = fixture(endpoint);
    let store: std::sync::Arc<keyring_core::CredentialStore> =
        std::sync::Arc::new(SensitiveStoreFailure);
    let client = NativeConnectors::new(CredentialVault::from_test_store(
        "sensitive-error-fixture",
        store,
    ))
    .unwrap();
    let error = client
        .call(
            &config,
            "gmail_send_draft",
            json!({"draft_id":"draft"}),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<ConnectorNotDispatched>().is_some());
    assert_eq!(error.to_string(), "Connector credential could not be read");
    assert!(!format!("{error:#}").contains("private-fixture-credential-bytes"));
    assert!(!format!("{error:?}").contains("private-fixture-credential-bytes"));
    assert!(error.source().is_none());
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}
