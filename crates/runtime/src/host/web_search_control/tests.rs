//! Search credentials are private and replaced only by an explicit backend save.
use super::*;

#[tokio::test]
async fn web_search_save_keeps_keys_out_of_settings_and_deletes_replaced_key() {
    let (_directory, host, _events) = crate::host::tests::state();
    let config = WebSearchSettings {
        backend: WebSearchBackend::Brave,
        ..Default::default()
    };
    host.save_web_search(config.clone(), Some("search-secret".into()))
        .await
        .unwrap();
    let saved = host.settings.lock().await.web_search.clone();
    let reference = saved.credential_ref.unwrap();
    assert_eq!(
        host.vault().unwrap().get(&reference).unwrap().as_deref(),
        Some("search-secret")
    );
    assert!(
        !serde_json::to_string(&host.store.settings().unwrap())
            .unwrap()
            .contains("search-secret")
    );
    host.save_web_search(WebSearchSettings::default(), None)
        .await
        .unwrap();
    assert!(host.vault().unwrap().get(&reference).unwrap().is_none());
}

#[tokio::test]
async fn web_search_rejects_credential_reference_substitution() {
    let (_directory, host, _events) = crate::host::tests::state();
    host.vault()
        .unwrap()
        .set("provider", "provider-secret")
        .unwrap();
    assert!(
        host.save_web_search(
            WebSearchSettings {
                backend: WebSearchBackend::Brave,
                credential_ref: Some("provider".into()),
                ..Default::default()
            },
            None
        )
        .await
        .is_err()
    );
    assert_eq!(
        host.settings.lock().await.web_search.backend,
        WebSearchBackend::Disabled
    );
}

#[test]
fn web_search_endpoint_never_embeds_secrets_or_uses_plain_http() {
    for endpoint in [
        "http://localhost:8080",
        "https://user:pass@example.com",
        "https://example.com?key=secret",
    ] {
        assert!(
            validate(&WebSearchSettings {
                backend: WebSearchBackend::Searxng,
                endpoint: endpoint.into(),
                ..Default::default()
            })
            .is_err()
        );
    }
}
