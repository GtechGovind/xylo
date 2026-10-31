//! Lazy local inference and speech helpers. Models and native executables are
//! explicit local assets. No model/package/browser is downloaded by this crate.

use crate::{
    CapabilityConfig, definition, observation,
    process::{self, ManagedProcess, ProcessSpec},
    required_str,
};
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use xylo_core::{ToolDefinition, ToolObservation};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LlamaConfig {
    pub executable: PathBuf,
    pub model: PathBuf,
    /// Zero leaves context unset so the native memory fitter can choose it.
    pub context_tokens: u32,
    /// Zero resolves available CPU parallelism when the server starts.
    pub threads: u16,
    /// -1 selects native auto placement; -2 requests all layers; 0 is CPU only.
    pub gpu_layers: i32,
    /// Native fitting applies to unset parameters; explicit overrides stay exact.
    pub fit_memory: bool,
}

impl Default for LlamaConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::new(),
            model: PathBuf::new(),
            context_tokens: 0,
            threads: 0,
            gpu_layers: -1,
            fit_memory: true,
        }
    }
}

impl LlamaConfig {
    fn server_arguments(&self, port: u16) -> Result<Vec<String>> {
        ensure!(
            self.context_tokens == 0 || (512..=1_048_576).contains(&self.context_tokens),
            "Invalid local inference context limit"
        );
        ensure!(
            (-2..=65_535).contains(&self.gpu_layers),
            "Invalid GPU layer configuration"
        );
        let threads = if self.threads == 0 {
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1)
        } else {
            usize::from(self.threads)
        };
        let gpu_layers = match self.gpu_layers {
            -1 => "auto".into(),
            -2 => "all".into(),
            layers => layers.to_string(),
        };
        let mut args = vec![
            "--model".into(),
            self.model.to_string_lossy().into_owned(),
            "--host".into(),
            "127.0.0.1".into(),
            "--port".into(),
            port.to_string(),
            "--threads".into(),
            threads.to_string(),
            "--n-gpu-layers".into(),
            gpu_layers,
            "--fit".into(),
            if self.fit_memory { "on" } else { "off" }.into(),
            "--parallel".into(),
            "1".into(),
        ];
        // In the pinned native version an explicit --ctx-size 0 means full
        // trained context and disables fit reduction. Omit it for true auto.
        if self.context_tokens != 0 {
            args.extend(["--ctx-size".into(), self.context_tokens.to_string()]);
        }
        Ok(args)
    }
}

/// argv templates for a locally installed whisper.cpp or sherpa-onnx helper.
/// Substitutions form individual argv entries, never shell source. `assets`
/// supports sherpa's multi-file encoders/decoders/tokens without assumptions
/// about a particular model architecture or executable's command-line flags.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpeechHelper {
    pub executable: PathBuf,
    pub assets: BTreeMap<String, PathBuf>,
    pub args: Vec<String>,
    #[serde(default)]
    pub synthesis_args: Option<Vec<String>>,
    #[serde(default = "default_extension")]
    pub output_extension: String,
}
fn default_extension() -> String {
    "txt".into()
}

struct Server {
    process: ManagedProcess,
    endpoint: String,
    used: Instant,
    _registration: crate::managed_endpoints::Registration,
}

/// Exclusive proof that no managed root chat or generation is active. Holding
/// it also prevents a new root activity lease during a configuration change.
pub struct IdleActivityLease {
    root: PathBuf,
    _activity: std::fs::File,
    _generation: std::fs::File,
}

fn try_idle_activity_lease(config: &CapabilityConfig) -> Result<IdleActivityLease> {
    let root = config
        .artifacts
        .parent()
        .context("Artifacts directory has no parent")?;
    let acquire = |name: &str, message: &str| -> Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(name))?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(file),
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                anyhow::bail!("{message}")
            }
            Err(error) => Err(error.into()),
        }
    };
    let activity = acquire(
        "local-inference-activity.lock",
        "A managed local chat is active. Finish or cancel it before stopping the model or changing native setup.",
    )?;
    let generation = acquire(
        "local-inference.lock",
        "A local generation is active. Wait for it to finish or cancel it before stopping the model or changing native setup.",
    )?;
    Ok(IdleActivityLease {
        root: root.to_path_buf(),
        _activity: activity,
        _generation: generation,
    })
}
pub struct ModelService {
    config: CapabilityConfig,
    server: Arc<Mutex<Option<Server>>>,
}

