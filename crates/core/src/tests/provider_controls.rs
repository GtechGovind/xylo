//! Provider fallback defaults and native effort capability contracts.
use super::*;

#[test]
fn provider_fallback_overrides_global_and_neither_changes_the_saved_model() {
    let mut settings = AppSettings {
        fallback_model: Some("global".into()),
        ..Default::default()
    };
    settings.provider.model = "primary".into();
    assert_eq!(
        settings.turn_provider().fallback_model.as_deref(),
        Some("global")
    );
    assert!(settings.provider.fallback_model.is_none());
    settings.provider.fallback_model = Some("provider".into());
    assert_eq!(
        settings.turn_provider().fallback_model.as_deref(),
        Some("provider")
    );
    let decoded: AppSettings =
        serde_json::from_value(serde_json::to_value(&settings).unwrap()).unwrap();
    assert_eq!(decoded.turn_provider().model, "primary");
    assert_eq!(
        decoded.turn_provider().fallback_model.as_deref(),
        Some("provider")
    );
}

#[test]
fn catalog_choices_still_override_native_effort_capabilities() {
    let mut provider = ProviderConfig {
        family: ApiFamily::Anthropic,
        model: "claude-sonnet-4-6".into(),
        ..Default::default()
    };
    assert_eq!(
        provider.supported_reasoning_efforts(),
        vec!["low", "medium", "high", "max"]
    );
    provider.reasoning_options = Some(vec![]);
    assert!(provider.supported_reasoning_efforts().is_empty());
    provider.reasoning_options = Some(vec!["low".into(), "invalid option".into(), "low".into()]);
    assert_eq!(provider.supported_reasoning_efforts(), vec!["low"]);
}
