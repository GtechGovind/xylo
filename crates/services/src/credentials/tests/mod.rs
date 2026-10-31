use super::*;
use std::path::PathBuf;

fn key(byte: u8) -> VaultKey {
    VaultKey::from_bytes([byte; 32])
}

fn vault_path(dir: &Path) -> PathBuf {
    dir.join(file::VAULT_FILE)
}

#[test]
fn secrets_round_trip_and_persist_across_instances() {
    let dir = tempfile::tempdir().unwrap();
    let vault = CredentialVault::unlocked(dir.path(), key(1)).unwrap();
    assert_eq!(vault.get("provider").unwrap(), None);
    vault.set("provider", "sk-first").unwrap();
    vault.set("provider", "sk-second").unwrap();
    vault.set("connector", "token").unwrap();
    assert_eq!(vault.get("provider").unwrap().as_deref(), Some("sk-second"));

    let reopened = CredentialVault::unlocked(dir.path(), key(1)).unwrap();
    assert_eq!(
        reopened.get("provider").unwrap().as_deref(),
        Some("sk-second")
    );
    reopened.delete("provider").unwrap();
    reopened.delete("never-stored").unwrap();
    assert_eq!(reopened.get("provider").unwrap(), None);
    assert_eq!(vault.get("provider").unwrap(), None);
    assert_eq!(vault.get("connector").unwrap().as_deref(), Some("token"));

    let raw = std::fs::read(vault_path(dir.path())).unwrap();
    assert!(!raw.windows(5).any(|window| window == b"token"));
}

#[test]
fn invalid_references_and_empty_secrets_are_rejected() {
    let vault = CredentialVault::for_tests();
    assert!(vault.set(" ", "secret").is_err());
    assert!(vault.set(&"r".repeat(257), "secret").is_err());
    assert!(vault.set("reference", "").is_err());
    assert!(vault.get("").is_err());
}

#[test]
fn wrong_key_fails_closed_and_never_rewrites_the_vault() {
    let dir = tempfile::tempdir().unwrap();
    CredentialVault::unlocked(dir.path(), key(1))
        .unwrap()
        .set("provider", "secret")
        .unwrap();
    let before = std::fs::read(vault_path(dir.path())).unwrap();

    let wrong = CredentialVault::unlocked(dir.path(), key(2)).unwrap();
    let error = wrong.get("provider").unwrap_err();
    assert!(
        error.to_string().contains("cannot be decrypted"),
        "{error:#}"
    );
    assert!(wrong.set("other", "value").is_err());
    assert!(wrong.delete("provider").is_err());
    assert_eq!(std::fs::read(vault_path(dir.path())).unwrap(), before);
}

#[test]
fn tampered_or_truncated_files_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    CredentialVault::unlocked(dir.path(), key(1))
        .unwrap()
        .set("provider", "secret")
        .unwrap();
    let original = std::fs::read(vault_path(dir.path())).unwrap();
    let last = original.len() - 1;
    let mut flipped = original.clone();
    flipped[last] ^= 1;
    let mut header = original.clone();
    header[7] = b'2';
    for damaged in [flipped, header, original[..20].to_vec(), Vec::new()] {
        std::fs::write(vault_path(dir.path()), &damaged).unwrap();
        let vault = CredentialVault::unlocked(dir.path(), key(1)).unwrap();
        assert!(vault.get("provider").is_err());
        assert!(vault.set("provider", "replacement").is_err());
        assert_eq!(std::fs::read(vault_path(dir.path())).unwrap(), damaged);
    }
}

