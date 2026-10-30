//! Explicit network/platform acceptance, separate from the offline suite.
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;
use xylo_helper_client::HelperClient;

#[tokio::test]
#[ignore = "Explicit downloads: set XYLO_ASSET_SMOKE_DATA and XYLO_ASSET_SMOKE_IDS"]
async fn selected_reviewed_assets_install_activate_and_native_pdf_executes() {
    let data =
        PathBuf::from(std::env::var_os("XYLO_ASSET_SMOKE_DATA").expect("Explicit smoke data root"));
    let ids = std::env::var("XYLO_ASSET_SMOKE_IDS").expect("Explicit comma-separated selection");
    assert!(data.is_absolute());
    std::fs::create_dir_all(&data).unwrap();
    let workspace = data.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let executable = std::env::var_os("XYLO_ASSET_SMOKE_HELPER")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_xylo-helper")));
    let client = HelperClient::spawn(executable, &data, &workspace)
        .await
        .unwrap();
    for id in ids.split(',') {
        let job = client
            .execute(
                "helper_asset_install",
                &json!({"id":id}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let job_id = job["job_id"].clone();
        let mut last = String::new();
        tokio::time::timeout(Duration::from_secs(1800), async {
            loop {
                let statuses = client
                    .execute(
                        "helper_asset_status",
                        &json!({"job_id":job_id}),
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap();
                let status = &statuses["jobs"][0];
                let phase = status["phase"].as_str().unwrap();
                if phase != last {
                    eprintln!("{id}: {phase}: {}", status["message"]);
                    last = phase.to_owned();
                }
                if matches!(phase, "installed" | "failed" | "cancelled" | "interrupted") {
                    assert_eq!(phase, "installed", "{status}");
                    break;
                }
                assert!(
                    client.pending_count() > 0,
                    "Background install must prevent helper idle eviction"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .unwrap();
        let activation = client
            .execute(
                "helper_asset_activate",
                &json!({"id":id}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(activation["state"], "activated");
        eprintln!("{id}: activated {}", activation);
    }
    let config = client
        .execute("helper_get_config", &json!({}), CancellationToken::new())
        .await
        .unwrap();
    if config["config"]["pdfium_library"].is_string() {
        let pdf = workspace.join("native.pdf");
        write_pdf(&pdf);
        let read = client
            .execute(
                "document_read",
                &json!({"path":pdf}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            read["summary"]
                .as_str()
                .unwrap()
                .contains("Native PDF verified"),
            "{read}"
        );
        let preview = client
            .execute(
                "document_pdf_preview",
                &json!({"path":pdf,"page":1}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(PathBuf::from(preview["artifacts"][0].as_str().unwrap()).is_file());
        eprintln!("native PDFium extraction and PNG preview passed");
    }
    let language_config = data.join("language-servers.json");
    if language_config.is_file() {
        let configuration: Value =
            serde_json::from_slice(&std::fs::read(language_config).unwrap()).unwrap();
        if let Some(server) = configuration["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|server| server["languages"] == json!(["rust"]))
        {
            let executable = PathBuf::from(server["executable"].as_str().unwrap());
            let result = xylo_capabilities::process::run(
                xylo_capabilities::process::ProcessSpec {
                    executable: executable.clone(),
                    args: vec!["--version".into()],
                    cwd: workspace.clone(),
                    env: Default::default(),
                    timeout: Duration::from_secs(5),
                    output_limit: 65536,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(result.success);
            assert!(result.stdout.contains("rust-analyzer"));
            verify_lsp_initialization(&executable).await;
            eprintln!("native rust-analyzer version and LSP initialize/shutdown passed");
        }
    }
    let catalog: Value = client
        .execute("helper_asset_catalog", &json!({}), CancellationToken::new())
        .await
        .unwrap();
    std::fs::write(
        data.join("asset-smoke-result.json"),
        serde_json::to_vec_pretty(&json!({"catalog":catalog,"configuration":config})).unwrap(),
    )
    .unwrap();
    client.shutdown().await.unwrap();
}

fn write_pdf(path: &std::path::Path) {
    let stream = "BT /F1 16 Tf 30 100 Td (Native PDF verified) Tj ET";
    let objects=["<< /Type /Catalog /Pages 2 0 R >>".to_owned(),"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_owned(),"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),format!("<< /Length {} >>\nstream\n{stream}\nendstream",stream.len())];
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, obj) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{obj}\nendobj\n", i + 1));
    }
    let start = pdf.len();
    pdf.push_str("xref\n0 6\n0000000000 65535 f \n");
    for offset in offsets {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n"
    ));
    std::fs::write(path, pdf).unwrap();
}

async fn verify_lsp_initialization(executable: &std::path::Path) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let mut child = tokio::process::Command::new(executable)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap());
    for (id, method, params) in [
        (
            1,
            "initialize",
            json!({"processId":null,"rootUri":null,"capabilities":{},"initializationOptions":{"cargo":{"buildScripts":{"enable":false}},"procMacro":{"enable":false},"checkOnSave":false}}),
        ),
        (2, "shutdown", Value::Null),
    ] {
        let body =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .unwrap();
        input
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await
            .unwrap();
        input.write_all(&body).await.unwrap();
        input.flush().await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut length = None;
                let mut total = 0;
                loop {
                    let mut line = String::new();
                    let count = output.read_line(&mut line).await.unwrap();
                    assert!(count > 0);
                    total += count;
                    assert!(total < 8192);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.strip_prefix("Content-Length: ") {
                        length = Some(value.trim().parse::<usize>().unwrap());
                    }
                }
                let length = length.unwrap();
                assert!(length <= 1024 * 1024);
                let mut body = vec![0; length];
                output.read_exact(&mut body).await.unwrap();
                let message: Value = serde_json::from_slice(&body).unwrap();
                if message["id"] == id {
                    assert!(message.get("error").is_none(), "{message}");
                    if id == 1 {
                        assert!(message["result"]["capabilities"].is_object());
                    }
                    break;
                }
            }
        })
        .await
        .unwrap();
        if id == 1 {
            let initialized = br#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#;
            input
                .write_all(format!("Content-Length: {}\r\n\r\n", initialized.len()).as_bytes())
                .await
                .unwrap();
            input.write_all(initialized).await.unwrap();
            input.flush().await.unwrap();
        }
    }
    let body = br#"{"jsonrpc":"2.0","method":"exit"}"#;
    input
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await
        .unwrap();
    input.write_all(body).await.unwrap();
    drop(input);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
