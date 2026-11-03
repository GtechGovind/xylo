//! Headless provider resolution, shared by CLI runs and SDK stream sessions.
use super::*;

#[test]
fn headless_inherits_global_fallback_and_preserves_model_override() {
    let settings = xylo_core::AppSettings {
        fallback_model: Some("global-fallback".into()),
        ..Default::default()
    };
    for override_model in [None, Some("explicit-primary")] {
        let provider = turn_provider(&settings, override_model);
        assert_eq!(provider.fallback_model.as_deref(), Some("global-fallback"));
        assert_eq!(
            provider.model,
            override_model.unwrap_or(&settings.provider.model)
        );
        assert_eq!(provider.endpoint, settings.provider.endpoint);
        assert_eq!(provider.family, settings.provider.family);
    }
    assert!(settings.provider.fallback_model.is_none());
}

#[test]
fn headless_explicit_provider_fallback_wins_even_with_primary_override() {
    let mut settings = xylo_core::AppSettings {
        fallback_model: Some("global-fallback".into()),
        ..Default::default()
    };
    settings.provider.fallback_model = Some("provider-fallback".into());
    let provider = turn_provider(&settings, Some("explicit-primary"));
    assert_eq!(provider.model, "explicit-primary");
    assert_eq!(
        provider.fallback_model.as_deref(),
        Some("provider-fallback")
    );
}

#[test]
fn model_override_resolves_catalog_metadata_only_on_the_selected_connection() {
    let mut settings = xylo_core::AppSettings::default();
    settings.provider.custom = Some(Default::default());
    settings.provider.model = "primary".into();
    settings.provider.reasoning_options = Some(vec!["high".into()]);
    settings.provider.reasoning_effort = Some("high".into());
    let mut catalog = settings.provider.clone();
    catalog.model = "secondary".into();
    catalog.model_capabilities = Some(xylo_core::ModelCapabilities {
        supported_apis: Some(vec![xylo_core::ApiFamily::Anthropic]),
        native_family: Some(xylo_core::ApiFamily::Anthropic),
        ..Default::default()
    });
    catalog.reasoning_options = Some(vec!["low".into()]);
    catalog.default_reasoning_effort = Some("low".into());
    settings.providers = vec![catalog];
    let provider = turn_provider(&settings, Some("secondary"));
    assert_eq!(provider.effective_family(), xylo_core::ApiFamily::Anthropic);
    assert_eq!(provider.reasoning_options, Some(vec!["low".into()]));
    assert_eq!(provider.default_reasoning_effort.as_deref(), Some("low"));
    assert!(provider.reasoning_effort.is_none());
    settings.providers[0].credential_ref = Some("other-account".into());
    let provider = turn_provider(&settings, Some("secondary"));
    assert!(provider.model_capabilities.is_none());
    assert!(provider.reasoning_options.is_none());
    assert!(provider.reasoning_effort.is_none());
}
