//! A provider layer that records every request and its reply.
//!
//! Installed through [`xylo_engine::AgentEngine::with_provider_layer`], the
//! recorder sits beneath the engine's context guard, so it records the exact
//! requests a provider receives, after fitting and compaction. Events pass
//! through unchanged and in order; recording never alters a reply.

use crate::trace::{Exchange, ModelReply, Outcome, RecordedError, RecordedRequest, RequestKind};
use futures_util::StreamExt;
use parking_lot::Mutex;
use std::{collections::BTreeMap, sync::Arc};
use tokio_util::sync::CancellationToken;
use xylo_engine::{
    EventStream, FinishReason, ModelError, ModelProvider, ModelRequest, StreamEvent, ToolCall,
};

/// The shared, append-only exchange log of one trial.
pub type ExchangeLog = Arc<Mutex<Vec<Exchange>>>;

/// Wraps a provider and appends one [`Exchange`] per request to a log.
pub struct RecordingProvider {
    inner: Arc<dyn ModelProvider>,
    log: ExchangeLog,
}

impl RecordingProvider {
    /// Records `inner`'s traffic into `log`.
    pub fn new(inner: Arc<dyn ModelProvider>, log: ExchangeLog) -> Self {
        Self { inner, log }
    }
}

impl ModelProvider for RecordingProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> EventStream {
        let index = {
            let mut log = self.log.lock();
            log.push(Exchange {
                kind: RequestKind::of(&request),
                request: RecordedRequest::of(&request),
                outcome: Outcome::Abandoned,
            });
            log.len() - 1
        };
        let mut inner = self.inner.stream(request, cancel);
        let log = self.log.clone();
        Box::pin(async_stream::stream! {
            let mut reply = ReplyBuilder::default();
            let mut failed = false;
            while let Some(item) = inner.next().await {
                match &item {
                    Ok(event) => reply.push(event),
                    Err(error) => {
                        log.lock()[index].outcome = Outcome::Error(error.into());
                        failed = true;
                    }
                }
                yield item;
                if failed {
                    break;
                }
            }
            if !failed {
                log.lock()[index].outcome = reply.finish();
            }
        })
    }
}

/// Accumulates stream events into a [`ModelReply`] without validating it:
/// a truncated or malformed call is recorded exactly as the model sent it.
#[derive(Default)]
struct ReplyBuilder {
    text: String,
    reasoning: String,
    calls: BTreeMap<u32, ToolCall>,
    usage: Option<serde_json::Value>,
    finish: Option<FinishReason>,
}

impl ReplyBuilder {
    fn push(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::Text(text) => self.text.push_str(text),
            StreamEvent::Reasoning(text) => self.reasoning.push_str(text),
            StreamEvent::ToolCall {
                index,
                id,
                name,
                arguments,
            } => {
                let call = self
                    .calls
                    .entry(*index)
                    .or_insert_with(|| ToolCall::new("", "", ""));
                if let Some(id) = id {
                    call.id.clone_from(id);
                }
                if let Some(name) = name {
                    call.name.clone_from(name);
                }
                if let Some(arguments) = arguments {
                    call.arguments.push_str(arguments);
                }
            }
            StreamEvent::Usage(usage) => self.usage = serde_json::to_value(usage).ok(),
            StreamEvent::Finished(finish) => self.finish = Some(*finish),
            StreamEvent::Native(_) => {}
        }
    }

    fn finish(self) -> Outcome {
        let Some(finish) = self.finish else {
            return Outcome::Error(RecordedError::from(&ModelError::transient(
                "Provider stream ended before completion",
            )));
        };
        Outcome::Reply(ModelReply {
            text: self.text,
            reasoning: (!self.reasoning.is_empty()).then_some(self.reasoning),
            tool_calls: self.calls.into_values().collect(),
            finish,
            usage: self.usage,
        })
    }
}

#[cfg(test)]
mod tests;
