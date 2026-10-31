use super::{allowed_since, secret_storage_detail};
use xylo_core::{SecretStorage, now_ms};

#[test]
fn rule_age_reads_in_product_units() {
    let now = now_ms();
    assert_eq!(allowed_since(now), "Allowed just now");
    assert_eq!(allowed_since(now - 5 * 60_000), "Allowed 5 min ago");
    assert_eq!(allowed_since(now - 3 * 86_400_000), "Allowed 3 days ago");
    assert_eq!(allowed_since(now + 60_000), "Allowed just now");
}

#[test]
fn secret_storage_defaults_to_the_prompt_free_local_file() {
    assert_eq!(SecretStorage::default(), SecretStorage::LocalFile);
    assert!(secret_storage_detail(SecretStorage::LocalFile).contains("No system prompts"));
}