impl ModelService {
    pub fn new(config: CapabilityConfig) -> Self {
        Self {
            config,
            server: Arc::new(Mutex::new(None)),
        }
    }

    async fn start(&self, cancel: CancellationToken) -> Result<String> {
        let mut guard = self.server.lock().await;
        if let Some(server) = guard.as_mut() {
            if server.process.is_running()? {
                server.used = Instant::now();
                return Ok(server.endpoint.clone());
            }
            *guard = None;
        }
        let cfg = self
            .config
            .llama
            .as_ref()
            .context("Configure local llama.cpp server and GGUF model paths")?;
        process::validate_executable(&cfg.executable)?;
        ensure!(
            cfg.model.is_absolute() && cfg.model.is_file(),
            "Configured GGUF model is missing"
        );
        let port = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?
            .local_addr()?
            .port();
        let args = cfg.server_arguments(port)?;
        let mut process = ManagedProcess::spawn(&ProcessSpec {
            executable: cfg.executable.clone(),
            args,
            cwd: self.config.workspace.clone(),
            env: BTreeMap::new(),
            timeout: Duration::from_secs(self.config.operation_timeout_seconds),
            output_limit: self.config.max_output_bytes,
        })?;
        let endpoint = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()?;
        let readiness = async {
            loop {
                ensure!(
                    process.is_running()?,
                    "llama.cpp exited before becoming ready; verify the binary, model and hardware configuration"
                );
                if let Ok(response) = client.get(format!("{endpoint}/health")).send().await
                    && response.status().is_success()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Ok::<(), anyhow::Error>(())
        };
        let ready = tokio::select! {
            result=readiness=>result,
            _=cancel.cancelled()=>Err(anyhow::anyhow!("Local inference start cancelled")),
            _=tokio::time::sleep(Duration::from_secs(self.config.operation_timeout_seconds))=>Err(anyhow::anyhow!("Local model did not become ready before timeout")),
        };
        if let Err(error) = ready {
            process.stop().await?;
            return Err(error);
        }
        let registration = match crate::managed_endpoints::Registration::create(
            self.config
                .artifacts
                .parent()
                .context("Artifacts root has no parent")?,
            &format!("{endpoint}/v1"),
            process.pid().context("Local model process has no PID")?,
        ) {
            Ok(registration) => registration,
            Err(error) => {
                process.stop().await?;
                return Err(error);
            }
        };
        *guard = Some(Server {
            process,
            endpoint: endpoint.clone(),
            used: Instant::now(),
            _registration: registration,
        });
        self.watch_idle();
        Ok(endpoint)
    }

    fn watch_idle(&self) {
        let weak = Arc::downgrade(&self.server);
        let idle_config = self.config.clone();
        let idle = Duration::from_secs(self.config.helper_idle_seconds.clamp(5, 3600));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(idle.min(Duration::from_secs(10))).await;
                let Some(server) = weak.upgrade() else {
                    break;
                };
                let mut guard = server.lock().await;
                if guard
                    .as_mut()
                    .is_some_and(|server| !server.process.is_running().unwrap_or(false))
                {
                    *guard = None;
                }
                if guard.as_ref().is_some_and(|s| s.used.elapsed() >= idle) {
                    // Activity spans the entire root turn, including tool calls;
                    // generation spans only a provider request. Neither may be
                    // stopped by idle eviction or a concurrent setup change.
                    let Ok(_idle_lease) = try_idle_activity_lease(&idle_config) else {
                        if let Some(server) = guard.as_mut() {
                            server.used = Instant::now();
                        }
                        continue;
                    };
                    if let Some(mut server) = guard.take() {
                        let _ = server.process.stop().await;
                    }
                    break;
                }
                if guard.is_none() {
                    break;
                }
            }
        });
    }

    pub async fn shutdown(&self) -> Result<()> {
        let lease = self.idle_activity_lease()?;
        self.shutdown_with_activity_lease(&lease).await
    }

    pub(crate) fn idle_activity_lease(&self) -> Result<IdleActivityLease> {
        try_idle_activity_lease(&self.config)
    }

    pub(crate) async fn shutdown_with_activity_lease(
        &self,
        lease: &IdleActivityLease,
    ) -> Result<()> {
        ensure!(
            self.config.artifacts.parent() == Some(lease.root.as_path()),
            "Activity lease belongs to another native profile"
        );
        if let Some(mut server) = self.server.lock().await.take() {
            server.process.stop().await?;
        }
        Ok(())
    }

    pub async fn status(&self) -> Value {
        let Ok(mut guard) = self.server.try_lock() else {
            return json!({"llama_cpp":{"state":"busy"},"whisper_cpp":helper_status(self.config.whisper.as_ref()),"sherpa_onnx":helper_status(self.config.sherpa.as_ref())});
        };
        let local = if let Some(server) = guard.as_mut() {
            json!({"state":if server.process.is_running().unwrap_or(false){"running"}else{"exited"},"endpoint":format!("{}/v1",server.endpoint),"pid":server.process.pid()})
        } else {
            json!({"state":if self.config.llama.is_some(){"configured_unverified"}else{"configuration_needed"}})
        };
        json!({"llama_cpp":local,"whisper_cpp":helper_status(self.config.whisper.as_ref()),"sherpa_onnx":helper_status(self.config.sherpa.as_ref())})
    }

    pub async fn execute(
        &self,
        name: &str,
        args: &Value,
        cancel: CancellationToken,
    ) -> Result<ToolObservation> {
        match name {
            "local_inference_status" => Ok(observation(self.status().await, vec![])),
            "local_inference_start" => {
                let url = self.start(cancel).await?;
                Ok(observation(
                    json!({"state":"ready","endpoint":format!("{url}/v1"),"health_checked":true}),
                    vec![],
                ))
            }
            "local_inference_stop" => {
                self.shutdown().await?;
                Ok(observation(json!({"state":"stopped"}), vec![]))
            }
            "local_inference_generate" => {
                // Shared with the host provider path: keep the OS lock only
                // for this request, never for the lifetime of ModelService.
                let _inference_lease = acquire_inference_lock(&self.config, &cancel).await?;
                let endpoint = self.start(cancel.clone()).await?;
                let mut guard = self.server.lock().await;
                let server = guard.as_mut().context("Local server stopped")?;
                server.used = Instant::now();
                let prompt = required_str(args, "prompt")?;
                ensure!(
                    prompt.len() <= 256 * 1024,
                    "Local prompt exceeds byte budget"
                );
                let max_tokens = args
                    .get("max_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(1024)
                    .clamp(1, 4096);
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(self.config.operation_timeout_seconds))
                    .build()?;
                let generate = async {
                    let response=client.post(format!("{endpoint}/v1/chat/completions")).json(&json!({"messages":[{"role":"user","content":prompt}],"max_tokens":max_tokens,"stream":false})).send().await?.error_for_status()?;
                    let mut bytes = Vec::new();
                    let mut stream = response.bytes_stream();
                    while let Some(chunk) = stream.next().await {
                        let chunk = chunk?;
                        ensure!(
                            bytes.len() + chunk.len() <= self.config.max_output_bytes,
                            "Local response exceeds byte budget"
                        );
                        bytes.extend_from_slice(&chunk);
                    }
                    Ok::<_, anyhow::Error>(observation(serde_json::from_slice(&bytes)?, vec![]))
                };
                let result = tokio::select! {result=generate=>result,_=cancel.cancelled()=>Err(anyhow::anyhow!("Local generation cancelled"))};
                server.used = Instant::now();
                if result.is_err() {
                    server.process.stop().await?;
                    *guard = None;
                }
                result
            }
            "speech_transcribe" | "speech_synthesize" => {
                crate::speech::execute(&self.config, name, args, cancel).await
            }

            _ => anyhow::bail!("Unknown model/speech tool"),
        }
    }
}

