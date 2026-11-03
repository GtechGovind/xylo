//! Xylo's bundled Obscura browser. CDP is a private implementation detail.
mod cdp;

use crate::{
    CapabilityConfig, definition, observation, output_path,
    process::{ManagedProcess, ProcessSpec, validate_executable},
    required_str,
};
use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use cdp::Cdp;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, sync::Mutex};
use tokio_util::sync::CancellationToken;
use xylo_core::{ToolDefinition, ToolObservation};

const DOWNLOAD_UNSUPPORTED: &str = "browser_download is unavailable with Obscura v0.2.6: its CDP download controls do not enforce byte limits or cancellation. Use a bounded HTTP download tool instead; no browser download was started.";
const MAX_SCREENSHOT_BYTES: usize = 8 * 1024 * 1024;

/// The requested browser operation never reached the native browser. This is
/// stronger evidence than an error returned after navigation/click dispatch.
#[derive(Debug)]
pub struct BrowserNotDispatched(anyhow::Error);

impl std::fmt::Display for BrowserNotDispatched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}
impl std::error::Error for BrowserNotDispatched {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

fn validate_operation(name: &str, args: &Value) -> Result<()> {
    match name {
        "browser_navigate" => {
            validate_url(required_str(args, "url")?)?;
        }
        "browser_click" | "browser_wait" => {
            required_str(args, "selector")?;
        }
        "browser_type" => {
            required_str(args, "selector")?;
            ensure!(
                required_str(args, "text")?.len() <= 100_000,
                "Input text exceeds limit"
            );
        }
        "browser_snapshot" | "browser_screenshot" | "browser_close" => {}
        "browser_download" => anyhow::bail!(DOWNLOAD_UNSUPPORTED),
        _ => anyhow::bail!("Unknown browser tool: {name}"),
    }
    Ok(())
}

fn sibling_runtime(current_executable: &Path) -> Result<PathBuf> {
    let directory = current_executable
        .parent()
        .context("Xylo executable has no parent directory")?;
    Ok(directory.join(if cfg!(windows) {
        "xylo-obscura.exe"
    } else {
        "xylo-obscura"
    }))
}

/// Resolve only the native component shipped beside the running Xylo binary.
/// There is deliberately no environment, PATH, import, or remote fallback.
pub fn runtime_executable() -> Result<PathBuf> {
    let current = std::env::current_exe().context("Cannot locate the running Xylo executable")?;
    validate_executable(&sibling_runtime(&current)?)
        .context("Bundled xylo-obscura runtime is missing or invalid; repair the Xylo installation")
}

// Keep storage alive until after the process tree has been stopped. Drop kills
// synchronously even when an operation future is aborted during launch or CDP.
struct Sidecar {
    child: Option<ManagedProcess>,
    profile: Option<tempfile::TempDir>,
}

impl Sidecar {
    fn is_running(&mut self) -> Result<bool> {
        self.child.as_mut().context("Obscura stopped")?.is_running()
    }

    fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(ManagedProcess::pid)
    }

    fn profile(&self) -> &Path {
        self.profile.as_ref().expect("live sidecar profile").path()
    }

    async fn stop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.stop().await;
        }
        self.child.take();
        self.profile.take();
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.finish_tree();
            let profile = self.profile.take();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = child.stop().await;
                    drop(child);
                    drop(profile);
                });
            } else {
                // ManagedProcess also kills on drop. Without a running Tokio
                // runtime, asynchronous reaping cannot be promised.
                drop(child);
                drop(profile);
            }
        }
    }
}

struct Session {
    process: Sidecar,
    cdp: Cdp,
    used: Instant,
}

impl Session {
    async fn close(mut self) {
        // Browser.close acknowledges without stopping Obscura v0.2.6.
        self.process.stop().await;
    }
}

#[derive(Deserialize)]
struct ReadyRecord {
    pid: u32,
    host: String,
    port: u16,
    websocket_path: String,
}

