use super::*;
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edits_require_a_conversation_read_and_report_type_errors() {
    let (dir, broker, _events) = fixture(PermissionMode::ApproveForMe);
    let path = dir.path().join("a.py");
    std::fs::write(&path, "valid\n").unwrap();
    let edit = json!({"path":"a.py","edits":[{"old_string":"valid","new_string":"BAD_TYPE"}]});
    assert!(
        run(&broker, "blind", "file_edit", edit.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("file_read")
    );
    run(&broker, "read", "file_read", json!({"path":"a.py"}))
        .await
        .unwrap();
    std::fs::write(&path, "external\n").unwrap();
    assert!(
        run(
            &broker,
            "stale",
            "file_write",
            json!({"path":"a.py","content":"valid\n"})
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("read it again")
    );
    std::fs::write(&path, "valid\n").unwrap();
    run(&broker, "reread", "file_read", json!({"path":"a.py"}))
        .await
        .unwrap();
    std::fs::write(dir.path().join("profile/language-servers.json"),json!({"servers":[{"languages":["python"],"executable":"/usr/bin/python3","args":[concat!(env!("CARGO_MANIFEST_DIR"),"/../runtime/src/lsp/agent/test_server.py")]}]}).to_string()).unwrap();
    let result = run(&broker, "edit", "file_edit", edit).await.unwrap();
    let value: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        value["summary"]["diagnostics"][0]["message"],
        "incompatible type"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "BAD_TYPE\n");
    broker.session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_images_return_managed_tool_attachments() {
    let (dir, broker, _events) = fixture(PermissionMode::ApproveForMe);
    let path = dir.path().join("a.png");
    image::RgbaImage::new(2, 2).save(&path).unwrap();
    let result = run(&broker, "image", "file_read", json!({"path":"a.png"}))
        .await
        .unwrap();
    let message = crate::model::Message::tool_result("image", result);
    assert_eq!(message.attachments.len(), 1);
    assert_eq!(message.attachments[0].media_type, "image/png");
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(std::fs::read(&path).unwrap());
    std::fs::write(dir.path().join("a.ipynb"),json!({"nbformat":4,"cells":[{"id":"a","cell_type":"code","source":"plot()","outputs":[{"output_type":"display_data","data":{"image/png":[&encoded[..encoded.len()/2],&encoded[encoded.len()/2..]]}}]}]}).to_string()).unwrap();
    let result = run(
        &broker,
        "notebook-image",
        "file_read",
        json!({"path":"a.ipynb"}),
    )
    .await
    .unwrap();
    let notebook_message = crate::model::Message::tool_result("notebook-image", result);
    assert_eq!(notebook_message.attachments.len(), 1);
    assert!(!notebook_message.content.contains(&encoded));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an installed native clangd; run during native qualification"]
async fn native_type_error_is_in_the_successful_edit_result() {
    let (dir, broker, _events) = fixture(PermissionMode::ApproveForMe);
    let path = dir.path().join("a.cpp");
    std::fs::write(&path, "int main() { return 1; }\n").unwrap();
    run(&broker, "read", "file_read", json!({"path":"a.cpp"}))
        .await
        .unwrap();
    let result = run(
        &broker,
        "edit",
        "file_edit",
        json!({"path":"a.cpp","edits":[{"old_string":"return 1","new_string":"return \"wrong\""}]}),
    )
    .await
    .unwrap();
    let result: Value = serde_json::from_str(&result).unwrap();
    assert!(
        result["summary"]["diagnostics"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["severity"] == 1)),
        "{result}"
    );
    broker.session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pdf_reads_report_the_existing_pdfium_configuration_requirement() {
    let (dir, broker, _events) = fixture(PermissionMode::ApproveForMe);
    std::fs::write(dir.path().join("book.pdf"), b"%PDF-1.4\n").unwrap();
    let error = run(
        &broker,
        "pdf",
        "file_read",
        json!({"path":"book.pdf","pages":"2-4"}),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("PDFium"), "{error}");
    assert!(
        run(
            &broker,
            "pdf-cap",
            "file_read",
            json!({"path":"book.pdf","pages":"1-21"})
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("20")
    );
}