fn helper_status(helper: Option<&SpeechHelper>) -> &'static str {
    match helper {
        None => "configuration_needed",
        Some(h) if h.assets.is_empty() => "configuration_needed",
        Some(h) if !h.executable.is_file() || h.assets.values().any(|p| !p.is_file()) => {
            "asset_missing"
        }
        Some(_) => "configured_unverified",
    }
}

pub(crate) async fn acquire_inference_lock(
    config: &CapabilityConfig,
    cancel: &CancellationToken,
) -> Result<std::fs::File> {
    let path = config
        .artifacts
        .parent()
        .context("Artifacts directory has no parent")?
        .join("local-inference.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(config.operation_timeout_seconds);
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(file),
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => (),
            Err(error) => return Err(error.into()),
        }
        tokio::select! {
            _ = cancel.cancelled() => anyhow::bail!("Local inference lease cancelled"),
            _ = tokio::time::sleep_until(deadline) => anyhow::bail!("Local inference resource is busy; request deadline exceeded"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => (),
        }
    }
}

/// Placeholders whose values come from the model rather than from Xylo. As a
/// whole argument they must follow a `--`, so text such as `--help` or
/// `--vits-model=/elsewhere` can never be read as a helper option.
const UNTRUSTED_PLACEHOLDERS: &[&str] = &["text"];

