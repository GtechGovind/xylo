use super::{CHATGPT_REF, forget_chatgpt};
use xylo_core::{ApiFamily, AppSettings, ProviderConfig};

fn chatgpt(model: &str) -> ProviderConfig {
    ProviderConfig {
        name: "ChatGPT".into(),
        family: ApiFamily::OpenAiResponses,
        endpoint: "https://api.openai.com/v1".into(),
        model: model.into(),
        credential_ref: Some(CHATGPT_REF.into()),
        ..ProviderConfig::default()
    }
}

#[test]
fn signing_out_with_no_other_provider_leaves_none_set_up() {
    let mut settings = AppSettings {
        provider: chatgpt("gpt-fixture"),
        ..AppSettings::default()
    };
    settings.remember_model(settings.provider.clone());
    assert!(forget_chatgpt(&mut settings));
    assert!(!settings.model_ready());
    assert!(settings.providers.is_empty());
    // No phantom "Local model" takes ChatGPT's place, so no row is listed.
    assert!(settings.provider.name.is_empty());
    // A late acknowledgement finds nothing left to remove.
    assert!(!forget_chatgpt(&mut settings));
}

#[test]
fn signing_out_switches_chats_to_another_saved_model() {
    let local = ProviderConfig {
        model: "local-coder".into(),
        ..ProviderConfig::default()
    };
    let mut settings = AppSettings {
        provider: chatgpt("gpt-fixture"),
        ..AppSettings::default()
    };
    settings.remember_model(local);
    settings.remember_model(settings.provider.clone());
    assert!(forget_chatgpt(&mut settings));
    assert!(settings.model_ready());
    assert_eq!(settings.provider.model, "local-coder");
    assert_eq!(settings.providers.len(), 1);
}
