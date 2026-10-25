//! Per-request model budget and cross-process local generation admission. The file
//! lease is held only during generation/stream consumption, never across tool use.
use async_trait::async_trait;
use futures_util::{StreamExt, stream::BoxStream};
use hermes_core::{AgentError, LlmProvider, LlmResponse, Message, StreamChunk, ToolSchema};
use std::{
    fs::{File, OpenOptions},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use workbench_core::{ApiFamily, ProviderConfig};
use workbench_runtime::resources::{ResourceManager, TaskDemand};

#[derive(Clone)]
pub(crate) struct GuardedProvider {
    inner: Arc<dyn LlmProvider>,
    context_tokens: u32,
    output_tokens: u32,
    model: String,
    known_tokenizer: bool,
    local_lock: Option<PathBuf>,
    timeout: Duration,
    cancel: CancellationToken,
    resources: ResourceManager,
}
impl GuardedProvider {
    pub(crate) fn new(
        inner: Arc<dyn LlmProvider>,
        config: &ProviderConfig,
        lock: PathBuf,
        cancel: CancellationToken,
        managed_single_slot: bool,
    ) -> Self {
        let local = url::Url::parse(&config.endpoint)
            .ok()
            .is_some_and(|url| matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")));
        Self {
            inner,
            context_tokens: config.context_tokens,
            output_tokens: config.output_tokens,
            model: config.model.clone(),
            known_tokenizer: matches!(
                config.family,
                ApiFamily::OpenAiChat | ApiFamily::OpenAiResponses | ApiFamily::Azure
            ),
            local_lock: (local && managed_single_slot).then_some(lock),
            timeout: Duration::from_secs(config.timeout_seconds),
            cancel,
            resources: ResourceManager::new(),
        }
    }
    fn preflight(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
    ) -> Result<(), AgentError> {
        // Image bytes are hydrated after this guard; reserve conservative visual
        // patches separately so base64 is never treated as ordinary text tokens.
        let image_tokens = messages
            .iter()
            .flat_map(|message| message.anthropic_content_blocks.as_deref().unwrap_or(&[]))
            .filter(|part| part["type"] == crate::attachments::IMAGE_MARKER)
            .map(|part| {
                let width = part["item"]["width"].as_u64().unwrap_or(4096);
                let height = part["item"]["height"].as_u64().unwrap_or(4096);
                let longest = width.max(height).max(1);
                let scale = 4096_f64.min(longest as f64) / longest as f64;
                let width = (width as f64 * scale).ceil() as usize;
                let height = (height as f64 * scale).ceil() as usize;
                width
                    .div_ceil(32)
                    .saturating_mul(height.div_ceil(32))
                    .saturating_add(1024)
            })
            .fold(0usize, usize::saturating_add);
        let messages =
            serde_json::to_string(messages).map_err(|e| AgentError::Config(e.to_string()))?;
        let schemas =
            serde_json::to_string(tools).map_err(|e| AgentError::Config(e.to_string()))?;
        // Built-in BPE assets initialize lazily from embedded data. Unknown/custom
        // models use one token per UTF-8 byte, a conservative upper bound for
        // byte-level tokenizers, plus explicit wire/framing reserve. No char/4 guess.
        let bpe = if self.known_tokenizer {
            tiktoken_rs::bpe_for_model(&self.model).ok()
        } else {
            None
        };
        let estimate = |text: &str| bpe.map_or(text.len(), |bpe| bpe.encode_ordinary(text).len());
        let input = estimate(&messages)
            .saturating_add(image_tokens)
            .saturating_add(estimate(&schemas))
            .saturating_add(256 + tools.len() * 64);
        let output = max_tokens
            .unwrap_or(self.output_tokens)
            .min(self.output_tokens) as usize;
        if input.saturating_add(output) > self.context_tokens as usize {
            return Err(AgentError::Config(format!(
                "Context budget exceeded: input estimate {input} plus output reserve {output} exceeds configured {} tokens. Shorten retained context or increase the configured model context. Custom models use conservative UTF-8 byte accounting.",
                self.context_tokens
            )));
        }
        Ok(())
    }
    async fn lease(&self) -> Result<Option<File>, AgentError> {
        let Some(path) = &self.local_lock else {
            return Ok(None);
        };
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| AgentError::Io(format!("Open local inference lock: {e}")))?;
        let start = tokio::time::Instant::now();
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(Some(file)),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {}
                Err(error) => return Err(AgentError::Io(format!("Lock local inference: {error}"))),
            }
            tokio::select! {_ = self.cancel.cancelled()=>return Err(interrupted()),_ = tokio::time::sleep(Duration::from_millis(20))=>{}}
            if start.elapsed() >= self.timeout {
                return Err(AgentError::Timeout(
                    "Waiting for the local model to finish another generation".into(),
                ));
            }
        }
    }
}
fn interrupted() -> AgentError {
    AgentError::Interrupted {
        message: Some("Task cancelled during model generation".into()),
    }
}
#[async_trait]
impl LlmProvider for GuardedProvider {
    async fn chat_completion(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra_body: Option<&serde_json::Value>,
    ) -> Result<LlmResponse, AgentError> {
        let max_tokens = Some(
            max_tokens
                .unwrap_or(self.output_tokens)
                .min(self.output_tokens),
        );
        self.preflight(messages, tools, max_tokens)?;
        let _resources = self
            .resources
            .acquire(
                TaskDemand::interactive(self.context_tokens),
                self.cancel.clone(),
            )
            .await
            .map_err(|e| AgentError::Config(e.to_string()))?;
        let _lease = self.lease().await?;
        tokio::select! {_ = self.cancel.cancelled()=>Err(interrupted()),result=tokio::time::timeout(self.timeout,self.inner.chat_completion(messages,tools,max_tokens,temperature,model,extra_body))=>result.map_err(|_|AgentError::Timeout("Model generation deadline exceeded".into()))?}
    }
    fn chat_completion_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra_body: Option<&serde_json::Value>,
    ) -> BoxStream<'static, Result<StreamChunk, AgentError>> {
        let max_tokens = Some(
            max_tokens
                .unwrap_or(self.output_tokens)
                .min(self.output_tokens),
        );
        let this = self.clone();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        let model = model.map(str::to_owned);
        let extra = extra_body.cloned();
        Box::pin(async_stream::try_stream! {
            this.preflight(&messages,&tools,max_tokens)?;
            let _resources=this.resources.acquire(TaskDemand::interactive(this.context_tokens),this.cancel.clone()).await.map_err(|e|AgentError::Config(e.to_string()))?;
            let _lease=this.lease().await?;
            let mut stream=this.inner.chat_completion_stream(&messages,&tools,max_tokens,temperature,model.as_deref(),extra.as_ref());
            let deadline=tokio::time::Instant::now()+this.timeout;
            loop{let next=tokio::select!{_ = this.cancel.cancelled()=>Err(interrupted()),_ = tokio::time::sleep_until(deadline)=>Err(AgentError::Timeout("Streaming model deadline exceeded".into())),next=stream.next()=>Ok(next)}?;
                let Some(chunk)=next else{break};yield chunk?;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::{Message, StreamDelta};
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Fixture {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl LlmProvider for Fixture {
        async fn chat_completion(
            &self,
            _: &[Message],
            _: &[ToolSchema],
            _: Option<u32>,
            _: Option<f64>,
            _: Option<&str>,
            _: Option<&serde_json::Value>,
        ) -> Result<LlmResponse, AgentError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(LlmResponse {
                message: Message::assistant("fixture"),
                usage: None,
                model: "fixture".into(),
                finish_reason: Some("stop".into()),
            })
        }
        fn chat_completion_stream(
            &self,
            _: &[Message],
            _: &[ToolSchema],
            _: Option<u32>,
            _: Option<f64>,
            _: Option<&str>,
            _: Option<&serde_json::Value>,
        ) -> BoxStream<'static, Result<StreamChunk, AgentError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(
                async_stream::stream! {yield Ok(StreamChunk{delta:Some(StreamDelta{content:Some("fixture".into()),tool_calls:None,extra:None}),finish_reason:None,usage:None});std::future::pending::<()>().await;},
            )
        }
    }
    fn guard(directory: &std::path::Path) -> (GuardedProvider, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let config = ProviderConfig {
            model: "unrecognized-custom-model".into(),
            context_tokens: 8192,
            output_tokens: 512,
            timeout_seconds: 2,
            ..Default::default()
        };
        (
            GuardedProvider::new(
                Arc::new(Fixture {
                    calls: calls.clone(),
                }),
                &config,
                directory.join("local-inference.lock"),
                CancellationToken::new(),
                true,
            ),
            calls,
        )
    }
    #[tokio::test]
    async fn custom_model_overflow_is_rejected_before_provider_call() {
        let directory = tempfile::tempdir().unwrap();
        let (guard, calls) = guard(directory.path());
        let result = guard
            .chat_completion(
                &[Message::user("é".repeat(6000))],
                &[],
                Some(512),
                None,
                None,
                None,
            )
            .await;
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("Context budget exceeded")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn streaming_lease_is_retained_until_drop_then_released() {
        let directory = tempfile::tempdir().unwrap();
        let (guard, _) = guard(directory.path());
        let mut stream = guard.chat_completion_stream(
            &[Message::user("small prompt")],
            &[],
            Some(512),
            None,
            None,
            None,
        );
        assert!(stream.next().await.unwrap().is_ok());
        let competing = OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.path().join("local-inference.lock"))
            .unwrap();
        assert!(fs2::FileExt::try_lock_exclusive(&competing).is_err());
        drop(stream);
        assert!(fs2::FileExt::try_lock_exclusive(&competing).is_ok());
        fs2::FileExt::unlock(&competing).unwrap();
        assert!(
            guard
                .chat_completion(&[Message::user("second")], &[], Some(512), None, None, None)
                .await
                .is_ok()
        );
    }
    #[tokio::test]
    async fn cancelled_waiting_generation_never_enters_provider() {
        let directory = tempfile::tempdir().unwrap();
        let (guard, calls) = guard(directory.path());
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.path().join("local-inference.lock"))
            .unwrap();
        fs2::FileExt::lock_exclusive(&lock).unwrap();
        guard.cancel.cancel();
        assert!(
            guard
                .chat_completion(&[Message::user("small")], &[], Some(512), None, None, None)
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn remote_provider_is_not_serialized_by_local_lock() {
        let directory = tempfile::tempdir().unwrap();
        let (mut guard, _) = guard(directory.path());
        guard.local_lock = None;
        assert!(
            guard
                .chat_completion(&[Message::user("small")], &[], Some(512), None, None, None)
                .await
                .is_ok()
        );
        assert!(!directory.path().join("local-inference.lock").exists());
    }
}

#[cfg(test)]
mod cancellation_boundary_tests {
    use super::*;
    use hermes_core::StreamDelta;
    struct OpenStream;
    #[async_trait]
    impl LlmProvider for OpenStream {
        async fn chat_completion(
            &self,
            _: &[Message],
            _: &[ToolSchema],
            _: Option<u32>,
            _: Option<f64>,
            _: Option<&str>,
            _: Option<&serde_json::Value>,
        ) -> Result<LlmResponse, AgentError> {
            std::future::pending().await
        }
        fn chat_completion_stream(
            &self,
            _: &[Message],
            _: &[ToolSchema],
            _: Option<u32>,
            _: Option<f64>,
            _: Option<&str>,
            _: Option<&serde_json::Value>,
        ) -> BoxStream<'static, Result<StreamChunk, AgentError>> {
            Box::pin(
                async_stream::stream! {yield Ok(StreamChunk{delta:Some(StreamDelta{content:Some("partial".into()),tool_calls:None,extra:None}),finish_reason:None,usage:None});std::future::pending::<()>().await;},
            )
        }
    }
    #[tokio::test]
    async fn cancelling_a_polled_open_stream_returns_typed_interruption_every_time() {
        let directory = tempfile::tempdir().unwrap();
        for _ in 0..32 {
            let cancel = CancellationToken::new();
            let config = ProviderConfig {
                model: "fixture".into(),
                ..Default::default()
            };
            let guard = GuardedProvider::new(
                Arc::new(OpenStream),
                &config,
                directory.path().join("lease"),
                cancel.clone(),
                false,
            );
            let mut stream =
                guard.chat_completion_stream(&[Message::user("task")], &[], None, None, None, None);
            assert!(stream.next().await.unwrap().is_ok());
            cancel.cancel();
            assert!(matches!(
                stream.next().await,
                Some(Err(AgentError::Interrupted { .. }))
            ));
            assert!(stream.next().await.is_none());
        }
    }
}
