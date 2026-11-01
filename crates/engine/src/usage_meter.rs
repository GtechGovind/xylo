//! Turn-owned accounting shared by main, helper and utility model requests.
use crate::model::{EventStream, ModelError, ModelProvider, ModelRequest, ModelUsage, StreamEvent};
use futures_util::StreamExt;
use parking_lot::Mutex;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_core::{Usage, pricing::UsageCall};
use xylo_services::Store;

type Receipts = Vec<(Uuid, String, Option<ModelUsage>)>;

#[derive(Clone, Default)]
pub(crate) struct Meter {
    receipts: Arc<Mutex<Receipts>>,
    persistence: Option<Persistence>,
}
#[derive(Clone)]
struct Persistence {
    store: Store,
    session: Uuid,
    turn: Uuid,
}

impl Meter {
    /// Restores earlier requests of this exact turn after a worker restart.
    pub(crate) fn for_turn(store: Store, session: Uuid, turn: Uuid) -> anyhow::Result<Self> {
        let receipts = store
            .turn_usage(session, turn)?
            .into_iter()
            .map(|(id, call)| match call {
                Some(call) => (
                    id,
                    call.model,
                    Some(ModelUsage {
                        input_tokens: call.input_tokens,
                        output_tokens: call.output_tokens,
                        cache_read_tokens: Some(call.cache_read_tokens),
                        cache_write_tokens: Some(call.cache_write_tokens),
                    }),
                ),
                None => (id, String::new(), None),
            })
            .collect();
        Ok(Self {
            receipts: Arc::new(Mutex::new(receipts)),
            persistence: Some(Persistence {
                store,
                session,
                turn,
            }),
        })
    }
    async fn record(
        &self,
        id: Uuid,
        model: String,
        usage: Option<ModelUsage>,
    ) -> Result<(), ModelError> {
        if let Some(persistence) = self.persistence.clone() {
            let call = usage.map(|usage| UsageCall {
                model: model.clone(),
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_tokens.unwrap_or_default(),
                cache_write_tokens: usage.cache_write_tokens.unwrap_or_default(),
            });
            tokio::task::spawn_blocking(move || {
                persistence
                    .store
                    .record_turn_usage(persistence.session, persistence.turn, id, call)
            })
            .await
            .map_err(|error| ModelError::Io(error.to_string()))?
            .map_err(|error| ModelError::Io(error.to_string()))?;
        }
        let mut receipts = self.receipts.lock();
        if let Some(receipt) = receipts.iter_mut().find(|(known, _, _)| *known == id) {
            *receipt = (id, model, usage);
        } else {
            receipts.push((id, model, usage));
        }
        Ok(())
    }
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
        for (_, model, reported) in self.receipts.lock().iter() {
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

struct Metered {
    inner: Arc<dyn ModelProvider>,
    model: String,
    meter: Meter,
}
impl ModelProvider for Metered {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> EventStream {
        let mut stream = self.inner.stream(request, cancel);
        let meter = self.meter.clone();
        let mut model = self.model.clone();
        let id = Uuid::new_v4();
        Box::pin(async_stream::stream! {
            let mut received=false;
            let mut usage=None;
            while let Some(event) = stream.next().await {
                if let Ok(event) = &event {
                    let mut changed=!received;
                    received=true;
                    match event {
                        StreamEvent::Metadata { model:actual, .. } => { changed |= model != *actual; model.clone_from(actual); },
                        StreamEvent::Usage(value) => {usage=Some(*value);changed=true;},
                        _ => {}
                    }
                    if changed && let Err(error)=meter.record(id,model.clone(),usage).await { yield Err(error); break; }
                }
                yield event;
            }
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
