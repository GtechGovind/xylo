use super::super::*;

#[test]
fn remembering_a_model_preserves_distinct_custom_connections() {
    let base = ProviderConfig {
        model: "shared-model".into(),
        ..Default::default()
    };
    for different_catalog in [false, true] {
        let mut first = base.clone();
        let mut second = base.clone();
        if different_catalog {
            first.discovery_url = Some("https://example.com/catalog-a".into());
            second.discovery_url = Some("https://example.com/catalog-b".into());
        } else {
            first.headers.insert("X-Tenant".into(), "a".into());
            second.headers.insert("X-Tenant".into(), "b".into());
        }
        let mut settings = AppSettings::default();
        settings.remember_model(first.clone());
        settings.remember_model(second.clone());
        assert_eq!(settings.providers.len(), 2);
        second.reasoning_effort = Some("high".into());
        settings.remember_model(second.clone());
        assert_eq!(settings.providers.len(), 2);
        assert_eq!(settings.providers[0].headers, first.headers);
        assert_eq!(settings.providers[0].discovery_url, first.discovery_url);
        assert_eq!(settings.providers[0].reasoning_effort, None);
        assert_eq!(
            settings.providers[1].reasoning_effort,
            second.reasoning_effort
        );
    }
}

#[test]
fn omitted_optional_provider_fields_default_to_none() {
    let mut value = serde_json::to_value(ProviderConfig::default()).unwrap();
    value.as_object_mut().unwrap().remove("reasoning_effort");
    value.as_object_mut().unwrap().remove("reasoning_options");
    value
        .as_object_mut()
        .unwrap()
        .remove("default_reasoning_effort");
    value.as_object_mut().unwrap().remove("discovery_url");
    let config: ProviderConfig = serde_json::from_value(value).unwrap();
    assert_eq!(config.reasoning_effort, None);
    assert_eq!(config.reasoning_options, None);
    assert_eq!(config.default_reasoning_effort, None);
    assert_eq!(config.discovery_url, None);
}

#[test]
fn advertised_reasoning_survives_persistence_and_explicit_no_support_wins() {
    let mut config = ProviderConfig {
        model: "future-account-model".into(),
        reasoning_options: Some(vec![
            "high".into(),
            "low".into(),
            "high".into(),
            "invalid value".into(),
        ]),
        default_reasoning_effort: Some("low".into()),
        reasoning_effort: Some("high".into()),
        ..Default::default()
    };
    let restored: ProviderConfig =
        serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
    assert_eq!(restored.supported_reasoning_efforts(), vec!["high", "low"]);
    assert_eq!(restored.effective_reasoning_effort(), Some("high"));
    assert_eq!(restored.default_reasoning_effort.as_deref(), Some("low"));
    config.model = "gpt-5.2".into();
    config.reasoning_options = Some(vec![]);
    assert!(config.supported_reasoning_efforts().is_empty());
    assert_eq!(config.effective_reasoning_effort(), None);
    config.reasoning_options = Some(vec!["high".into()]);
    config.family = ApiFamily::Anthropic;
    assert!(config.supported_reasoning_efforts().is_empty());
}

#[test]
fn reasoning_capability_follows_protocol_model_and_saved_effort() {
    let mut config = ProviderConfig {
        model: "gpt-5.2".into(),
        reasoning_effort: Some("xhigh".into()),
        ..Default::default()
    };
    assert_eq!(config.effective_reasoning_effort(), Some("xhigh"));
    config.model = "gpt-5.2-pro".into();
    config.reasoning_effort = Some("low".into());
    assert_eq!(config.effective_reasoning_effort(), None);
    config.model = "gpt-4.1".into();
    assert!(config.supported_reasoning_efforts().is_empty());
    config.model = "gpt-5.2-chat-latest".into();
    assert!(config.supported_reasoning_efforts().is_empty());
    config.model = "gpt-5.2".into();
    config.family = ApiFamily::Anthropic;
    assert!(config.supported_reasoning_efforts().is_empty());
}

#[test]
fn the_served_context_lowers_but_never_raises_the_window() {
    let mut config = ProviderConfig {
        context_tokens: 32_768,
        ..Default::default()
    };
    assert_eq!(config.context_window(), 32_768);
    config.served_context_tokens = Some(4096);
    assert_eq!(config.context_window(), 4096);
    config.served_context_tokens = Some(131_072);
    assert_eq!(config.context_window(), 32_768);
    config.served_context_tokens = Some(0);
    assert_eq!(config.context_window(), 32_768);
}

#[test]
fn custom_catalog_supports_unknown_native_model_reasoning_and_preserves_old_settings() {
    let mut config = ProviderConfig {
        family: ApiFamily::Anthropic,
        model: "future-native".into(),
        custom: Some(Default::default()),
        reasoning_options: Some(vec!["low".into(), "high".into()]),
        ..Default::default()
    };
    assert_eq!(config.supported_reasoning_efforts(), vec!["low", "high"]);
    config.custom = None;
    assert!(config.supported_reasoning_efforts().is_empty());
    let mut value = serde_json::to_value(&config).unwrap();
    value.as_object_mut().unwrap().remove("custom");
    value.as_object_mut().unwrap().remove("model_capabilities");
    let restored: ProviderConfig = serde_json::from_value(value).unwrap();
    assert!(restored.custom.is_none() && restored.model_capabilities.is_none());
    assert_eq!(restored.effective_family(), ApiFamily::Anthropic);
}

#[test]
fn automatic_custom_routing_uses_only_configured_documented_routes() {
    let mut config = ProviderConfig {
        custom: Some(Default::default()),
        model_capabilities: Some(crate::ModelCapabilities {
            supported_apis: Some(vec![
                ApiFamily::OpenAiChat,
                ApiFamily::OpenAiResponses,
                ApiFamily::Anthropic,
            ]),
            native_family: Some(ApiFamily::Anthropic),
            ..Default::default()
        }),
        ..Default::default()
    };
    let custom = config.custom.as_mut().unwrap();
    custom.paths.messages.clear();
    custom.paths.responses.clear();
    assert_eq!(config.effective_family(), ApiFamily::OpenAiChat);
}