#[test]
fn concurrent_writers_in_separate_instances_keep_every_entry() {
    let dir = tempfile::tempdir().unwrap();
    let writers: Vec<_> = (0..8)
        .map(|writer| {
            let path = dir.path().to_owned();
            std::thread::spawn(move || {
                let vault = CredentialVault::unlocked(&path, key(3)).unwrap();
                for entry in 0..10 {
                    vault
                        .set(
                            &format!("w{writer}-e{entry}"),
                            &format!("s{writer}-{entry}"),
                        )
                        .unwrap();
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    let vault = CredentialVault::unlocked(dir.path(), key(3)).unwrap();
    for writer in 0..8 {
        for entry in 0..10 {
            assert_eq!(
                vault.get(&format!("w{writer}-e{entry}")).unwrap(),
                Some(format!("s{writer}-{entry}"))
            );
        }
    }
}

#[test]
fn cached_entries_reload_after_another_instance_writes() {
    let dir = tempfile::tempdir().unwrap();
    let first = CredentialVault::unlocked(dir.path(), key(4)).unwrap();
    let second = CredentialVault::unlocked(dir.path(), key(4)).unwrap();
    first.set("shared", "one").unwrap();
    assert_eq!(second.get("shared").unwrap().as_deref(), Some("one"));
    second.set("shared", "two").unwrap();
    assert_eq!(first.get("shared").unwrap().as_deref(), Some("two"));
    second.delete("shared").unwrap();
    assert_eq!(first.get("shared").unwrap(), None);
}

#[test]
fn local_file_key_is_created_once_at_open_and_reused() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let vault = CredentialVault::open(&data, SecretStorage::LocalFile).unwrap();
    assert!(vault.reset_notice().is_none());
    let key_path = data.join(key::KEY_FILE);
    let key_bytes = std::fs::read(&key_path).unwrap();
    vault.set("provider", "secret").unwrap();
    assert_eq!(std::fs::read(&key_path).unwrap(), key_bytes);
    assert_eq!(key_bytes.len(), 32);

    let reopened = CredentialVault::open(&data, SecretStorage::LocalFile).unwrap();
    assert_eq!(reopened.get("provider").unwrap().as_deref(), Some("secret"));
    assert_eq!(
        reopened.export_key().unwrap().as_bytes(),
        vault.export_key().unwrap().as_bytes()
    );
    assert_eq!(std::fs::read(&key_path).unwrap(), key_bytes);

    let worker = CredentialVault::unlocked(&data, reopened.export_key().unwrap()).unwrap();
    assert_eq!(worker.get("provider").unwrap().as_deref(), Some("secret"));
}

#[test]
fn concurrent_key_creation_publishes_one_key() {
    let dir = tempfile::tempdir().unwrap();
    let keys: Vec<_> = (0..8)
        .map(|_| {
            let path = dir.path().to_owned();
            std::thread::spawn(move || {
                CredentialVault::open(&path, SecretStorage::LocalFile)
                    .unwrap()
                    .export_key()
                    .unwrap()
                    .as_bytes()
                    .to_vec()
            })
        })
        .collect();
    let keys: Vec<_> = keys.into_iter().map(|key| key.join().unwrap()).collect();
    assert!(keys.windows(2).all(|pair| pair[0] == pair[1]));
    assert_eq!(
        std::fs::read(dir.path().join(key::KEY_FILE)).unwrap(),
        keys[0]
    );
}

#[test]
fn host_replaces_a_vault_it_can_never_read_and_says_so_once() {
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join(key::KEY_FILE);
    let saved = |dir: &Path| {
        let host = CredentialVault::open(dir, SecretStorage::LocalFile).unwrap();
        host.set("provider", "secret").unwrap();
        assert!(host.reset_notice().is_none());
    };
    let damage: [&dyn Fn(); 4] = [
        &|| std::fs::write(&key_path, [7u8; 32]).unwrap(), // wrong key
        &|| std::fs::write(&key_path, [7u8; 31]).unwrap(), // corrupt key
        &|| std::fs::remove_file(&key_path).unwrap(),      // missing key
        &|| std::fs::write(vault_path(dir.path()), b"XYLOVLT1 tampered").unwrap(),
    ];
    for damage in damage {
        saved(dir.path());
        damage();
        let host = CredentialVault::open(dir.path(), SecretStorage::LocalFile).unwrap();
        let notice = host.reset_notice().expect("the replacement is reported");
        assert!(notice.contains("Reconnect your providers"), "{notice}");
        assert_eq!(host.get("provider").unwrap(), None, "old secrets are gone");
        assert!(
            !vault_path(dir.path()).exists(),
            "the unreadable file is deleted"
        );
        host.set("fresh", "value").unwrap();
        let reopened = CredentialVault::open(dir.path(), SecretStorage::LocalFile).unwrap();
        assert!(reopened.reset_notice().is_none());
        assert_eq!(reopened.get("fresh").unwrap().as_deref(), Some("value"));
    }
}

#[test]
fn worker_with_a_wrong_key_fails_closed_and_deletes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let host = CredentialVault::open(dir.path(), SecretStorage::LocalFile).unwrap();
    host.set("provider", "secret").unwrap();
    let vault_before = std::fs::read(vault_path(dir.path())).unwrap();
    let key_before = std::fs::read(dir.path().join(key::KEY_FILE)).unwrap();
    let worker = CredentialVault::unlocked(dir.path(), key(9)).unwrap();
    assert!(worker.get("provider").is_err());
    assert!(worker.set("other", "value").is_err());
    assert!(worker.reset_notice().is_none());
    assert_eq!(std::fs::read(vault_path(dir.path())).unwrap(), vault_before);
    assert_eq!(
        std::fs::read(dir.path().join(key::KEY_FILE)).unwrap(),
        key_before
    );
}

#[cfg(unix)]
#[test]
fn transient_read_failures_are_errors_and_delete_nothing() {
    let dir = tempfile::tempdir().unwrap();
    CredentialVault::open(dir.path(), SecretStorage::LocalFile).unwrap();
    let key_before = std::fs::read(dir.path().join(key::KEY_FILE)).unwrap();
    // A directory where the vault file belongs fails with an I/O error, which
    // says nothing about whether the data is readable.
    std::fs::create_dir(vault_path(dir.path())).unwrap();
    assert!(CredentialVault::open(dir.path(), SecretStorage::LocalFile).is_err());
    assert!(vault_path(dir.path()).is_dir());
    assert_eq!(
        std::fs::read(dir.path().join(key::KEY_FILE)).unwrap(),
        key_before
    );
}

#[test]
fn keychain_host_replaces_a_vault_whose_key_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<keyring_core::CredentialStore> = keyring_core::mock::Store::new().unwrap();
    let host =
        CredentialVault::open_with_keychain_store(dir.path(), SecretStorage::OsKeychain, store)
            .unwrap();
    host.set("provider", "secret").unwrap();
    let fresh_keychain: Arc<keyring_core::CredentialStore> =
        keyring_core::mock::Store::new().unwrap();
    let replaced = CredentialVault::open_with_keychain_store(
        dir.path(),
        SecretStorage::OsKeychain,
        fresh_keychain.clone(),
    )
    .unwrap();
    assert!(replaced.reset_notice().is_some());
    assert_eq!(replaced.get("provider").unwrap(), None);
    assert!(
        keychain_item(&fresh_keychain).is_some(),
        "a new key was created"
    );
}

#[cfg(unix)]
#[test]
fn vault_and_key_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let vault = CredentialVault::open(dir.path(), SecretStorage::LocalFile).unwrap();
    vault.set("provider", "secret").unwrap();
    vault.set("provider", "rotated").unwrap();
    for name in [key::KEY_FILE, file::VAULT_FILE, "secrets.vault.lock"] {
        let mode = std::fs::metadata(dir.path().join(name))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{name}");
    }
}

#[test]
fn keychain_mode_keeps_one_key_item_and_no_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<keyring_core::CredentialStore> = keyring_core::mock::Store::new().unwrap();
    let host = CredentialVault::with_keychain_store(dir.path(), store.clone());
    host.set("provider", "secret").unwrap();
    assert!(!dir.path().join(key::KEY_FILE).exists());

    let item = store
        .build(xylo_core::CREDENTIAL_SERVICE, "vault-key", None)
        .unwrap()
        .get_secret()
        .unwrap();
    assert_eq!(item.as_slice(), host.export_key().unwrap().as_bytes());

    let reopened = CredentialVault::with_keychain_store(dir.path(), store.clone());
    assert_eq!(reopened.get("provider").unwrap().as_deref(), Some("secret"));
    assert_eq!(
        reopened.export_key().unwrap().as_bytes(),
        host.export_key().unwrap().as_bytes()
    );

    let empty_keychain =
        CredentialVault::with_keychain_store(dir.path(), keyring_core::mock::Store::new().unwrap());
    assert!(
        empty_keychain.get("provider").is_err(),
        "an existing vault is never paired with a fresh key"
    );
}

