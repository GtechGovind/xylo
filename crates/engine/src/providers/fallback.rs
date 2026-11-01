//! A provider and its explicitly configured alternative connection model.
use crate::model::{EventStream, ModelProvider, ModelRequest};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(crate) struct WithFallback {
    pub primary: Arc<dyn ModelProvider>,
    pub alternative: Arc<dyn ModelProvider>,
    pub name: String,
}
impl ModelProvider for WithFallback {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> EventStream {
        self.primary.stream(request, cancel)
    }
    fn fallback(&self) -> Option<(String, Arc<dyn ModelProvider>)> {
        Some((self.name.clone(), self.alternative.clone()))
    }
}
