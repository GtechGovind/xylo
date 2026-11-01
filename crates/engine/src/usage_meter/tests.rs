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
