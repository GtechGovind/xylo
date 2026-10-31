use super::*;
#[tokio::test]
async fn audio_stop_bypasses_full_normal_execution_slots() {
    let shared = Arc::new(Shared::default());
    let (requests, _incoming) = mpsc::channel(QUEUE_CAPACITY);
    let (control, mut controls) = mpsc::channel(QUEUE_CAPACITY);
    let (_exited_tx, exited) = watch::channel(true);
    let client = HelperClient {
        inner: Arc::new(Inner {
            shared: shared.clone(),
            requests,
            control,
            slots: Semaphore::new(QUEUE_CAPACITY),
            control_slots: Semaphore::new(8),
            stop: CancellationToken::new(),
            exited,
            pid: 0,
        }),
    };
    let _all_slots = client
        .inner
        .slots
        .acquire_many(QUEUE_CAPACITY as u32)
        .await
        .unwrap();
    let peer = tokio::spawn(async move {
        let message = controls.recv().await.unwrap();
        assert!(
            matches!(message.command,Command::Execute{name,..} if name=="helper_audio_record_cancel")
        );
        shared
            .pending
            .lock()
            .unwrap()
            .remove(&message.id)
            .unwrap()
            .send(Ok(json!({"state":"cancelled"})))
            .unwrap();
    });
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        client.execute(
            "helper_audio_record_cancel",
            &json!({}),
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result["state"], "cancelled");
    peer.await.unwrap();
}

#[test]
fn helper_environment_is_an_allowlist() {
    for (name, _) in helper_environment() {
        assert!(
            HELPER_ENVIRONMENT.contains(&name.as_str()) || name.starts_with("LC_"),
            "Helper would inherit {name}"
        );
        assert!(!name.ends_with("_API_KEY") && !name.ends_with("_TOKEN"));
    }
}
