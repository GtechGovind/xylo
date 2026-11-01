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
