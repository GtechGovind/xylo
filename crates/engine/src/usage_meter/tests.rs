use super::*;
use crate::model::{FinishReason, Message, ModelUsage, Reasoning, StreamEvent};
use futures_util::StreamExt;

struct Fixture {
    model: &'static str,
    fallback: bool,
}
impl ModelProvider for Fixture {
    fn stream(&self, _: ModelRequest, _: CancellationToken) -> EventStream {
        Box::pin(futures_util::stream::iter(vec![
            Ok(StreamEvent::Metadata {
                model: self.model.into(),
                rate_limits: None,
            }),
            Ok(StreamEvent::Usage(ModelUsage {
                input_tokens: 10,
                output_tokens: 1,
                cache_read_tokens: Some(4),
                cache_write_tokens: None,
            })),
            Ok(StreamEvent::Usage(ModelUsage {
                input_tokens: 10,
                output_tokens: 3,
                cache_read_tokens: Some(4),
                cache_write_tokens: Some(2),
            })),
            Ok(StreamEvent::Text("Answer".into())),
            Ok(StreamEvent::Finished(FinishReason::Stop)),
        ]))
    }
    fn fallback(&self) -> Option<(String, Arc<dyn ModelProvider>)> {
        self.fallback.then(|| {
            (
                "alternative".into(),
                Arc::new(Self {
                    model: "alternative",
                    fallback: false,
                }) as Arc<dyn ModelProvider>,
            )
        })
    }
}
fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![Message::user("fixture")],
        tools: vec![],
        max_tokens: 20,
        reasoning: Reasoning::Lowest,
    }
}

#[tokio::test]
async fn utility_and_fallback_requests_share_counts_without_counting_usage_updates_twice() {
    let meter = Meter::default();
    let provider = meter.wrap(
        Arc::new(Fixture {
            model: "primary",
            fallback: true,
        }),
        "configured".into(),
    );
    provider
        .complete(request(), CancellationToken::new())
        .await
        .unwrap();
    let (_, alternative) = provider.fallback().unwrap();
    alternative
        .complete(request(), CancellationToken::new())
        .await
        .unwrap();
    let mut usage = xylo_core::Usage {
        context_tokens: 10,
        elapsed_ms: 4,
        ..Default::default()
    };
    meter.apply(&mut usage);
    assert_eq!((usage.input_tokens, usage.output_tokens), (20, 6));
    assert_eq!(
        (usage.cache_read_tokens, usage.cache_write_tokens),
        (Some(8), Some(4))
    );
    assert_eq!((usage.context_tokens, usage.elapsed_ms), (10, 4));
    assert_eq!(
        usage
            .calls
            .iter()
            .map(|call| call.model.as_str())
            .collect::<Vec<_>>(),
        vec!["primary", "alternative"]
    );
}

#[tokio::test]
async fn dropping_a_partial_stream_retains_the_last_reported_usage() {
    let meter = Meter::default();
    let provider = meter.wrap(
        Arc::new(Fixture {
            model: "primary",
            fallback: false,
        }),
        "configured".into(),
    );
    let mut stream = provider.stream(request(), CancellationToken::new());
    stream.next().await.unwrap().unwrap();
    stream.next().await.unwrap().unwrap();
    drop(stream);
    let mut usage = xylo_core::Usage::default();
    meter.apply(&mut usage);
    assert_eq!((usage.input_tokens, usage.output_tokens), (10, 1));
}

#[tokio::test]
async fn a_reopened_turn_meter_includes_prior_requests_and_new_fallback_requests() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("db");
    let store = Store::open(&path).unwrap();
    let session = Uuid::new_v4();
    let turn = Uuid::new_v4();
    store
        .upsert_conversation(&xylo_core::Conversation {
            id: session,
            title: "Usage".into(),
            workspace: root.path().into(),
            updated_at: 0,
            title_source: Default::default(),
            pinned: false,
            archived: false,
        })
        .unwrap();
    let meter = Meter::for_turn(store.clone(), session, turn).unwrap();
    let provider = meter.wrap(
        Arc::new(Fixture {
            model: "primary",
            fallback: false,
        }),
        "primary".into(),
    );
    provider
        .complete(request(), CancellationToken::new())
        .await
        .unwrap();
    drop(provider);
    drop(meter);
    drop(store);
    let meter = Meter::for_turn(Store::open_existing(&path).unwrap(), session, turn).unwrap();
    meter
        .wrap(
            Arc::new(Fixture {
                model: "alternative",
                fallback: false,
            }),
            "alternative".into(),
        )
        .complete(request(), CancellationToken::new())
        .await
        .unwrap();
    let mut usage = xylo_core::Usage::default();
    meter.apply(&mut usage);
    assert_eq!((usage.input_tokens, usage.output_tokens), (20, 6));
    assert_eq!(
        usage
            .calls
            .iter()
            .map(|call| call.model.as_str())
            .collect::<Vec<_>>(),
        vec!["primary", "alternative"]
    );
}

struct SearchFixture(serde_json::Value);
#[async_trait::async_trait]
impl ModelProvider for SearchFixture {
    fn stream(&self, _: ModelRequest, _: CancellationToken) -> EventStream {
        Box::pin(futures_util::stream::empty())
    }
    fn supports_search(&self) -> bool {
        true
    }
    async fn search(
        &self,
        _: serde_json::Value,
        _: Vec<std::net::SocketAddr>,
        _: CancellationToken,
    ) -> Result<serde_json::Value, ModelError> {
        Ok(self.0.clone())
    }
}
#[tokio::test]
async fn native_search_survives_metering_and_records_actual_model_cache_and_reasoning() {
    use serde_json::json;
    for (raw, input, output, read, write) in [
        (
            json!({"input_tokens":12,"output_tokens":7,"input_tokens_details":{"cached_tokens":3}}),
            12,
            7,
            3,
            0,
        ),
        (
            json!({"input_tokens":12,"output_tokens":7,"cache_read_input_tokens":3,"cache_creation_input_tokens":2}),
            17,
            7,
            3,
            2,
        ),
        (
            json!({"promptTokenCount":12,"candidatesTokenCount":7,"thoughtsTokenCount":2,"cachedContentTokenCount":3,"toolUsePromptTokenCount":1}),
            13,
            9,
            3,
            0,
        ),
    ] {
        let meter = Meter::default();
        let provider = meter.wrap(
            Arc::new(SearchFixture(
                json!({"usage":raw,"model":"actual-search-model","requested_model":"configured"}),
            )),
            "configured".into(),
        );
        assert!(provider.supports_search());
        provider
            .search(json!({"query":"fixture"}), vec![], CancellationToken::new())
            .await
            .unwrap();
        let mut usage = Usage::default();
        meter.apply(&mut usage);
        assert_eq!((usage.input_tokens, usage.output_tokens), (input, output));
        assert_eq!(usage.calls.len(), 1);
        let call = &usage.calls[0];
        assert_eq!(call.model, "actual-search-model");
        assert_eq!(
            (call.cache_read_tokens, call.cache_write_tokens),
            (read, write)
        );
    }
}
