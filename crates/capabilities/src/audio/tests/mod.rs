use super::*;
#[tokio::test]
async fn microphone_and_playback_require_user_action_before_touching_devices() {
    let root = tempfile::tempdir().unwrap();
    let service = AudioService::new(root.path(), root.path()).unwrap();
    assert_eq!(service.active_jobs(), 0);
    assert_eq!(service.status().unwrap()["device_status"], "not_probed");
    assert!(
        service
            .execute(
                "helper_audio_record_start",
                &json!({}),
                CancellationToken::new()
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("explicit user")
    );
    assert!(
        service
            .execute(
                "helper_audio_play",
                &json!({"path":"/invalid"}),
                CancellationToken::new()
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("explicit user")
    );
    assert_eq!(service.status().unwrap()["recording"]["state"], "idle");
    service.shutdown().await;
}
