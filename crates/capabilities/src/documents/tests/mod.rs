use super::*;
use std::path::Path;
fn config(root: &Path) -> CapabilityConfig {
    CapabilityConfig {
        workspace: root.canonicalize().unwrap(),
        artifacts: root.canonicalize().unwrap(),
        ..Default::default()
    }
}
#[tokio::test]
async fn office_exports_round_trip_text_and_numbers_offline() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = config(temp.path());
    execute(
        &cfg,
        "document_export_xlsx",
        &json!({"name":"sample","rows":[["=1+1",42], ["Station", "A"]]}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let result = execute(
        &cfg,
        "document_read",
        &json!({"path":"sample.xlsx"}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(result.summary.contains("=1+1") && result.summary.contains("42"));
    execute(&cfg, "document_export_docx", &json!({"name":"report","paragraphs":["A & B <C> \"quoted\" it's"],"sources":[{"title":"Source","url":"https://example.com"}]}), CancellationToken::new()).await.unwrap();
    let result = execute(
        &cfg,
        "document_read",
        &json!({"path":"report.docx"}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let text: Value = serde_json::from_str(&result.summary).unwrap();
    assert!(
        text["text"]
            .as_str()
            .unwrap()
            .contains("A & B <C> \"quoted\" it's"),
        "{result:?}"
    );
    assert!(
        execute(
            &cfg,
            "document_export_docx",
            &json!({"name":"report","paragraphs":["overwrite"]}),
            CancellationToken::new()
        )
        .await
        .is_err()
    );
}

mod editing;