impl ReadyRecord {
    fn endpoint(&self, pid: Option<u32>) -> Result<String> {
        ensure!(
            Some(self.pid) == pid && self.pid != 0,
            "Obscura ready file PID mismatch"
        );
        ensure!(
            self.host == "127.0.0.1"
                && self.port != 0
                && self.websocket_path == "/devtools/browser",
            "Obscura ready file is not the expected loopback endpoint"
        );
        Ok(format!(
            "ws://127.0.0.1:{}{}",
            self.port, self.websocket_path
        ))
    }
}

fn launch_spec(
    executable: PathBuf,
    profile: &Path,
    token: String,
    timeout: Duration,
    allow_private_network: bool,
) -> ProcessSpec {
    // Verified against v0.2.6 crates/obscura-cli/src/main.rs Serve options.
    let mut args = vec![
        "serve".into(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        "0".into(),
        "--workers".into(),
        "1".into(),
        "--max-connections".into(),
        "1".into(),
        "--ready-file".into(),
        profile.join("ready.json").to_string_lossy().into_owned(),
        "--storage-dir".into(),
        profile.join("storage").to_string_lossy().into_owned(),
    ];
    if allow_private_network {
        args.push("--allow-private-network".into());
    }
    ProcessSpec {
        executable,
        args,
        cwd: profile.to_owned(),
        env: BTreeMap::from([("OBSCURA_CDP_TOKEN".into(), token)]),
        timeout,
        output_limit: 64 * 1024,
    }
}

fn screenshot_bytes(response: &Value) -> Result<Vec<u8>> {
    let encoded = response["data"]
        .as_str()
        .context("Obscura omitted screenshot data")?;
    ensure!(
        encoded.len() <= MAX_SCREENSHOT_BYTES,
        "Screenshot exceeds encoded byte limit"
    );
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    ensure!(
        bytes.len() >= 24 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") && &bytes[12..16] == b"IHDR",
        "Obscura did not return a PNG screenshot"
    );
    ensure!(
        u32::from_be_bytes(bytes[16..20].try_into()?) == 1280
            && u32::from_be_bytes(bytes[20..24].try_into()?) == 900,
        "Obscura returned unexpected screenshot dimensions"
    );
    // Header checks constrain dimensions before decoding; do not publish a
    // truncated payload merely because its signature resembles a PNG.
    image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
        .context("Obscura returned an invalid PNG screenshot")?;
    Ok(bytes)
}

fn save_screenshot(path: &Path, bytes: &[u8]) -> Result<()> {
    // Write privately first, then publish without replacing an
    // existing artifact (including symlinks). A failed write leaves no partial
    // artifact at the published path.
    let directory = path.parent().context("Screenshot path has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(bytes)?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)?;
    Ok(())
}

pub struct BrowserService {
    config: CapabilityConfig,
    state: Arc<Mutex<Option<Session>>>,
}

impl BrowserService {
    pub fn new(config: CapabilityConfig) -> Self {
        Self {
            config,
            state: Arc::new(Mutex::new(None)),
        }
    }

    async fn launch(&self) -> Result<Session> {
        let executable = runtime_executable()?;
        let profile = tempfile::Builder::new().prefix("xylo-obscura-").tempdir()?;
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let spec = launch_spec(
            executable,
            profile.path(),
            token.clone(),
            Duration::from_secs(self.config.operation_timeout_seconds),
            self.config.browser_allow_private_network,
        );
        let child = ManagedProcess::spawn(&spec)?;
        let mut process = Sidecar {
            child: Some(child),
            profile: Some(profile),
        };
        let endpoint = loop {
            ensure!(
                process.is_running()?,
                "Bundled Obscura exited before publishing readiness"
            );
            match tokio::fs::File::open(process.profile().join("ready.json")).await {
                Ok(file) => {
                    let mut bytes = Vec::new();
                    file.take(4097).read_to_end(&mut bytes).await?;
                    ensure!(bytes.len() <= 4096, "Obscura ready file exceeds limit");
                    let record: ReadyRecord =
                        serde_json::from_slice(&bytes).context("Invalid Obscura ready file")?;
                    break record.endpoint(process.pid())?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => return Err(error.into()),
            }
        };
        let mut cdp = Cdp::connect(&endpoint, &token).await?;
        cdp.initialize().await?;
        let weak = Arc::downgrade(&self.state);
        let idle = Duration::from_secs(self.config.helper_idle_seconds.clamp(5, 3600));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(idle.min(Duration::from_secs(10))).await;
                let Some(state) = weak.upgrade() else { break };
                let mut guard = state.lock().await;
                if guard
                    .as_ref()
                    .is_some_and(|session| session.used.elapsed() >= idle)
                {
                    if let Some(session) = guard.take() {
                        session.close().await;
                    }
                    break;
                }
                if guard.is_none() {
                    break;
                }
            }
        });
        Ok(Session {
            process,
            cdp,
            used: Instant::now(),
        })
    }

    pub async fn status(&self) -> Value {
        let Ok(mut state) = self.state.try_lock() else {
            return json!({"state":"busy","backend":"obscura","download":"unavailable"});
        };
        let running = state
            .as_mut()
            .is_some_and(|session| session.process.is_running().unwrap_or(false));
        json!({
            "state": if running { "running" } else if runtime_executable().is_ok() { "ready" } else { "missing" },
            "backend":"obscura",
            "profile":"isolated",
            "download":"unavailable"
        })
    }

    pub async fn shutdown(&self) -> Result<()> {
        if let Some(session) = self.state.lock().await.take() {
            session.close().await;
        }
        Ok(())
    }

    pub async fn execute(
        &self,
        name: &str,
        args: &Value,
        cancel: CancellationToken,
    ) -> Result<ToolObservation> {
        // Reject invalid operations before locking or launching a browser.
        validate_operation(name, args).map_err(BrowserNotDispatched)?;
        // In-flight operations own their session and drop its process on failure.
        // A cancelled request still waiting for the mutex must not tear down a
        // different request's healthy session or wait for that request to finish.
        let dispatched = AtomicBool::new(false);
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(anyhow::anyhow!("Browser operation cancelled")),
            _ = tokio::time::sleep(Duration::from_secs(self.config.operation_timeout_seconds)) => {
                Err(anyhow::anyhow!("Browser operation timed out"))
            }
            result = self.inner(name, args, &dispatched) => result,
        };
        result.map_err(|error| {
            if dispatched.load(Ordering::Acquire) {
                error
            } else {
                BrowserNotDispatched(error).into()
            }
        })
    }

    async fn inner(
        &self,
        name: &str,
        args: &Value,
        dispatched: &AtomicBool,
    ) -> Result<ToolObservation> {
        if name == "browser_close" {
            if let Some(session) = self.state.lock().await.take() {
                dispatched.store(true, Ordering::Release);
                session.close().await;
            }
            return Ok(observation(json!({"closed":true}), vec![]));
        }
        ensure!(
            [
                "browser_navigate",
                "browser_snapshot",
                "browser_click",
                "browser_type",
                "browser_wait",
                "browser_screenshot"
            ]
            .contains(&name),
            "Unknown browser tool: {name}"
        );
        let mut state = self.state.lock().await;
        if state.is_none() {
            *state = Some(self.launch().await?);
        }
        // Own the session while holding the serialization lock. Aborting this
        // future also drops (and kills) the sidecar, not a half-used connection.
        let mut session = state.take().context("Browser failed to initialize")?;
        ensure!(
            session.process.is_running()?,
            "Bundled Obscura process has exited"
        );
        let cdp = &mut session.cdp;
        // From this point a CDP request can reach a page. Even a TLS error may
        // follow a redirect or unload script, so error text cannot prove that
        // no side effects occurred. Keep those errors uncertain.
        dispatched.store(true, Ordering::Release);
        let value = match name {
            "browser_navigate" => {
                let url = validate_url(required_str(args, "url")?)?;
                let response = cdp
                    .call(
                        "Page.navigate",
                        json!({"url":url.as_str(),"waitUntil":"load"}),
                    )
                    .await?;
                ensure!(
                    response.get("errorText").is_none(),
                    "Browser navigation failed: {}",
                    response["errorText"]
                );
                cdp.evaluate("({url:location.href,title:document.title})".into())
                    .await?
            }
            "browser_snapshot" => {
                let mut response = cdp.call("Accessibility.getFullAXTree", json!({})).await?;
                let limit = args
                    .get("max_nodes")
                    .and_then(Value::as_u64)
                    .unwrap_or(300)
                    .clamp(1, 2000) as usize;
                let nodes = response["nodes"]
                    .as_array_mut()
                    .context("Obscura omitted accessibility nodes")?;
                let total = nodes.len();
                nodes.truncate(limit);
                response["total_nodes"] = json!(total);
                response["truncated"] = json!(total > limit);
                response
            }
            "browser_click" | "browser_type" => {
                let selector = serde_json::to_string(required_str(args, "selector")?)?;
                let action = if name == "browser_click" {
                    "if(e.matches(':disabled'))return false;e.click()"
                } else {
                    "if(!e.matches('input,textarea')||e.matches(':disabled')||e.readOnly)return false;e.focus();if(document.activeElement!==e)return false"
                };
                let found = cdp.evaluate(format!(
                    "(()=>{{const e=document.querySelector({selector});if(!e)return false;{action};return true;}})()"
                )).await?;
                ensure!(
                    found == json!(true),
                    "Browser selector was not found or is not actionable"
                );
                if name == "browser_type" {
                    let text = required_str(args, "text")?;
                    ensure!(text.len() <= 100_000, "Input text exceeds limit");
                    cdp.call("Input.insertText", json!({"text":text})).await?;
                    json!({"input_dispatched":true})
                } else {
                    json!({"click_dispatched":true})
                }
            }
            "browser_wait" => {
                let selector = required_str(args, "selector")?;
                let literal = serde_json::to_string(selector)?;
                loop {
                    if cdp
                        .evaluate(format!("document.querySelector({literal}) !== null"))
                        .await?
                        == json!(true)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                json!({"found":selector})
            }
            "browser_screenshot" => {
                let response = cdp
                    .call(
                        "Page.captureScreenshot",
                        json!({
                            "format":"png", "captureBeyondViewport":false
                        }),
                    )
                    .await?;
                let bytes = screenshot_bytes(&response)?;
                let path = output_path(
                    &self.config,
                    &format!("browser-{}", uuid::Uuid::new_v4()),
                    "png",
                )?;
                save_screenshot(&path, &bytes)?;
                session.used = Instant::now();
                *state = Some(session);
                return Ok(observation(
                    json!({"path":path,"width":1280,"height":900,"viewport_only":true}),
                    vec![path],
                ));
            }
            _ => unreachable!(),
        };
        session.used = Instant::now();
        *state = Some(session);
        Ok(observation(value, vec![]))
    }
}