#[test]
fn unlocked_vault_creates_no_key_material() {
    let dir = tempfile::tempdir().unwrap();
    let vault = CredentialVault::unlocked(dir.path(), key(9)).unwrap();
    vault.set("provider", "secret").unwrap();
    assert!(!dir.path().join(key::KEY_FILE).exists());
    assert_eq!(vault.export_key().unwrap().as_bytes(), &[9; 32]);
}

#[test]
fn vault_key_is_redacted_and_round_trips_through_worker_credentials() {
    let key = key(0xab);
    assert_eq!(format!("{key:?}"), "VaultKey([REDACTED])");
    let credential = xylo_core::WorkerCredential::from(&key);
    assert_eq!(
        VaultKey::try_from(&credential).unwrap().as_bytes(),
        key.as_bytes()
    );
    for invalid in [None, Some("zz".to_owned()), Some("ab".repeat(31))] {
        assert!(VaultKey::try_from(&xylo_core::WorkerCredential(invalid)).is_err());
    }
}

fn keychain_item(store: &Arc<keyring_core::CredentialStore>) -> Option<Vec<u8>> {
    match store
        .build(xylo_core::CREDENTIAL_SERVICE, "vault-key", None)
        .unwrap()
        .get_secret()
    {
        Ok(bytes) => Some(bytes),
        Err(keyring_core::Error::NoEntry) => None,
        Err(error) => panic!("{error}"),
    }
}

