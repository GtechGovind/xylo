use super::*;
use std::os::unix::fs::MetadataExt;

struct Fixture {
    _directory: tempfile::TempDir,
    manager: Manager,
    operation: Operation,
    lease: Arc<RunLease>,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let manager = Manager::open(
            &CapabilityConfig {
                workspace: root.clone(),
                artifacts: root.join("artifacts"),
                management_root: Some(root.join("management")),
                ..Default::default()
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let profile = EnvironmentProfile {
            id: Uuid::new_v4(),
            name: "Capture fixture".into(),
            target: EnvironmentTarget::Container {
                executable: "/bin/sh".into(),
                runtime: ContainerRuntime::Docker,
                container: ContainerTarget::Existing {
                    target: "unused".into(),
                },
            },
        };
        let operation = Operation {
            created_at: now_ms(),
            digest: "fixture-request-digest".into(),
            expected_revision: 1,
            previous_container_id: None,
            receipt: EnvironmentReceipt {
                operation_id: Uuid::new_v4(),
                environment_id: profile.id,
                action: EnvironmentAction::Execute,
                state: EnvironmentOperationState::Prepared,
                environment: None,
                output: None,
                message: "Prepared".into(),
                created_at_ms: Some(now_ms()),
                command_label: Some("fixture".into()),
            },
            profile,
        };
        let lease = RunLease::new(
            manager.root.clone(),
            manager
                .operation_lease(operation.receipt.operation_id, &CancellationToken::new())
                .await
                .unwrap(),
            manager
                .profile_lease(operation.profile.id, &CancellationToken::new())
                .await
                .unwrap(),
        )
        .unwrap();
        Self {
            _directory: directory,
            manager,
            operation,
            lease,
        }
    }

    fn prepare(&self) -> Session {
        prepare(
            &self.manager,
            &self.operation,
            32,
            &CancellationToken::new(),
        )
        .unwrap()
    }

    async fn run(&self, command: &str, timeout: Duration, cancel: CancellationToken) -> Operation {
        let spec = process::ProcessSpec {
            executable: "/bin/sh".into(),
            args: vec!["-c".into(), command.into()],
            cwd: self.manager.artifacts.clone(),
            env: Default::default(),
            timeout,
            output_limit: 32,
        };
        tokio::time::timeout(
            Duration::from_secs(12),
            self.prepare().run(spec, 32, cancel, self.lease.clone()),
        )
        .await
        .expect("Native fixture must finish within its cleanup deadline")
        .unwrap()
    }
}

fn assert_artifacts(manager: &Manager, operation: &Operation) {
    verify(manager, operation).unwrap();
    let output = operation.receipt.output.as_ref().unwrap();
    assert_eq!(output.artifacts.len(), 2);
    for artifact in &output.artifacts {
        let bytes = std::fs::read(&artifact.path).unwrap();
        assert_eq!(artifact.stored_bytes, bytes.len() as u64);
        assert_eq!(artifact.sha256, format!("{:x}", Sha256::digest(&bytes)));
        assert_eq!(std::fs::metadata(&artifact.path).unwrap().nlink(), 1);
    }
}

#[tokio::test]
async fn binary_streams_are_complete_and_separate_from_bounded_lossy_previews() {
    let fixture = Fixture::new().await;
    let command = "printf 'begin\\000\\377'; i=0; while [ $i -lt 200 ]; do printf 'more-output'; i=$((i+1)); done; printf 'error\\000\\376' >&2";
    let operation = fixture
        .run(command, Duration::from_secs(3), CancellationToken::new())
        .await;
    assert_eq!(
        operation.receipt.state,
        EnvironmentOperationState::Succeeded
    );
    assert_artifacts(&fixture.manager, &operation);
    let output = operation.receipt.output.as_ref().unwrap();
    assert!(output.truncated);
    assert!(
        output.stdout.len() <= 96,
        "Only the bounded UTF-8 replacement preview belongs in JSON"
    );
    assert!(output.artifacts.iter().all(|artifact| artifact.complete
        && artifact.observed_bytes_exact
        && artifact.reason == EnvironmentCaptureReason::Eof));
    assert_eq!(
        &std::fs::read(&output.artifacts[0].path).unwrap()[..7],
        b"begin\0\xff"
    );
    assert_eq!(
        std::fs::read(&output.artifacts[1].path).unwrap(),
        b"error\0\xfe"
    );
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(
            output.artifacts[0]
                .path
                .parent()
                .unwrap()
                .join("manifest.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(manifest.get("profile").is_none() && manifest.get("command").is_none());
}

#[tokio::test]
async fn failed_command_retains_complete_output_without_claiming_rollback() {
    let fixture = Fixture::new().await;
    let operation = fixture
        .run(
            "printf 'partial side effect'; printf 'failure evidence' >&2; exit 7",
            Duration::from_secs(3),
            CancellationToken::new(),
        )
        .await;
    assert_eq!(operation.receipt.state, EnvironmentOperationState::Failed);
    assert_artifacts(&fixture.manager, &operation);
    let output = operation.receipt.output.as_ref().unwrap();
    assert_eq!(output.exit_code, Some(7));
    assert!(!output.success);
    assert!(output.artifacts.iter().all(|artifact| artifact.complete));
}

#[tokio::test]
async fn later_stream_or_manifest_tampering_is_detected_without_overwriting_evidence() {
    for manifest in [false, true] {
        let fixture = Fixture::new().await;
        let operation = fixture
            .run(
                "printf 'original evidence'",
                Duration::from_secs(3),
                CancellationToken::new(),
            )
            .await;
        let output = operation.receipt.output.as_ref().unwrap();
        let path = if manifest {
            output.artifacts[0]
                .path
                .parent()
                .unwrap()
                .join("manifest.json")
        } else {
            output.artifacts[0].path.clone()
        };
        std::fs::write(&path, b"tampered evidence").unwrap();
        assert!(verify(&fixture.manager, &operation).is_err());
        assert!(recover(&fixture.manager, &operation).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"tampered evidence");
    }
}

#[tokio::test]
async fn cancellation_and_timeout_retain_prefixes_with_unknown_effects() {
    for cancelled in [false, true] {
        let fixture = Fixture::new().await;
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let marker = fixture.manager.artifacts.join("ready");
        let interrupt = tokio::spawn(async move {
            if cancelled {
                tokio::time::timeout(Duration::from_secs(3), async {
                    while !marker.exists() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
                stop.cancel();
            }
        });
        let operation = fixture
            .run(
                "printf 'retained stdout'; printf 'retained stderr' >&2; touch ready; sleep 20",
                Duration::from_secs(1),
                cancel,
            )
            .await;
        interrupt.await.unwrap();
        assert_eq!(
            operation.receipt.state,
            EnvironmentOperationState::OutcomeUnknown
        );
        assert_artifacts(&fixture.manager, &operation);
        for artifact in &operation.receipt.output.as_ref().unwrap().artifacts {
            assert!(!artifact.complete);
            assert!(artifact.stored_bytes > 0);
            assert_eq!(
                artifact.reason,
                if cancelled {
                    EnvironmentCaptureReason::Cancelled
                } else {
                    EnvironmentCaptureReason::TimedOut
                }
            );
        }
    }
}

#[tokio::test]
async fn stream_ceiling_still_drains_and_reports_the_exact_observed_total() {
    let fixture = Fixture::new().await;
    let bytes = STREAM_LIMIT + 17;
    let command = format!("head -c {bytes} /dev/zero; printf done >&2");
    let operation = fixture
        .run(&command, Duration::from_secs(6), CancellationToken::new())
        .await;
    assert_eq!(
        operation.receipt.state,
        EnvironmentOperationState::Succeeded
    );
    assert_artifacts(&fixture.manager, &operation);
    let output = operation.receipt.output.unwrap();
    let stdout = &output.artifacts[0];
    assert_eq!(stdout.stored_bytes, STREAM_LIMIT);
    assert_eq!(stdout.observed_bytes, bytes);
    assert!(stdout.observed_bytes_exact);
    assert!(!stdout.complete);
    assert_eq!(stdout.reason, EnvironmentCaptureReason::ByteLimit);
    assert_eq!(std::fs::read(&output.artifacts[1].path).unwrap(), b"done");
}

#[tokio::test]
async fn missing_outer_receipt_recovers_retained_prefix_without_replaying() {
    let fixture = Fixture::new().await;
    let mut session = fixture.prepare();
    session.intent.dispatched = true;
    session.write_intent().unwrap();
    session
        .stdout
        .take()
        .unwrap()
        .write_all(b"before abrupt exit")
        .unwrap();
    session
        .stderr
        .take()
        .unwrap()
        .write_all(b"last diagnostic")
        .unwrap();
    drop(session);
    let id = fixture.operation.receipt.operation_id;
    let located = fixture.manager.operation(id).unwrap().unwrap();
    assert_eq!(located.receipt.state, EnvironmentOperationState::Running);
    assert!(located.receipt.output.is_none());
    assert!(
        fixture
            .manager
            .all_operations()
            .unwrap()
            .iter()
            .any(|operation| operation.receipt.operation_id == id)
    );
    let recovered = recover(&fixture.manager, &located).unwrap().unwrap();
    assert_eq!(
        recovered.receipt.state,
        EnvironmentOperationState::OutcomeUnknown
    );
    assert_artifacts(&fixture.manager, &recovered);
    let output = recovered.receipt.output.as_ref().unwrap();
    assert_eq!(output.stdout, "before abrupt exit");
    assert!(output.artifacts.iter().all(|artifact| !artifact.complete
        && !artifact.observed_bytes_exact
        && artifact.reason == EnvironmentCaptureReason::Interrupted));
    let repeated = recover(&fixture.manager, &located).unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(repeated.receipt).unwrap(),
        serde_json::to_value(recovered.receipt).unwrap()
    );
}

#[tokio::test]
async fn exact_link_publication_crash_is_recovered_but_unknown_aliases_are_refused() {
    let fixture = Fixture::new().await;
    let session = fixture.prepare();
    let mut source = session.stdout.unwrap();
    source.write_all(b"crash-window bytes").unwrap();
    source.sync_all().unwrap();
    let (hash, bytes) = capture_fs::hash_file(&mut source, STREAM_LIMIT).unwrap();
    let source_path = session.directory.path.join("stdout.part");
    let destination_path = session.target.path.join("stdout.bin");
    std::fs::hard_link(&source_path, &destination_path).unwrap();
    let extra_path = fixture.manager.artifacts.join("unknown-alias");
    std::fs::hard_link(&source_path, &extra_path).unwrap();
    assert!(
        session
            .directory
            .resume_publication("stdout.part", &session.target, "stdout.bin", &hash, bytes)
            .is_err()
    );
    assert!(source_path.exists() && extra_path.exists());
    std::fs::remove_file(extra_path).unwrap();
    assert!(
        session
            .directory
            .resume_publication("stdout.part", &session.target, "stdout.bin", &hash, bytes)
            .unwrap()
    );
    assert!(!source_path.exists());
    assert_eq!(
        std::fs::read(destination_path).unwrap(),
        b"crash-window bytes"
    );
}

#[tokio::test]
async fn unrelated_destination_and_hash_tampering_never_authorize_publication() {
    let fixture = Fixture::new().await;
    let session = fixture.prepare();
    let mut source = session.stdout.unwrap();
    source.write_all(b"owned bytes").unwrap();
    source.sync_all().unwrap();
    let (hash, bytes) = capture_fs::hash_file(&mut source, STREAM_LIMIT).unwrap();
    session
        .target
        .file("stdout.bin", true)
        .unwrap()
        .write_all(b"unrelated")
        .unwrap();
    assert!(
        session
            .directory
            .resume_publication("stdout.part", &session.target, "stdout.bin", &hash, bytes)
            .is_err()
    );
    assert!(session.directory.path.join("stdout.part").exists());
    assert_eq!(
        std::fs::read(session.target.path.join("stdout.bin")).unwrap(),
        b"unrelated"
    );
}

#[tokio::test]
async fn storage_quota_refuses_new_reservation_without_evicting_previous_bytes() {
    let fixture = Fixture::new().await;
    let directory = root(&fixture.manager, true).unwrap();
    let retained_id = Uuid::new_v4();
    let usage = Usage {
        version: 1,
        reservations: BTreeMap::from([(retained_id, STORAGE_LIMIT - MAX_USAGE as u64)]),
        pending: BTreeMap::new(),
        intents: BTreeMap::new(),
    };
    directory
        .write(
            "usage.json",
            &serde_json::to_vec(&usage).unwrap(),
            MAX_USAGE,
        )
        .unwrap();
    let error = prepare(
        &fixture.manager,
        &fixture.operation,
        32,
        &CancellationToken::new(),
    )
    .err()
    .unwrap();
    assert!(error.downcast_ref::<QuotaUnavailable>().is_some());
    let after: Usage =
        serde_json::from_slice(&directory.read("usage.json", MAX_USAGE).unwrap()).unwrap();
    assert_eq!(after.reservations, usage.reservations);
    assert!(after.pending.is_empty());
}

#[tokio::test]
async fn symlink_hardlink_and_renamed_authorities_never_escape_artifact_storage() {
    let fixture = Fixture::new().await;
    let session = fixture.prepare();
    let outside = fixture.manager.root.join("private-secret");
    std::fs::write(&outside, b"private bytes").unwrap();
    std::os::unix::fs::symlink(&outside, session.target.path.join("symlink.bin")).unwrap();
    assert!(session.target.file("symlink.bin", false).is_err());
    std::fs::hard_link(&outside, session.target.path.join("hardlink.bin")).unwrap();
    assert!(session.target.file("hardlink.bin", false).is_err());
    let moved = session.target.path.with_extension("moved");
    std::fs::rename(&session.target.path, &moved).unwrap();
    std::os::unix::fs::symlink(&fixture.manager.root, &session.target.path).unwrap();
    assert!(session.target.verify_named().is_err());
    assert_eq!(std::fs::read(outside).unwrap(), b"private bytes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_blocking_output_work_retains_real_operation_and_profile_leases() {
    let fixture = Fixture::new().await;
    let (started, admitted) = std::sync::mpsc::channel();
    let (release, finished) = std::sync::mpsc::channel();
    let guard = fixture.lease.clone();
    let task = workbench_resources::spawn_blocking(move || {
        let _lease = guard;
        started.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(3)).unwrap();
    });
    admitted.recv_timeout(Duration::from_secs(2)).unwrap();
    let Fixture {
        _directory,
        manager,
        operation,
        lease,
    } = fixture;
    drop(lease);
    task.abort(); // abort cannot stop already executing blocking IO.
    assert!(
        manager
            .try_operation_lease(operation.receipt.operation_id)
            .unwrap()
            .is_none()
    );
    assert!(
        manager
            .try_profile_lease(operation.profile.id)
            .unwrap()
            .is_none()
    );
    assert_eq!(active(&manager.root), 1);
    release.send(()).unwrap();
    task.await.unwrap();
    assert_eq!(active(&manager.root), 0);
    assert!(
        manager
            .try_operation_lease(operation.receipt.operation_id)
            .unwrap()
            .is_some()
    );
    drop(_directory);
}

#[tokio::test]
async fn every_pre_intent_setup_crash_remains_discoverable_and_reconciles() {
    for boundary in 0..4 {
        let fixture = Fixture::new().await;
        let intent = Intent {
            version: 1,
            operation: fixture.operation.clone(),
            artifacts_root: fixture.manager.artifacts.clone(),
            dispatched: false,
            staged: false,
            published: false,
            preview_limit: 32,
        };
        let id = fixture.operation.receipt.operation_id;
        update_usage(
            &fixture.manager,
            id,
            RESERVATION,
            Some(&intent),
            &CancellationToken::new(),
        )
        .unwrap();
        if boundary > 0 {
            let directory = root(&fixture.manager, false)
                .unwrap()
                .child(&id.to_string(), true)
                .unwrap();
            directory
                .file("stdout.part", true)
                .unwrap()
                .write_all(b"retained prefix")
                .unwrap();
            if boundary > 1 {
                directory.file("stderr.part", true).unwrap();
            }
            if boundary > 2 {
                Directory::open(&fixture.manager.artifacts)
                    .unwrap()
                    .child("environments", true)
                    .unwrap()
                    .child(&id.to_string(), true)
                    .unwrap();
            }
        }
        let located = fixture.manager.operation(id).unwrap().unwrap();
        assert_eq!(located.receipt.state, EnvironmentOperationState::Running);
        assert!(
            fixture
                .manager
                .all_operations()
                .unwrap()
                .iter()
                .any(|operation| operation.receipt.operation_id == id)
        );
        let recovered = recover(&fixture.manager, &located).unwrap().unwrap();
        assert_eq!(
            recovered.receipt.state,
            EnvironmentOperationState::OutcomeUnknown,
            "Missing metadata alone must never assert non-dispatch"
        );
        assert_artifacts(&fixture.manager, &recovered);
        let output = recovered.receipt.output.as_ref().unwrap();
        assert!(
            output
                .artifacts
                .iter()
                .all(|artifact| !artifact.complete && !artifact.observed_bytes_exact)
        );
        if boundary > 0 {
            assert_eq!(
                std::fs::read(&output.artifacts[0].path).unwrap(),
                b"retained prefix"
            );
        }
        let directory = root(&fixture.manager, false).unwrap();
        let usage: Usage =
            serde_json::from_slice(&directory.read("usage.json", MAX_USAGE).unwrap()).unwrap();
        assert_eq!(
            usage.reservations[&id],
            ENTRY_RESERVATION
                + output
                    .artifacts
                    .iter()
                    .map(|artifact| artifact.stored_bytes)
                    .sum::<u64>(),
            "Quiescent recovery releases unused byte reservation while retaining actual prefixes and metadata"
        );
    }
}
