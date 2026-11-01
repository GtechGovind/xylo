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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_search_ordered_snapshots_cannot_restore_rotated_or_disabled_credentials() {
    let (_directory, host, _events) = crate::host::tests::state();
    host.save_web_search(
        WebSearchSettings {
            backend: WebSearchBackend::Brave,
            ..Default::default()
        },
        Some("old-key".into()),
    )
    .await
    .unwrap();
    let stale = host.settings.lock().await.clone();
    let old_reference = stale.web_search.credential_ref.clone().unwrap();
    for (backend, key) in [
        (WebSearchBackend::Brave, Some("new-key".to_owned())),
        (WebSearchBackend::Disabled, None),
    ] {
        let order = settings_control::CommitOrder::default();
        let first = order.reserve();
        let second = order.reserve();
        let third = order.reserve();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, resume) = tokio::sync::oneshot::channel();
        let writer = host.clone();
        let dedicated = tokio::spawn(async move {
            let _ticket = first.enter().await;
            entered.send(()).unwrap();
            resume.await.unwrap();
            writer
                .handle(HostCommand::SaveWebSearch(
                    WebSearchSettings {
                        backend,
                        ..Default::default()
                    },
                    key,
                ))
                .await
                .unwrap();
            writer.settings.lock().await.web_search.clone()
        });
        ready.await.unwrap();
        let mut snapshot = stale.clone();
        snapshot.theme = "unrelated theme".into();
        let writer = host.clone();
        let generic = tokio::spawn(async move {
            let _ticket = second.enter().await;
            writer
                .handle(HostCommand::SaveSettings(snapshot))
                .await
                .unwrap();
        });
        let writer = host.clone();
        let mut snapshot = stale.clone();
        snapshot.web_search.credential_ref = Some("provider-credential".into());
        snapshot.provider=ProviderConfig{family:ApiFamily::Azure,endpoint:"https://resource.openai.azure.com/openai/deployments/explicit?api-version=2024-10-21".into(),model:"explicit".into(),..Default::default()};
        let provider = tokio::spawn(async move {
            let _ticket = third.enter().await;
            let generation = writer.discovery.begin();
            let config = snapshot.provider.clone();
            writer
                .handle(HostCommand::SaveProvider(
                    generation,
                    Box::new(snapshot),
                    config,
                    None,
                ))
                .await
                .unwrap();
        });
        release.send(()).unwrap();
        let authoritative = dedicated.await.unwrap();
        generic.await.unwrap();
        provider.await.unwrap();
        assert_eq!(host.settings.lock().await.web_search, authoritative);
        assert_eq!(host.store.settings().unwrap().web_search, authoritative);
        assert_eq!(authoritative.backend, backend);
        assert!(host.vault().unwrap().get(&old_reference).unwrap().is_none());
    }
}

#[tokio::test]
async fn web_search_current_project_merge_is_not_search_write_authority() {
    let (_directory, host, _events) = crate::host::tests::state();
    host.vault()
        .unwrap()
        .set("provider-credential", "provider-secret")
        .unwrap();
    let mut snapshot = host.settings.lock().await.clone();
    snapshot.web_search = WebSearchSettings {
        backend: WebSearchBackend::Brave,
        credential_ref: Some("provider-credential".into()),
        ..Default::default()
    };
    host.commit_settings(snapshot, settings_control::ProjectFields::Current)
        .await
        .unwrap();
    assert_eq!(
        host.settings.lock().await.web_search,
        WebSearchSettings::default()
    );
    assert_eq!(
        host.store.settings().unwrap().web_search,
        WebSearchSettings::default()
    );
}