#[test]
fn switching_storage_starts_an_empty_vault_and_leaves_no_old_key() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<keyring_core::CredentialStore> = keyring_core::mock::Store::new().unwrap();
    let host = CredentialVault::open_with_keychain_store(
        dir.path(),
        SecretStorage::LocalFile,
        store.clone(),
    )
    .unwrap();
    host.set("provider", "secret").unwrap();
    let file_key = host.export_key().unwrap();

    host.change_storage(dir.path(), SecretStorage::OsKeychain)
        .unwrap();
    assert_eq!(host.storage(), Some(SecretStorage::OsKeychain));
    assert!(
        !dir.path().join(key::KEY_FILE).exists(),
        "the key file is deleted"
    );
    assert!(!vault_path(dir.path()).exists(), "the old vault is deleted");
    assert_eq!(host.get("provider").unwrap(), None, "secrets are discarded");
    let keychain_key = keychain_item(&store).expect("a new key lives in the keychain");
    assert_ne!(
        keychain_key.as_slice(),
        file_key.as_bytes(),
        "the key is new"
    );
    host.set("second", "value").unwrap();
    let reopened = CredentialVault::with_keychain_store(dir.path(), store.clone());
    assert_eq!(reopened.get("second").unwrap().as_deref(), Some("value"));

    reopened
        .change_storage(dir.path(), SecretStorage::LocalFile)
        .unwrap();
    assert!(
        keychain_item(&store).is_none(),
        "the keychain item is deleted"
    );
    assert_eq!(reopened.get("second").unwrap(), None);
    let new_file_key = std::fs::read(dir.path().join(key::KEY_FILE)).unwrap();
    assert_ne!(new_file_key.as_slice(), keychain_key.as_slice());
    let in_file = CredentialVault::open(dir.path(), SecretStorage::LocalFile).unwrap();
    in_file.set("third", "value").unwrap();
    assert_eq!(in_file.get("third").unwrap().as_deref(), Some("value"));
}

#[test]
fn changing_to_the_current_storage_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let host = CredentialVault::open(dir.path(), SecretStorage::LocalFile).unwrap();
    host.set("provider", "secret").unwrap();
    let before = std::fs::read(dir.path().join(key::KEY_FILE)).unwrap();
    host.change_storage(dir.path(), SecretStorage::LocalFile)
        .unwrap();
    assert_eq!(
        std::fs::read(dir.path().join(key::KEY_FILE)).unwrap(),
        before
    );
    assert_eq!(host.get("provider").unwrap().as_deref(), Some("secret"));
}

