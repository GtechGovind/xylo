//! A child can encounter new startup imports after the parent prompt was built.
use super::*;
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_startup_records_imports_before_calling_the_model() {
    let (_directory, engine, mut request) = fixture();
    let engine = AgentEngine::new(
        engine.store.clone(),
        engine.paths.clone(),
        ToolSession::for_tests(PermissionMode::ApproveForMe),
    );
    let guide = tempfile::Builder::new()
        .prefix("xylo-child-source-")
        .tempdir_in(std::env::var_os("HOME").unwrap())
        .unwrap();
    let source = guide.path().join("guide.md");
    std::fs::write(&source, "Child-only external instruction").unwrap();
    let instructions = request.workspace.join("AGENTS.md");
    let store = engine.store.clone();
    let session_id = request.session_id;
    let (endpoint, seen) = serve(move |body| {
        if is_title(body) {
            return message("Fixture");
        }
        if body["instructions"]
            .as_str()
            .is_some_and(|text| text.contains("Child-only external instruction"))
        {
            assert_eq!(
                store.session_instruction_origin(session_id).unwrap(),
                xylo_core::MemoryOrigin::ToolOutput
            );
            return message("Child complete");
        }
        if !answered(body) {
            assert_eq!(
                store.session_instruction_origin(session_id).unwrap(),
                xylo_core::MemoryOrigin::AgentInferred
            );
            std::fs::write(&instructions, format!("@{}", source.display())).unwrap();
            function_call(
                "delegate",
                "agent_delegate",
                json!({"task":"Inspect the project convention"}),
            )
        } else {
            message("Parent complete")
        }
    })
    .await;
    request.provider = responses(endpoint);
    let (events, _receiver) = mpsc::channel(1024);
    let cancel = CancellationToken::new();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        engine.run(request.clone(), events, cancel.clone()),
    )
    .await;
    if result.is_err() {
        cancel.cancel();
    }
    result.unwrap().unwrap();
    assert!(seen.lock().iter().any(|body| {
        body["instructions"]
            .as_str()
            .is_some_and(|text| text.contains("Child-only external instruction"))
    }));
    assert_eq!(
        engine
            .store
            .session_instruction_origin(request.session_id)
            .unwrap(),
        xylo_core::MemoryOrigin::ToolOutput
    );
}