fn validate_url(raw: &str) -> Result<url::Url> {
    let url = url::Url::parse(raw)?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "Browser accepts HTTP(S) URLs only"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "Credentials in URLs are not supported"
    );
    Ok(url)
}

pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        definition(
            "browser_navigate",
            "Navigate an isolated bundled Obscura tab to an HTTP(S) URL.",
            json!({"url":{"type":"string"}}),
            &["url"],
            true,
        ),
        definition(
            "browser_snapshot",
            "Read Obscura's accessibility tree (not Chromium-equivalent). max_nodes limits returned nodes, not upstream tree construction.",
            json!({"max_nodes":{"type":"integer","minimum":1,"maximum":2000}}),
            &[],
            false,
        ),
        definition(
            "browser_click",
            "Dispatch a DOM click to a CSS-selected element in Obscura (not a physical pointer click).",
            json!({"selector":{"type":"string"}}),
            &["selector"],
            true,
        ),
        definition(
            "browser_type",
            "Insert text into a focused, editable CSS-selected input or textarea in Obscura.",
            json!({"selector":{"type":"string"},"text":{"type":"string"}}),
            &["selector", "text"],
            true,
        ),
        definition(
            "browser_wait",
            "Wait for a CSS selector in Obscura, subject to the operation timeout.",
            json!({"selector":{"type":"string"}}),
            &["selector"],
            false,
        ),
        definition(
            "browser_screenshot",
            "Save the current 1280x900 Obscura viewport as a new PNG artifact without overwriting existing files.",
            json!({}),
            &[],
            false,
        ),
        definition(
            "browser_close",
            "Stop the bundled Obscura sidecar and remove its temporary profile.",
            json!({}),
            &[],
            true,
        ),
    ]
}

#[cfg(test)]
mod tests;
