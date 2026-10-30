//! Test keys are generated in memory, never used as publisher credentials.
use super::*;
use minisign::KeyPair;
use std::io::Cursor;
fn keys() -> KeyPair {
    KeyPair::generate_unencrypted_keypair().unwrap()
}

#[test]
fn fresh_build_does_not_require_packaging_configuration_or_create_update_state() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("updates");
    assert!(
        Updater::for_application(data.clone(), directory.path().join("xylo"))
            .unwrap()
            .is_none()
    );
    assert!(!data.exists());
    let policy = UpdatePolicy::default();
    assert_eq!(policy.application_id, "com.qurkos.xylo");
    assert!(policy.validate().is_err());
}
fn signature(keys: &KeyPair, bytes: &[u8]) -> String {
    STANDARD.encode(
        minisign::sign(
            Some(&keys.pk),
            &keys.sk,
            Cursor::new(bytes),
            Some("test fixture only"),
            None,
        )
        .unwrap()
        .to_string(),
    )
}
fn fixture(keys: &KeyPair) -> (tempfile::TempDir, Updater) {
    let dir = tempfile::tempdir().unwrap();
    let origin = "https://updates.example.test";
    let policy = UpdatePolicy {
        enabled: true,
        application_id: "com.qurkos.xylo".into(),
        channel: "stable".into(),
        manifest_url: Some(format!("{origin}/stable.json").parse().unwrap()),
        signature_url: Some(format!("{origin}/stable.json.sig").parse().unwrap()),
        public_key: Some(STANDARD.encode(keys.pk.to_box().unwrap().to_bytes())),
        allowed_origins: vec![origin.into()],
        max_artifact_bytes: 1024 * 1024,
        macos_team_id: Some("TEST000000".into()),
    };
    let updater = Updater::new(
        policy,
        "0.1.0".parse().unwrap(),
        native_target().unwrap(),
        dir.path().join("updates"),
        dir.path().join("Current.app/Contents/MacOS/xylo"),
    )
    .unwrap();
    (dir, updater)
}
fn manifest(
    updater: &Updater,
    keys: &KeyPair,
    version: &str,
    sequence: u64,
    bytes: &[u8],
) -> ReleaseManifest {
    let at = now().unwrap();
    let artifact = ReleaseArtifact {
        url: "https://updates.example.test/package".parse().unwrap(),
        signature: signature(keys, bytes),
        format: if updater.target.starts_with("macos-") {
            "app"
        } else if updater.target.starts_with("linux-") {
            "appimage"
        } else {
            "nsis"
        }
        .into(),
        sha256: hex(&Sha256::digest(bytes)),
        size_bytes: bytes.len() as u64,
    };
    ReleaseManifest {
        schema: 1,
        application_id: updater.policy.application_id.clone(),
        channel: "stable".into(),
        sequence,
        version: version.parse().unwrap(),
        published_at: at - 30,
        expires_at: at + 3600,
        notes: "Test release".into(),
        platforms: BTreeMap::from([(updater.target.clone(), artifact)]),
    }
}
fn verify(
    updater: &Updater,
    keys: &KeyPair,
    manifest: &ReleaseManifest,
) -> Result<Option<VerifiedUpdate>> {
    let bytes = serde_json::to_vec(manifest)?;
    updater.verify_manifest(&bytes, &signature(keys, &bytes), now()?)
}
#[test]
fn authentic_manifest_and_artifact_require_both_signatures() {
    let key = keys();
    let (_dir, updater) = fixture(&key);
    let bytes = b"signed fixture bytes";
    let manifest = manifest(&updater, &key, "0.2.0", 2, bytes);
    let offer = verify(&updater, &key, &manifest).unwrap().unwrap();
    assert_eq!(offer.summary().version, "0.2.0");
    verify_bytes(&updater.policy, &offer.artifact, bytes).unwrap();
    assert!(verify_bytes(&updater.policy, &offer.artifact, b"tampered bytes").is_err());
    let mut metadata = serde_json::to_vec(&manifest).unwrap();
    let signed = signature(&key, &metadata);
    metadata.push(b' ');
    assert!(
        updater
            .verify_manifest(&metadata, &signed, now().unwrap())
            .is_err()
    );
    let wrong = keys();
    assert!(
        updater
            .verify_manifest(
                &serde_json::to_vec(&manifest).unwrap(),
                &signature(&wrong, &serde_json::to_vec(&manifest).unwrap()),
                now().unwrap()
            )
            .is_err()
    );
    let mut forged = offer.artifact.clone();
    forged.sha256 = hex(&Sha256::digest(b"attacker bytes"));
    forged.size_bytes = 14;
    assert!(verify_bytes(&updater.policy, &forged, b"attacker bytes").is_err());
}
#[test]
fn wrong_platform_channel_format_origin_and_expired_releases_are_rejected() {
    let key = keys();
    let (_dir, updater) = fixture(&key);
    let original = manifest(&updater, &key, "0.2.0", 2, b"bytes");
    for mutation in 0..9 {
        let mut manifest = original.clone();
        match mutation {
            0 => manifest.application_id = "other.app".into(),
            1 => manifest.channel = "beta".into(),
            2 => manifest.platforms.clear(),
            3 => manifest.platforms.get_mut(&updater.target).unwrap().format = "wrong".into(),
            4 => {
                manifest.platforms.get_mut(&updater.target).unwrap().url =
                    "https://untrusted.example/file".parse().unwrap()
            }
            5 => manifest.expires_at = now().unwrap() - 1,
            6 => manifest.published_at = now().unwrap() + 600,
            7 => {
                manifest
                    .platforms
                    .get_mut(&updater.target)
                    .unwrap()
                    .size_bytes = updater.policy.max_artifact_bytes + 1
            }
            _ => {
                manifest.published_at = i64::MIN;
                manifest.expires_at = i64::MAX;
            }
        }
        assert!(
            verify(&updater, &key, &manifest).is_err(),
            "mutation {mutation} accepted"
        );
    }
}
#[test]
fn rollback_floor_advances_only_after_install_or_running_version_reconciliation() {
    let key = keys();
    let (_dir, updater) = fixture(&key);
    let offer = verify(
        &updater,
        &key,
        &manifest(&updater, &key, "0.3.0", 3, b"bytes"),
    )
    .unwrap()
    .unwrap();
    assert!(
        updater.load_state().unwrap().highest_version.is_none(),
        "checking must not advance the installed floor"
    );
    let state = state::State {
        pending: Some(state::Pending {
            version: "0.3.0".parse().unwrap(),
            sequence: 3,
            manifest: offer.manifest_hash,
        }),
        ..Default::default()
    };
    updater.save_state(&state).unwrap();
    updater.reconcile().unwrap();
    assert!(updater.load_state().unwrap().highest_version.is_none());
    let mut restarted = updater.clone();
    restarted.current = "0.3.0".parse().unwrap();
    restarted.reconcile().unwrap();
    assert_eq!(
        restarted.load_state().unwrap().highest_version.unwrap(),
        restarted.current
    );
    assert!(
        verify(
            &restarted,
            &key,
            &manifest(&restarted, &key, "0.2.0", 4, b"bytes")
        )
        .is_err()
    );
    assert!(
        verify(
            &restarted,
            &key,
            &manifest(&restarted, &key, "0.4.0", 2, b"bytes")
        )
        .is_err()
    );
    assert!(
        verify(
            &restarted,
            &key,
            &manifest(&restarted, &key, "0.4.0", 3, b"bytes")
        )
        .is_err()
    );
}
#[test]
fn corrupted_journal_fails_closed_and_cancelled_check_never_requests_feed() {
    let key = keys();
    let (_dir, updater) = fixture(&key);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        updater
            .check(&cancel)
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    fs::write(updater.data.join("installed.json"), b"broken").unwrap();
    assert!(updater.check(&CancellationToken::new()).is_err());
}