#[test]
fn only_a_host_vault_in_its_own_folder_can_change_storage() {
    let dir = tempfile::tempdir().unwrap();
    let worker = CredentialVault::unlocked(dir.path(), key(3)).unwrap();
    assert!(
        worker
            .change_storage(dir.path(), SecretStorage::LocalFile)
            .is_err()
    );
    let host = CredentialVault::open(dir.path(), SecretStorage::LocalFile).unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    assert!(
        host.change_storage(elsewhere.path(), SecretStorage::OsKeychain)
            .is_err()
    );
}

fn record(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(key::STORAGE_FILE)).unwrap()
}

fn mock_store() -> Arc<keyring_core::CredentialStore> {
    keyring_core::mock::Store::new().unwrap()
}

/// Makes the next call on the vault key item in `store` fail transiently.
fn fail_next_keychain_call(store: &Arc<keyring_core::CredentialStore>) {
    let entry = store
        .build(xylo_core::CREDENTIAL_SERVICE, "vault-key", None)
        .unwrap();
    let mock: &keyring_core::mock::Cred = entry.as_any().downcast_ref().unwrap();
    mock.set_error(keyring_core::Error::Invalid(
        "mock".into(),
        "keychain is locked".into(),
    ));
}

#[test]
fn the_record_not_the_requested_storage_decides_where_an_existing_key_is_read() {
    // A reset settings database asks for the default (key file) storage; the
    // keychain vault must open unchanged instead of being replaced.
    let dir = tempfile::tempdir().unwrap();
    let store = mock_store();
    let host = CredentialVault::open_with_keychain_store(
        dir.path(),
        SecretStorage::OsKeychain,
        store.clone(),
    )
    .unwrap();
    host.set("provider", "secret").unwrap();
    assert_eq!(record(dir.path()), "os-keychain\n");

    let reopened = CredentialVault::open_with_keychain_store(
        dir.path(),
        SecretStorage::LocalFile,
        store.clone(),
    )
    .unwrap();
    assert!(reopened.reset_notice().is_none());
    assert_eq!(reopened.storage(), Some(SecretStorage::OsKeychain));
    assert_eq!(reopened.get("provider").unwrap().as_deref(), Some("secret"));
    assert!(
        !dir.path().join(key::KEY_FILE).exists(),
        "no key file is created"
    );
    assert_eq!(
        CredentialVault::recorded_storage(dir.path()).unwrap(),
        Some(SecretStorage::OsKeychain)
    );
}

#[test]
fn without_a_record_both_places_are_tried_before_anything_is_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let store = mock_store();
    CredentialVault::open_with_keychain_store(dir.path(), SecretStorage::OsKeychain, store.clone())
        .unwrap()
        .set("provider", "secret")
        .unwrap();
    std::fs::remove_file(dir.path().join(key::STORAGE_FILE)).unwrap();

    let host = CredentialVault::open_with_keychain_store(
        dir.path(),
        SecretStorage::LocalFile,
        store.clone(),
    )
    .unwrap();
    assert!(host.reset_notice().is_none());
    assert_eq!(host.get("provider").unwrap().as_deref(), Some("secret"));
    assert_eq!(host.storage(), Some(SecretStorage::OsKeychain));
    assert_eq!(
        record(dir.path()),
        "os-keychain\n",
        "the record is restored"
    );

    // An unrecognized record is no record at all.
    std::fs::write(dir.path().join(key::STORAGE_FILE), "something else").unwrap();
    let host =
        CredentialVault::open_with_keychain_store(dir.path(), SecretStorage::LocalFile, store)
            .unwrap();
    assert_eq!(host.get("provider").unwrap().as_deref(), Some("secret"));
}

#[test]
fn without_a_record_a_place_that_cannot_be_checked_deletes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = mock_store();
    CredentialVault::open_with_keychain_store(dir.path(), SecretStorage::OsKeychain, store.clone())
        .unwrap()
        .set("provider", "secret")
        .unwrap();
    std::fs::remove_file(dir.path().join(key::STORAGE_FILE)).unwrap();
    let before = std::fs::read(vault_path(dir.path())).unwrap();

    fail_next_keychain_call(&store);
    assert!(
        CredentialVault::open_with_keychain_store(
            dir.path(),
            SecretStorage::LocalFile,
            store.clone()
        )
        .is_err()
    );
    assert_eq!(std::fs::read(vault_path(dir.path())).unwrap(), before);
    assert!(keychain_item(&store).is_some(), "the key is kept");
}

