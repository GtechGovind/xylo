use super::secret_storage_detail;
use xylo_core::SecretStorage;

#[test]
fn secret_storage_defaults_to_the_prompt_free_local_file() {
    assert_eq!(SecretStorage::default(), SecretStorage::LocalFile);
    assert!(secret_storage_detail(SecretStorage::LocalFile).contains("No system prompts"));
}