/// Expands a speech helper's argv template in one pass. Placeholders are
/// found in the template, never in substituted values, so a value may
/// contain braces or text that looks like another placeholder.
pub(crate) fn expand_arguments(
    args: &[String],
    values: &BTreeMap<String, String>,
) -> Result<Vec<String>> {
    ensure!(args.len() <= 128, "Too many speech helper arguments");
    let mut options_ended = false;
    let mut expanded = Vec::with_capacity(args.len());
    for arg in args {
        if arg == "--" {
            options_ended = true;
        }
        if let Some(name) = arg
            .strip_prefix('{')
            .and_then(|rest| rest.strip_suffix('}'))
            && UNTRUSTED_PLACEHOLDERS.contains(&name)
        {
            ensure!(
                options_ended,
                "Speech argv template must put `--` before {{{name}}}"
            );
        }
        expanded.push(expand_argument(arg, values)?);
    }
    Ok(expanded)
}

/// One template argument with each `{name}` replaced by its value.
fn expand_argument(template: &str, values: &BTreeMap<String, String>) -> Result<String> {
    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find(['{', '}']) {
        ensure!(
            rest[open..].starts_with('{'),
            "Unbalanced brace in speech argv template"
        );
        output.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after
            .find('}')
            .context("Unbalanced brace in speech argv template")?;
        let name = &after[..close];
        let value = values
            .get(name)
            .with_context(|| format!("Unknown speech argv template placeholder {{{name}}}"))?;
        output.push_str(value);
        rest = &after[close + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        definition(
            "local_inference_status",
            "Inspect configured native model helpers and verified process state.",
            json!({}),
            &[],
            false,
        ),
        definition(
            "local_inference_start",
            "Start locally provisioned llama.cpp server lazily and verify its loopback health endpoint.",
            json!({}),
            &[],
            true,
        ),
        definition(
            "local_inference_stop",
            "Stop and reap the managed local model server.",
            json!({}),
            &[],
            true,
        ),
        definition(
            "local_inference_generate",
            "Run one generation on the configured local llama.cpp server with bounded output.",
            json!({"prompt":{"type":"string"},"max_tokens":{"type":"integer","minimum":1,"maximum":4096}}),
            &["prompt"],
            true,
        ),
        definition(
            "speech_transcribe",
            "Transcribe a local audio file using the explicitly configured whisper.cpp or sherpa-onnx native helper. Model/argv must match installed helper.",
            json!({"engine":{"type":"string","enum":["whisper","sherpa"]},"path":{"type":"string"}}),
            &["engine", "path"],
            true,
        ),
        definition(
            "speech_synthesize",
            "Synthesize speech using a configured native sherpa helper and explicit local voice assets.",
            json!({"engine":{"type":"string","enum":["sherpa"]},"text":{"type":"string","maxLength":4000}}),
            &["engine", "text"],
            true,
        ),
    ]
}

#[cfg(test)]
mod tests;
