//! Explicit asset test: synthesis and transcription without opening an audio
//! device. This test never records, plays speakers, or requests OS permission.
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;
use workbench_helper_client::HelperClient;

#[tokio::test]
#[ignore = "Explicit model downloads: set WORKBENCH_SPEECH_SMOKE_DATA to an isolated absolute test data directory"]
async fn selected_native_voice_synthesizes_and_whisper_transcribes_without_devices() {
    let data = PathBuf::from(
        std::env::var_os("WORKBENCH_SPEECH_SMOKE_DATA")
            .expect("Explicit isolated speech test data root"),
    );
    assert!(data.is_absolute());
    std::fs::create_dir_all(&data).unwrap();
    let workspace = data.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let client = HelperClient::spawn(env!("CARGO_BIN_EXE_workbench-helper"), &data, &workspace)
        .await
        .unwrap();
    for id in ["vits-ljs-int8", "whisper-tiny-en"] {
        let job = client
            .execute(
                "helper_asset_install",
                &json!({"id":id}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let mut last = String::new();
        tokio::time::timeout(Duration::from_secs(600), async {
            loop {
                let statuses = client
                    .execute(
                        "helper_asset_status",
                        &json!({"job_id":job["job_id"]}),
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap();
                let status = &statuses["jobs"][0];
                let phase = status["phase"].as_str().unwrap();
                if phase != last {
                    eprintln!("{id}: {phase} {}", status["message"]);
                    last = phase.into();
                }
                if matches!(phase, "installed" | "failed" | "cancelled" | "interrupted") {
                    assert_eq!(phase, "installed", "{status}");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .unwrap();
        client
            .execute(
                "helper_asset_activate",
                &json!({"id":id}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }
    let generated = client
        .execute(
            "speech_synthesize",
            &json!({"engine":"sherpa","text":"Hello, this is a native speech test."}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let generated: Value = serde_json::from_str(generated["summary"].as_str().unwrap()).unwrap();
    let audio = PathBuf::from(generated["path"].as_str().unwrap());
    assert!(audio.is_file());
    eprintln!("Native synthesis: {generated}");
    let transcribed = client
        .execute(
            "speech_transcribe",
            &json!({"engine":"whisper","path":audio}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let transcript: Value = serde_json::from_str(transcribed["summary"].as_str().unwrap()).unwrap();
    let text = transcript["text"].as_str().unwrap().to_lowercase();
    assert!(text.contains("native speech test"), "{transcript}");
    eprintln!("Native transcription: {text}");
    let status = client
        .execute("helper_audio_status", &json!({}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(status["recording"]["state"], "idle");
    assert_eq!(status["playing"], false);
    assert_eq!(status["device_status"], "not_probed");
    assert!(
        std::fs::read_dir(data.join("artifacts"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("speech-input-"))
    );
    std::fs::write(
        data.join("speech-smoke-result.json"),
        serde_json::to_vec_pretty(
            &json!({"synthesis":generated,"transcription":transcript,"audio_status":status}),
        )
        .unwrap(),
    )
    .unwrap();
    client.shutdown().await.unwrap();
}
