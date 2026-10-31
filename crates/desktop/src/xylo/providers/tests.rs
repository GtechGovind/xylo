use super::{
    Activation, PROVIDER_PRESETS, apply_catalog, catalog_entries, connection_status, context_size,
    provider_kind_description, requires_api_key, same_connection, saved_connections,
};
use std::collections::BTreeMap;
use xylo_core::{ApiFamily, AppSettings, ProviderConfig};

fn connection(model: &str) -> ProviderConfig {
    ProviderConfig {
        name: "OpenAI".into(),
        family: ApiFamily::OpenAiResponses,
        endpoint: "https://api.openai.com/v1".into(),
        model: model.into(),
        credential_ref: Some("provider:openai".into()),
        ..Default::default()
    }
}

fn local(model: &str) -> ProviderConfig {
    ProviderConfig {
        name: "Local model".into(),
        endpoint: "http://127.0.0.1:11434/v1".into(),
        model: model.into(),
        ..Default::default()
    }
}

#[test]
fn discovering_another_connection_adds_its_models_without_activating_them() {
    let mut settings = AppSettings {
        provider: local("llama"),
        providers: vec![local("llama")],
        ..Default::default()
    };
    let source = connection("");
    let activation = apply_catalog(
        &mut settings,
        &source,
        vec![connection("gpt-a"), connection("gpt-b")],
    );
    assert_eq!(activation, Activation::Kept);
    assert_eq!(settings.provider.model, "llama");
    let models: Vec<_> = settings
        .providers
        .iter()
        .filter(|p| same_connection(p, &source))
        .map(|p| p.model.as_str())
        .collect();
    assert_eq!(models, ["gpt-a", "gpt-b"]);
}

#[test]
fn the_first_connection_activates_its_first_model() {
    let mut settings = AppSettings::default();
    let activation = apply_catalog(
        &mut settings,
        &connection(""),
        vec![connection("gpt-a"), connection("gpt-b")],
    );
    assert_eq!(activation, Activation::First);
    assert_eq!(settings.provider.model, "gpt-a");
}

#[test]
fn refresh_drops_vanished_models_and_replaces_a_vanished_active_one() {
    let mut settings = AppSettings {
        provider: connection("gpt-b"),
        providers: vec![connection("gpt-b"), connection("gpt-old")],
        ..Default::default()
    };
    let source = connection("");
    let registered = vec![connection("gpt-a"), connection("gpt-b")];
    assert_eq!(
        apply_catalog(&mut settings, &source, registered.clone()),
        Activation::Kept
    );
    assert_eq!(settings.provider.model, "gpt-b");
    assert!(!settings.providers.iter().any(|p| p.model == "gpt-old"));

    settings.provider = connection("gpt-removed");
    assert_eq!(
        apply_catalog(&mut settings, &source, registered),
        Activation::Replaced {
            previous: "gpt-removed".into()
        }
    );
    assert_eq!(settings.provider.model, "gpt-a");
}

#[test]
fn context_sizes_read_in_product_units() {
    assert_eq!(context_size(128_000), "128K context");
    assert_eq!(context_size(1_000_000), "1M context");
    assert_eq!(context_size(512), "512 tokens context");
}

#[test]
fn catalog_entries_keep_saved_choices_and_apply_advertised_limits() {
    let mut saved = connection("gpt-a");
    saved.context_tokens = 50_000;
    saved.output_tokens = 4_000;
    saved.reasoning_effort = Some("high".into());
    saved.reasoning_options = Some(vec!["low".into(), "high".into()]);
    let models = ["gpt-a".to_owned(), "gpt-b".to_owned()];
    let models: Vec<&String> = models.iter().collect();
    let limits = BTreeMap::from([("gpt-b".to_owned(), 200_000)]);
    let options = BTreeMap::from([("gpt-a".to_owned(), vec!["low".into(), "high".into()])]);
    let entries = catalog_entries(
        &connection(""),
        &[saved],
        &models,
        &limits,
        &options,
        &BTreeMap::new(),
    );
    assert_eq!(entries[0].context_tokens, 50_000);
    assert_eq!(entries[0].reasoning_effort.as_deref(), Some("high"));
    assert_eq!(entries[1].context_tokens, 200_000);
}

#[test]
fn saved_connections_list_each_connection_once_with_the_active_model() {
    let settings = AppSettings {
        provider: connection("gpt-b"),
        providers: vec![connection("gpt-a"), connection("gpt-b"), local("llama")],
        ..Default::default()
    };
    let rows = saved_connections(&settings);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].model, "gpt-b");
}

#[test]
fn only_hosted_presets_require_a_key() {
    assert!(requires_api_key("OpenAI"));
    assert!(!requires_api_key("Local"));
    assert!(!requires_api_key("Custom"));
}

#[test]
fn presets_keep_their_ids_and_each_kind_has_its_own_description() {
    for (id, kind, detail) in PROVIDER_PRESETS {
        assert_eq!(id, format!("preset-{}", kind.to_lowercase()));
        assert!(!detail.is_empty());
    }
    assert_ne!(
        provider_kind_description("ChatGPT"),
        provider_kind_description("OpenAI")
    );
    assert_eq!(
        provider_kind_description(""),
        "Choose how Xylo connects to a model."
    );
}

/// Regression: a signed-in ChatGPT connection read "Needs setup" because the
/// list judged it by account status that is only read when its editor opens.
#[test]
fn a_chatgpt_connection_with_models_is_connected_before_its_account_status_is_read() {
    use super::settings_ui::Status;
    assert_eq!(
        connection_status("ChatGPT", true, 3, false, None),
        Status::Connected
    );
    assert_eq!(
        connection_status("ChatGPT", true, 3, false, Some(true)),
        Status::Connected
    );
    assert_eq!(
        connection_status("ChatGPT", true, 3, false, Some(false)),
        Status::NeedsSetup
    );
    assert_eq!(
        connection_status("ChatGPT", true, 0, false, None),
        Status::NeedsSetup
    );
    assert_eq!(
        connection_status("OpenAI", false, 3, false, None),
        Status::NeedsSetup
    );
    assert_eq!(
        connection_status("Local", false, 2, false, None),
        Status::Connected
    );
    assert_eq!(
        connection_status("OpenAI", true, 3, true, Some(true)),
        Status::Error
    );
}