#[test]
fn with_no_vault_the_recorded_place_still_wins() {
    let dir = tempfile::tempdir().unwrap();
    let store = mock_store();
    CredentialVault::open_with_keychain_store(dir.path(), SecretStorage::LocalFile, store.clone())
        .unwrap();
    let host = CredentialVault::open_with_keychain_store(
        dir.path(),
        SecretStorage::OsKeychain,
        store.clone(),
    )
    .unwrap();
    assert_eq!(host.storage(), Some(SecretStorage::LocalFile));
    assert!(keychain_item(&store).is_none());
}

#[test]
fn a_new_key_that_cannot_be_saved_leaves_the_old_vault_in_use() {
    let dir = tempfile::tempdir().unwrap();
    let store = mock_store();
    let host = CredentialVault::open_with_keychain_store(
        dir.path(),
        SecretStorage::LocalFile,
        store.clone(),
    )
    .unwrap();
    host.set("provider", "secret").unwrap();
    let key_before = std::fs::read(dir.path().join(key::KEY_FILE)).unwrap();

    fail_next_keychain_call(&store);
    assert!(
        host.change_storage(dir.path(), SecretStorage::OsKeychain)
            .is_err()
    );
    assert_eq!(host.storage(), Some(SecretStorage::LocalFile));
    assert_eq!(host.get("provider").unwrap().as_deref(), Some("secret"));
    assert_eq!(
        std::fs::read(dir.path().join(key::KEY_FILE)).unwrap(),
        key_before
    );
    assert_eq!(record(dir.path()), "local-file\n");

    // A later start (with any requested storage) still opens the old vault.
    let reopened =
        CredentialVault::open_with_keychain_store(dir.path(), SecretStorage::OsKeychain, store)
            .unwrap();
    assert!(reopened.reset_notice().is_none());
    assert_eq!(reopened.get("provider").unwrap().as_deref(), Some("secret"));
}

#[test]
fn a_completed_switch_survives_a_restart_with_the_old_setting() {
    let dir = tempfile::tempdir().unwrap();
    let store = mock_store();
    let host = CredentialVault::open_with_keychain_store(
        dir.path(),
        SecretStorage::LocalFile,
        store.clone(),
    )
    .unwrap();
    host.change_storage(dir.path(), SecretStorage::OsKeychain)
        .unwrap();
    host.set("provider", "after the switch").unwrap();

    let reopened =
        CredentialVault::open_with_keychain_store(dir.path(), SecretStorage::LocalFile, store)
            .unwrap();
    assert!(reopened.reset_notice().is_none());
    assert_eq!(reopened.storage(), Some(SecretStorage::OsKeychain));
    assert_eq!(
        reopened.get("provider").unwrap().as_deref(),
        Some("after the switch")
    );
}

#[test]
fn starting_over_discards_the_vault_and_uses_the_chosen_place() {
    let dir = tempfile::tempdir().unwrap();
    let store = mock_store();
    CredentialVault::open_with_keychain_store(dir.path(), SecretStorage::OsKeychain, store.clone())
        .unwrap()
        .set("provider", "secret")
        .unwrap();

    let host = CredentialVault::start_over_with_keychain_store(
        dir.path(),
        SecretStorage::LocalFile,
        store.clone(),
    )
    .unwrap();
    assert_eq!(host.storage(), Some(SecretStorage::LocalFile));
    assert_eq!(host.get("provider").unwrap(), None);
    assert!(keychain_item(&store).is_none(), "the old key is removed");
    assert_eq!(record(dir.path()), "local-file\n");
    host.set("fresh", "value").unwrap();
    let reopened = CredentialVault::open(dir.path(), SecretStorage::OsKeychain).unwrap();
    assert_eq!(reopened.get("fresh").unwrap().as_deref(), Some("value"));
}

#[test]
fn worker_vaults_report_no_storage() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(CredentialVault::recorded_storage(dir.path()).unwrap(), None);
    let worker = CredentialVault::unlocked(dir.path(), key(3)).unwrap();
    assert_eq!(worker.storage(), None);
}
