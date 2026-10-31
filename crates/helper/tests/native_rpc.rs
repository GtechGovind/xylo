//! These tests launch the actual compiled helper over private pipes. No account,
//! external site, model asset, Python, or Node runtime is involved.

use serde_json::json;
use tokio_util::sync::CancellationToken;
use xylo_helper_client::HelperClient;

#[tokio::test]
async fn priority_cancel_that_overtakes_execute_prevents_dispatch() {
    use xylo_helper_client::protocol::{self, Command, Outcome, Request, Response};
    let root = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_xylo-helper"))
        .arg("--data-dir")
        .arg(root.path().join("data"))
        .arg("--workspace")
        .arg(root.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap());
    let execute = Request::new(Command::Execute {
        name: "helper_version".into(),
        args: json!({}),
    });
    protocol::write_frame(
        &mut input,
        &Request::new(Command::Cancel {
            request_id: execute.id,
        }),
    )
    .await
    .unwrap();
    protocol::write_frame(&mut input, &execute).await.unwrap();
    let _: Response =
        serde_json::from_slice(&protocol::read_frame(&mut output).await.unwrap().unwrap()).unwrap();
    let response: Response =
        serde_json::from_slice(&protocol::read_frame(&mut output).await.unwrap().unwrap()).unwrap();
    assert_eq!(response.id, execute.id);
    assert!(matches!(response.outcome,Outcome::Failed{code,..} if code=="cancelled"));
    protocol::write_frame(&mut input, &Request::new(Command::Shutdown))
        .await
        .unwrap();
    drop(input);
    assert!(child.wait().await.unwrap().success());
}

