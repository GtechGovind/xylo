//! Turn-owned accounting shared by main, helper and utility model requests.
use crate::model::{EventStream, ModelProvider, ModelRequest, ModelUsage, StreamEvent};
use futures_util::StreamExt;
use parking_lot::Mutex;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use xylo_core::{Usage, pricing::UsageCall};

type Receipts = Vec<(String, Option<ModelUsage>)>;

#[derive(Clone, Default)]
pub(crate) struct Meter(Arc<Mutex<Receipts>>);

impl Meter {
    pub(crate) fn wrap(
        &self,
        inner: Arc<dyn ModelProvider>,
        model: String,
    ) -> Arc<dyn ModelProvider> {
        Arc::new(Metered {
            inner,
            model,
            meter: self.clone(),
        })
    }

    /// Replaces request totals, preserving the main loop's context and timing.
    pub(crate) fn apply(&self, usage: &mut Usage) {
        usage.input_tokens = 0;
        usage.output_tokens = 0;
        usage.cache_read_tokens = None;
        usage.cache_write_tokens = None;
        usage.calls.clear();
        for (model, reported) in self.0.lock().iter() {
            let Some(reported) = reported else {
                // A response with no token receipt cannot be estimated as free.
                usage.calls.push(UsageCall::default());
                continue;
            };
            usage.input_tokens = usage.input_tokens.saturating_add(reported.input_tokens);
            usage.output_tokens = usage.output_tokens.saturating_add(reported.output_tokens);
            for (total, value) in [
                (&mut usage.cache_read_tokens, reported.cache_read_tokens),
                (&mut usage.cache_write_tokens, reported.cache_write_tokens),
            ] {
                if let Some(value) = value {
                    *total = Some(total.unwrap_or_default().saturating_add(value));
                }
            }
            usage.calls.push(UsageCall {
                model: model.clone(),
                input_tokens: reported.input_tokens,
                output_tokens: reported.output_tokens,
                cache_read_tokens: reported.cache_read_tokens.unwrap_or_default(),
                cache_write_tokens: reported.cache_write_tokens.unwrap_or_default(),
            });
        }
    }
}

/// Keeps the latest cumulative receipt even when a deadline drops the stream.
struct Receipt {
    meter: Meter,
    model: String,
    usage: Option<ModelUsage>,
    received: bool,
}
impl Drop for Receipt {
    fn drop(&mut self) {
        if self.received {
            self.meter.0.lock().push((self.model.clone(), self.usage));
        }
    }
}
struct Metered {
    inner: Arc<dyn ModelProvider>,
    model: String,
    meter: Meter,
}
impl ModelProvider for Metered {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> EventStream {
        let mut stream = self.inner.stream(request, cancel);
        let mut receipt = Receipt {
            meter: self.meter.clone(),
            model: self.model.clone(),
            usage: None,
            received: false,
        };
        Box::pin(async_stream::stream! {
            while let Some(event) = stream.next().await {
                if let Ok(event) = &event {
                    receipt.received = true;
                    match event {
                        StreamEvent::Metadata { model, .. } => receipt.model.clone_from(model),
                        StreamEvent::Usage(usage) => receipt.usage = Some(*usage),
                        _ => {}
                    }
                }
                yield event;
            }
            drop(receipt);
        })
    }
    fn fallback(&self) -> Option<(String, Arc<dyn ModelProvider>)> {
        self.inner
            .fallback()
            .map(|(model, inner)| (model.clone(), self.meter.wrap(inner, model)))
    }
}
#[cfg(test)]
mod tests;
