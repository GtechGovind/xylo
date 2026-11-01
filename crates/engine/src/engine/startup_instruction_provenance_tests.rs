//! Startup expansion persists the same source authority as lazy instructions.
use super::*;
use xylo_core::MemoryOrigin;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_import_exposure_survives_restart_and_fork() {
    let (_directory, engine, request) = tests::fixture();
    let guide = tempfile::Builder::new()
        .prefix("xylo-startup-source-")
        .tempdir_in(std::env::var_os("HOME").unwrap())
        .unwrap();
    let source = guide.path().join("guide.md");
    std::fs::write(&source, "External startup convention").unwrap();
    std::fs::write(
        request.workspace.join("AGENTS.md"),
        format!("@{}\n", source.display()),
    )
    .unwrap();
    let (events, _receiver) = mpsc::channel(256);
    let broker = engine
        .broker(&request, events, CancellationToken::new())
        .unwrap();
    let prompt = AgentEngine::system_prompt(&request, &broker, PermissionMode::ReadOnly).unwrap();
    assert!(prompt.contains("External startup convention"));
    assert_eq!(
        engine
            .store
            .session_instruction_origin(request.session_id)
            .unwrap(),
        MemoryOrigin::ToolOutput
    );
    std::fs::remove_file(source).unwrap();
    let reopened = Store::open(&engine.paths.database).unwrap();
    let fork = uuid::Uuid::new_v4();
    reopened
        .copy_instruction_provenance(request.session_id, fork)
        .unwrap();
    assert_eq!(
        reopened.session_instruction_origin(fork).unwrap(),
        MemoryOrigin::ToolOutput
    );
    assert_eq!(
        reopened
            .session_instruction_origin(uuid::Uuid::new_v4())
            .unwrap(),
        MemoryOrigin::AgentInferred
    );
    broker.shutdown().await.unwrap();
}