#[tokio::test]
async fn persistent_helper_exports_documents_and_correlates_concurrent_requests() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let client = HelperClient::spawn(env!("CARGO_BIN_EXE_xylo-helper"), &data, &workspace)
        .await
        .unwrap();
    let version = client
        .execute("helper_version", &json!({}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(version["protocol_version"], 1);
    let status = client
        .execute("helper_status", &json!({}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        status["capabilities"]["models"]["llama_cpp"]["state"],
        "configuration_needed"
    );
    let definitions = client
        .execute("helper_tools", &json!({}), CancellationToken::new())
        .await
        .unwrap();
    assert!(
        definitions["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "document_export_docx")
    );

    let catalog = client
        .execute("helper_asset_catalog", &json!({}), CancellationToken::new())
        .await
        .unwrap();
    assert!(catalog["assets"].as_array().unwrap().len() >= 10);
    assert!(
        catalog["assets"]
            .as_array()
            .unwrap()
            .iter()
            .all(|asset| asset["installed"] == false)
    );
    assert_eq!(
        client
            .execute("helper_asset_status", &json!({}), CancellationToken::new())
            .await
            .unwrap()["jobs"],
        json!([])
    );
    assert!(
        client
            .execute(
                "helper_asset_install",
                &json!({"id":"unknown-arbitrary-url"}),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert!(
        client
            .execute(
                "helper_asset_activate",
                &json!({"id":"qwen3-0.6b-q8"}),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(client.pending_count(), 0);

    let audio = client
        .execute("helper_audio_status", &json!({}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(audio["recording"]["state"], "idle");
    assert_eq!(audio["device_status"], "not_probed");
    assert!(
        client
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
        client
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

    let mut requests = tokio::task::JoinSet::new();
    for index in 0..12 {
        let client = client.clone();
        requests.spawn(async move {
            let result = client
                .execute(
                    if index % 2 == 0 {
                        "helper_version"
                    } else {
                        "helper_status"
                    },
                    &json!({}),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            if index % 2 == 0 {
                assert_eq!(result["name"], "xylo-helper");
            } else {
                assert!(result.get("capabilities").is_some());
            }
        });
    }
    while let Some(result) = requests.join_next().await {
        result.unwrap();
    }

    let exported = client.execute("document_export_docx", &json!({"name":"native-helper-report","paragraphs":["Native helper verified & connected."],"sources":[{"title":"Local source","url":"https://example.com"}]}), CancellationToken::new()).await.unwrap();
    assert_eq!(exported["status"], "completed");
    let path = exported["artifacts"][0].as_str().unwrap();
    assert!(std::path::Path::new(path).is_file());
    let read = client
        .execute(
            "document_read",
            &json!({"path":path}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        read["summary"]
            .as_str()
            .unwrap()
            .contains("Native helper verified & connected.")
    );

    client.execute("helper_configure", &json!({"config":{"helper_idle_seconds":5,"workspace":"/not-authorized","artifacts":"/not-authorized"}}), CancellationToken::new()).await.unwrap();
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(data.join("capabilities.json")).unwrap()).unwrap();
    assert_eq!(
        stored["workspace"].as_str().unwrap(),
        workspace.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(
        stored["artifacts"].as_str().unwrap(),
        data.canonicalize()
            .unwrap()
            .join("artifacts")
            .to_str()
            .unwrap()
    );
    client
        .execute("helper_reload_config", &json!({}), CancellationToken::new())
        .await
        .unwrap();
    let second = HelperClient::spawn(env!("CARGO_BIN_EXE_xylo-helper"), &data, &workspace)
        .await
        .unwrap();
    let native_path = workspace.join("native-llama");
    let model_path = workspace.join("chosen-model.gguf");
    let first_args = json!({"config":{"llama":{"executable":native_path}}});
    let second_args = json!({"config":{"llama":{"model":model_path}}});
    let (first_patch, second_patch) = tokio::join!(
        client.execute("helper_patch_config", &first_args, CancellationToken::new()),
        second.execute(
            "helper_patch_config",
            &second_args,
            CancellationToken::new()
        )
    );
    first_patch.unwrap();
    second_patch.unwrap();
    let patched = client
        .execute("helper_get_config", &json!({}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(patched["config"]["llama"]["executable"], json!(native_path));
    assert_eq!(patched["config"]["llama"]["model"], json!(model_path));
    assert_eq!(patched["config"]["helper_idle_seconds"], 5);
    let activity = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data.join("local-inference-activity.lock"))
        .unwrap();
    fs2::FileExt::try_lock_shared(&activity).unwrap();
    let before = std::fs::read(data.join("capabilities.json")).unwrap();
    let blocked = client
        .execute(
            "helper_patch_config",
            &json!({"config":{"helper_idle_seconds":9}}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(blocked.to_string().contains("managed local chat is active"));
    assert_eq!(
        std::fs::read(data.join("capabilities.json")).unwrap(),
        before
    );
    assert!(
        client
            .execute("helper_reload_config", &json!({}), CancellationToken::new())
            .await
            .is_err()
    );
    drop(activity);
    assert!(
        client
            .execute(
                "helper_patch_config",
                &json!({"config":{"misspelled_setting":true}}),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    second.shutdown().await.unwrap();
    client
        .execute(
            "helper_release_resources",
            &json!({}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        client
            .execute("not_a_native_tool", &json!({}), CancellationToken::new())
            .await
            .is_err()
    );
    assert!(
        client
            .execute("helper_version", &json!({}), CancellationToken::new())
            .await
            .is_ok()
    );
    let oversized = "x".repeat(xylo_helper_client::protocol::MAX_FRAME_BYTES);
    assert!(
        client
            .execute(
                "helper_status",
                &json!({"too_large":oversized}),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    client.shutdown().await.unwrap();
    assert!(client.is_closed());
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_reaches_the_native_child_and_helper_remains_available() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("native-fixture");
    let model = root.path().join("fixture.gguf");
    std::fs::write(&model, b"fixture").unwrap();
    // A local fixture stands in for llama-server and never becomes ready.
    // exec makes its PID the long-lived command's PID for cleanup proof.
    std::fs::write(
        &executable,
        "#!/bin/sh\nprintf '%s' $$ > \"$0.pid\"\nexec sleep 30\n",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let client = HelperClient::spawn(
        env!("CARGO_BIN_EXE_xylo-helper"),
        root.path().join("data"),
        root.path(),
    )
    .await
    .unwrap();
    client
        .execute(
            "helper_patch_config",
            &json!({"config":{"llama":{"executable":executable,"model":model}}}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let running_client = client.clone();
    let running_cancel = cancel.clone();
    let running = tokio::spawn(async move {
        running_client
            .execute("local_inference_start", &json!({}), running_cancel)
            .await
    });
    let pid_file = root.path().join("native-fixture.pid");
    tokio::time::timeout(Duration::from_secs(3), async {
        while !pid_file.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid = std::fs::read_to_string(&pid_file).unwrap();
    cancel.cancel();
    assert!(running.await.unwrap().is_err());
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let status = tokio::process::Command::new("/bin/kill")
                .arg("-0")
                .arg(&pid)
                .stderr(std::process::Stdio::null())
                .status()
                .await
                .unwrap();
            if !status.success() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Cancelled native child must be terminated and reaped");
    assert!(
        client
            .execute("helper_status", &json!({}), CancellationToken::new())
            .await
            .is_ok()
    );
    client.shutdown().await.unwrap();
}
