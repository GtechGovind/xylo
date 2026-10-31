use super::*;
#[test]
fn local_server_defaults_fit_available_hardware_and_preserve_overrides() {
    let config: LlamaConfig = serde_json::from_value(json!({})).unwrap();
    let automatic = config.server_arguments(12345).unwrap();
    assert!(!automatic.iter().any(|arg| arg == "--ctx-size"));
    assert!(
        automatic
            .windows(2)
            .any(|arg| arg == ["--n-gpu-layers", "auto"])
    );
    assert!(automatic.windows(2).any(|arg| arg == ["--fit", "on"]));
    let threads = automatic.iter().position(|arg| arg == "--threads").unwrap();
    assert!(automatic[threads + 1].parse::<usize>().unwrap() >= 1);

    let explicit = LlamaConfig {
        threads: 7,
        gpu_layers: 0,
        context_tokens: 16384,
        fit_memory: false,
        ..Default::default()
    }
    .server_arguments(12345)
    .unwrap();
    for pair in [
        ["--threads", "7"],
        ["--n-gpu-layers", "0"],
        ["--ctx-size", "16384"],
        ["--fit", "off"],
    ] {
        assert!(explicit.windows(2).any(|arg| arg == pair));
    }
    assert!(
        LlamaConfig {
            gpu_layers: -3,
            ..Default::default()
        }
        .server_arguments(12345)
        .is_err()
    );
}

#[test]
fn helper_templates_preserve_argument_boundaries_and_fail_unknown_assets() {
    let values = BTreeMap::from([("input".into(), "/tmp/voice with spaces.wav".into())]);
    assert_eq!(
        expand_arguments(&["-f".into(), "{input}".into()], &values).unwrap(),
        vec!["-f", "/tmp/voice with spaces.wav"]
    );
    assert!(expand_arguments(&["{model}".into()], &values).is_err());
}
#[test]
fn speech_text_is_substituted_once_and_never_read_as_an_option() {
    let values = BTreeMap::from([
        ("output".to_owned(), "/out/speech.wav".to_owned()),
        ("threads".to_owned(), "4".to_owned()),
        (
            "text".to_owned(),
            "--vits-model=/evil {threads} {output} }{".to_owned(),
        ),
    ]);
    let template: Vec<String> = [
        "--output-filename={output}",
        "--num-threads={threads}",
        "--",
        "{text}",
    ]
    .map(String::from)
    .to_vec();
    assert_eq!(
        expand_arguments(&template, &values).unwrap(),
        vec![
            "--output-filename=/out/speech.wav",
            "--num-threads=4",
            "--",
            "--vits-model=/evil {threads} {output} }{",
        ]
    );
    // Without `--` the text could be taken for an option.
    let unguarded: Vec<String> = ["--output-filename={output}", "{text}"]
        .map(String::from)
        .to_vec();
    assert!(expand_arguments(&unguarded, &values).is_err());
    // Embedded in a larger argument the text stays that argument's value.
    assert_eq!(
        expand_arguments(&["--text={text}".to_owned()], &values).unwrap(),
        vec!["--text=--vits-model=/evil {threads} {output} }{"]
    );
    for broken in ["{output", "output}", "{}", "a}{b"] {
        assert!(
            expand_arguments(&[broken.to_owned()], &values).is_err(),
            "{broken}"
        );
    }
}

#[tokio::test]
async fn unconfigured_server_does_not_start_or_claim_readiness() {
    let service = ModelService::new(CapabilityConfig::default());
    assert!(service.start(CancellationToken::new()).await.is_err());
    assert!(
        service
            .status()
            .await
            .to_string()
            .contains("configuration_needed")
    );
}

#[tokio::test]
async fn inference_request_lock_is_exclusive_and_cancellable() {
    let root = tempfile::tempdir().unwrap();
    let artifacts = root.path().join("artifacts");
    std::fs::create_dir(&artifacts).unwrap();
    let config = CapabilityConfig {
        artifacts,
        ..Default::default()
    };
    let held = acquire_inference_lock(&config, &CancellationToken::new())
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(acquire_inference_lock(&config, &cancel).await.is_err());
    drop(held);
    assert!(
        acquire_inference_lock(&config, &CancellationToken::new())
            .await
            .is_ok()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn root_activity_prevents_idle_eviction_and_shutdown_without_a_generation() {
    let root = tempfile::tempdir().unwrap();
    let artifacts = root.path().join("artifacts");
    std::fs::create_dir(&artifacts).unwrap();
    let config = CapabilityConfig {
        workspace: root.path().to_path_buf(),
        artifacts,
        helper_idle_seconds: 5,
        ..Default::default()
    };
    let service = ModelService::new(config.clone());
    let process = ManagedProcess::spawn(&ProcessSpec {
        executable: PathBuf::from("/bin/sleep"),
        args: vec!["30".into()],
        cwd: root.path().to_path_buf(),
        env: BTreeMap::new(),
        timeout: Duration::from_secs(30),
        output_limit: 1024,
    })
    .unwrap();
    let registration = crate::managed_endpoints::Registration::create(
        root.path(),
        "http://127.0.0.1:12123/v1",
        process.pid().unwrap(),
    )
    .unwrap();
    *service.server.lock().await = Some(Server {
        process,
        endpoint: "http://127.0.0.1:12123".into(),
        used: Instant::now() - Duration::from_secs(20),
        _registration: registration,
    });
    let activity = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.path().join("local-inference-activity.lock"))
        .unwrap();
    fs2::FileExt::try_lock_shared(&activity).unwrap();
    assert_eq!(
        service.start(CancellationToken::new()).await.unwrap(),
        "http://127.0.0.1:12123"
    );
    service.server.lock().await.as_mut().unwrap().used = Instant::now() - Duration::from_secs(20);
    service.watch_idle();
    // No generation lock is held: this represents a root turn doing tools.
    tokio::time::sleep(Duration::from_millis(5250)).await;
    assert_eq!(service.status().await["llama_cpp"]["state"], "running");
    assert!(service.server.lock().await.as_ref().unwrap().used.elapsed() < Duration::from_secs(2));
    let error = service.shutdown().await.unwrap_err();
    assert!(error.to_string().contains("managed local chat is active"));
    assert_eq!(service.status().await["llama_cpp"]["state"], "running");
    drop(activity);
    service.shutdown().await.unwrap();
    assert!(service.server.lock().await.is_none());
    assert_eq!(
        serde_json::from_slice::<Value>(
            &std::fs::read(root.path().join("managed-endpoints.json")).unwrap()
        )
        .unwrap(),
        json!({})
    );
}
