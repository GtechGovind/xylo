use super::*;
use xylo_services::hooks::HookOutput;
struct HookTools {
    stops: std::sync::atomic::AtomicUsize,
}
impl ToolExecutor for HookTools {
    fn schemas(&self) -> Vec<ToolSchema> {
        Vec::new()
    }
    fn deferred_count(&self) -> usize {
        0
    }
    fn access(&self, _: &str, _: &Value) -> Option<Access> {
        None
    }
    fn execute(&self, _: &str, _: &str, _: Value) -> anyhow::Result<String> {
        unreachable!()
    }
    fn lifecycle(&self, event: &str, _: Value) -> anyhow::Result<HookOutput> {
        Ok(match event {
            "UserPromptSubmit" => HookOutput {
                context: vec!["The user prompt hook supplied this context".into()],
                ..Default::default()
            },
            "Stop" => {
                self.stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                HookOutput {
                    block_reason: Some("Finish the requested checks".into()),
                    ..Default::default()
                }
            }
            _ => HookOutput::default(),
        })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_hook_context_reaches_provider_and_stop_continuations_cap_at_three() {
    let mut h = harness(
        (0..4)
            .map(|_| vec![text("answer"), finish(FinishReason::Stop)])
            .collect(),
    );
    let tools = Arc::new(HookTools {
        stops: Default::default(),
    });
    h.agent.tools = tools.clone();
    let outcome = h.agent.run(vec![Message::user("request")]).await.unwrap();
    assert_eq!(outcome.text, "answer");
    let requests = h.provider.requests.lock();
    assert_eq!(requests.len(), 4);
    assert!(requests[0].messages.iter().any(|m| {
        m.content
            .contains("The user prompt hook supplied this context")
    }));
    assert_eq!(tools.stops.load(std::sync::atomic::Ordering::SeqCst), 4);
}