#[test]
fn cancellation_interrupts_a_stalled_tls_handshake() {
    use std::net::TcpListener;
    use std::time::Instant;
    let key = keys();
    let (_dir, mut updater) = fixture(&key);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("https://{}", listener.local_addr().unwrap());
    updater.policy.allowed_origins = vec![origin.clone()];
    updater.policy.manifest_url = Some(format!("{origin}/release.json").parse().unwrap());
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let server = std::thread::spawn(move || {
        let (mut connection, _) = listener.accept().unwrap();
        // Receive the client's TLS hello but never send a server hello. This
        // tests cancellation while an actual connection is blocked in TLS.
        connection
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut hello = [0; 1024];
        let _ = connection.read(&mut hello).unwrap();
        stop.cancel();
        std::thread::sleep(Duration::from_millis(100));
    });
    let began = Instant::now();
    let error = updater.check(&cancel).unwrap_err();
    server.join().unwrap();
    assert!(error.to_string().contains("cancel"), "{error:#}");
    assert!(began.elapsed() < Duration::from_secs(2));
}

#[test]
fn stable_channel_rejects_prerelease_and_policy_rejects_missing_publisher() {
    let key = keys();
    let (_dir, updater) = fixture(&key);
    assert!(
        verify(
            &updater,
            &key,
            &manifest(&updater, &key, "0.2.0-beta.1", 2, b"bytes")
        )
        .is_err()
    );
    let mut policy = updater.policy.clone();
    policy.macos_team_id = None;
    let data = tempfile::tempdir().unwrap();
    assert!(
        Updater::new(
            policy,
            "0.1.0".parse().unwrap(),
            "macos-aarch64".into(),
            data.path().join("updates"),
            PathBuf::from("unused")
        )
        .is_err()
    );
}
#[test]
fn staged_bytes_are_verified_again_before_any_install_mutation() {
    let key = keys();
    let (dir, updater) = fixture(&key);
    let bytes = b"signed fixture bytes";
    let offer = verify(&updater, &key, &manifest(&updater, &key, "0.2.0", 2, bytes))
        .unwrap()
        .unwrap();
    let mut file = NamedTempFile::new_in(&updater.data).unwrap();
    file.write_all(bytes).unwrap();
    updater
        .verify_file(file.path(), &offer.artifact, &CancellationToken::new())
        .unwrap();
    fs::write(file.path(), b"tampered stage").unwrap();
    let result = updater.install(StagedUpdate { file, offer }, &CancellationToken::new());
    assert!(result.is_err());
    assert!(!dir.path().join("Current.app").exists());
    assert!(updater.load_state().unwrap().highest_version.is_none());
}
fn app_archive(link: Option<&str>) -> Vec<u8> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    for path in [
        "Contents/Info.plist",
        "Contents/MacOS/xylo",
        "Contents/MacOS/xylo-worker",
        "Contents/MacOS/xylo-helper",
    ] {
        let bytes = b"fixture release";
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        archive
            .append_data(
                &mut header,
                format!("Fixture.app/{path}"),
                Cursor::new(bytes),
            )
            .unwrap();
    }
    if let Some(link) = link {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_mode(0o777);
        header.set_size(0);
        header.set_cksum();
        archive
            .append_link(&mut header, "Fixture.app/escape", link)
            .unwrap();
    }
    archive.into_inner().unwrap().finish().unwrap()
}
#[test]
fn macos_archive_layout_is_bounded_complete_and_rejects_symlink_escape() {
    archive::preflight("macos-aarch64", "app", &app_archive(None)).unwrap();
    assert!(
        archive::preflight("macos-aarch64", "app", &app_archive(Some("../../outside"))).is_err()
    );
    assert!(archive::preflight("macos-aarch64", "nsis", b"bytes").is_err());
}
#[cfg(target_os = "macos")]
#[test]
fn macos_transaction_preserves_old_bundle_on_verification_failure_and_replaces_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("Current.app");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("old"), b"previous").unwrap();
    let archive = app_archive(None);
    assert!(
        macos::replace(&archive, &destination, |_| anyhow::bail!(
            "fixture rejects publisher"
        ))
        .is_err()
    );
    assert_eq!(fs::read(destination.join("old")).unwrap(), b"previous");
    macos::replace(&archive, &destination, |_| Ok(())).unwrap();
    assert!(!destination.join("old").exists());
    assert_eq!(
        fs::read(destination.join("Contents/MacOS/xylo-helper")).unwrap(),
        b"fixture release"
    );
}
