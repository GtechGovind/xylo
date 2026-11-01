//! The CLI engine options reach the actual provider request and round budget.
use super::*;
use crate::{FinishReason, RunOptions, StreamEvent};
use std::sync::atomic::{AtomicUsize, Ordering};

struct OptionsProbe {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    calls: AtomicUsize,
}
impl ModelProvider for OptionsProbe {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> crate::EventStream {
        self.requests.lock().push(request.clone());
        let step = self.calls.fetch_add(1, Ordering::SeqCst);
        if request.reasoning == crate::Reasoning::Lowest {
            return Box::pin(futures_util::stream::iter([
                Ok(StreamEvent::Text("Title".into())),
                Ok(StreamEvent::Finished(FinishReason::Stop)),
            ]));
        }
        if step == 0 && !request.tools.is_empty() {
            Box::pin(futures_util::stream::iter([
                Ok(StreamEvent::ToolCall {
                    index: 0,
                    id: Some("list".into()),
                    name: Some("files_list".into()),
                    arguments: Some("{\"path\":\".\"}".into()),
                }),
                Ok(StreamEvent::Finished(FinishReason::ToolCalls)),
            ]))
        } else {
            Box::pin(futures_util::stream::iter([
                Ok(StreamEvent::Text("Finished".into())),
                Ok(StreamEvent::Finished(FinishReason::Stop)),
            ]))
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_system_append_and_tool_restriction_reach_provider() {
    let (_root, engine, mut request) = fixture();
    request.provider = ProviderConfig {
        model: "fixture".into(),
        endpoint: "https://scripted.invalid/v1".into(),
        context_tokens: 32768,
        ..Default::default()
    };
    let requests = Arc::new(Mutex::new(Vec::new()));
    let probe: Arc<dyn ModelProvider> = Arc::new(OptionsProbe {
        requests: requests.clone(),
        calls: AtomicUsize::new(0),
    });
    let engine = engine
        .with_provider_credential(WorkerCredential(Some("offline".into())))
        .with_provider_layer(Arc::new(move |_| probe.clone()))
        .with_run_options(RunOptions {
            append_system_prompt: "EXPLICIT_SYSTEM_APPEND".into(),
            allowed_tools: Some(vec!["files_list".into()]),
            max_turns: Some(2),
            ..Default::default()
        });
    engine.store.register_user_turn(&request).unwrap();
    let (events, mut receiver) = mpsc::channel(256);
    let run =
        tokio::spawn(async move { engine.run(request, events, CancellationToken::new()).await });
    while receiver.recv().await.is_some() {}
    run.await.unwrap().unwrap();
    let seen = requests.lock();
    let root = seen
        .iter()
        .filter(|request| request.reasoning != crate::Reasoning::Lowest)
        .collect::<Vec<_>>();
    assert_eq!(
        root.len(),
        2,
        "the round budget must not silently make another provider call"
    );
    assert!(
        root[0]
            .messages
            .iter()
            .any(|message| message.role == Role::System
                && message.content.contains("EXPLICIT_SYSTEM_APPEND"))
    );
    assert!(!root[0].tools.is_empty());
    assert!(root[0].tools.iter().all(|tool| tool.name == "files_list"));
}
