//! A provider and its explicitly configured alternative connection model.
use crate::model::{EventStream, ModelProvider, ModelRequest};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(crate) struct WithFallback {
    pub primary: Arc<dyn ModelProvider>,
    pub alternative: Arc<dyn ModelProvider>,
    pub name: String,
}
#[async_trait::async_trait]
impl ModelProvider for WithFallback {
    fn supports_search(&self) -> bool {
        self.primary.supports_search()
    }
    async fn search(
        &self,
        arguments: serde_json::Value,
        addresses: Vec<std::net::SocketAddr>,
        cancel: CancellationToken,
    ) -> Result<serde_json::Value, crate::model::ModelError> {
        self.primary.search(arguments, addresses, cancel).await
    }

    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> EventStream {
        self.primary.stream(request, cancel)
    }
    fn fallback(&self) -> Option<(String, Arc<dyn ModelProvider>)> {
        Some((self.name.clone(), self.alternative.clone()))
    }
}
