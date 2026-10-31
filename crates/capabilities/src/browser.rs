//! A dedicated, explicitly provisioned Chromium profile controlled through CDP.

use crate::{
    CapabilityConfig, definition, observation, output_path, process::validate_executable,
    required_str,
};
use anyhow::{Context, Result, ensure};
use chromiumoxide::{
    Browser, BrowserConfig, Page,
    cdp::browser_protocol::{
        accessibility::GetFullAxTreeParams,
        browser::{
            CancelDownloadParams, DownloadProgressState, EventDownloadProgress,
            EventDownloadWillBegin, SetDownloadBehaviorBehavior, SetDownloadBehaviorParams,
        },
        page::{CaptureScreenshotFormat, NavigateParams},
    },
    page::ScreenshotParams,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::Mutex, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use xylo_core::{ToolDefinition, ToolObservation};

/// Download budget used when `browser_download` is called without `max_bytes`.
const DEFAULT_DOWNLOAD_MAX_BYTES: u64 = 50 * 1024 * 1024;
/// Hard ceiling for any single browser download; matches the tool schema's
/// `max_bytes.maximum` so a caller cannot raise the budget past it.
const DOWNLOAD_MAX_BYTES_CEILING: u64 = 100 * 1024 * 1024;

struct Session {
    browser: Browser,
    page: Page,
    handler: JoinHandle<()>,
    profile: PathBuf,
    downloads: PathBuf,
    used: Instant,
}

impl Session {
    async fn close(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(3), self.browser.close()).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), self.browser.wait()).await;
        self.handler.abort();
        drop(self.browser); // chromiumoxide kills/reaps a launched browser on drop.
        let _ = tokio::fs::remove_dir_all(self.profile).await;
    }
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
        let chromium=self.config.chromium.clone().or_else(detect_browser).context("Browser automation requires Google Chrome, Chromium, or Microsoft Edge installed on this computer")?;
        let executable = validate_executable(&chromium)?;
        let profile = self
            .config
            .artifacts
            .join(format!(".browser-{}", uuid::Uuid::new_v4()));
        let downloads = profile.join("downloads");
        tokio::fs::create_dir_all(&downloads).await?;
        let cfg = BrowserConfig::builder()
            .chrome_executable(executable)
            .user_data_dir(&profile)
            .window_size(1280, 900)
            .request_timeout(Duration::from_secs(self.config.operation_timeout_seconds))
            .build()
            .map_err(anyhow::Error::msg)?;
        let (browser, mut handler) = Browser::launch(cfg).await?;
        let handler = tokio::spawn(async move {
            while let Some(result) = handler.next().await {
                if result.is_err() {
                    break;
                }
            }
        });
        // Downloads are denied browser-wide from the start so that navigation,
        // clicks, or page script cannot write files; only browser_download
        // opens a short, budgeted window and closes it again.
        let ready = async {
            browser.execute(deny_downloads()).await?;
            Ok::<_, anyhow::Error>(browser.new_page("about:blank").await?)
        }
        .await;
        let page = match ready {
            Ok(page) => page,
            Err(error) => {
                handler.abort();
                let _ = tokio::fs::remove_dir_all(&profile).await;
                return Err(error);
            }
        };
        let weak = Arc::downgrade(&self.state);
        let idle = Duration::from_secs(self.config.helper_idle_seconds.clamp(5, 3600));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(idle.min(Duration::from_secs(10))).await;
                let Some(state) = weak.upgrade() else {
                    break;
                };
                let mut guard = state.lock().await;
                if guard.as_ref().is_some_and(|s| s.used.elapsed() >= idle) {
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
            browser,
            page,
            handler,
            profile,
            downloads,
            used: Instant::now(),
        })
    }

    pub async fn status(&self) -> Value {
        let Ok(state) = self.state.try_lock() else {
            return json!({"state":"busy","protocol":"CDP"});
        };
        json!({"state":if state.is_some() {"running"} else {crate::asset_status(self.config.chromium.clone().or_else(detect_browser).as_deref())}, "protocol":"CDP","profile":"isolated"})
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
        let result = tokio::select! {
            result = self.inner(name, args) => result,
            _ = cancel.cancelled() => Err(anyhow::anyhow!("Browser operation cancelled")),
            _ = tokio::time::sleep(Duration::from_secs(self.config.operation_timeout_seconds)) => Err(anyhow::anyhow!("Browser operation timed out")),
        };
        // A timed-out click or download can have effects. Close the isolated
        // browser rather than replaying the action on an uncertain page.
        if result.is_err() {
            self.shutdown().await?;
        }
        result
    }

    async fn inner(&self, name: &str, args: &Value) -> Result<ToolObservation> {
        if name == "browser_close" {
            self.shutdown().await?;
            return Ok(observation(json!({"closed":true}), vec![]));
        }
        let mut state = self.state.lock().await;
        if state.is_none() {
            *state = Some(self.launch().await?);
        }
        let session = state.as_mut().context("Browser failed to initialize")?;
        session.used = Instant::now();
        let page = &session.page;
        match name {
            "browser_navigate" => {
                let url = validate_url(required_str(args, "url")?)?;
                page.goto(url.as_str()).await?;
                Ok(observation(
                    json!({"url":page.url().await?,"title":page.get_title().await?}),
                    vec![],
                ))
            }
            "browser_snapshot" => {
                let response = page.execute(GetFullAxTreeParams::default()).await?;
                let mut value = serde_json::to_value(response.result)?;
                let limit = args
                    .get("max_nodes")
                    .and_then(Value::as_u64)
                    .unwrap_or(300)
                    .clamp(1, 2000) as usize;
                if let Some(nodes) = value.get_mut("nodes").and_then(Value::as_array_mut) {
                    let total = nodes.len();
                    nodes.truncate(limit);
                    value["total_nodes"] = json!(total);
                    value["truncated"] = json!(total > limit);
                }
                Ok(observation(value, vec![]))
            }
            "browser_click" => {
                page.find_element(required_str(args, "selector")?)
                    .await?
                    .click()
                    .await?;
                Ok(observation(json!({"clicked":true}), vec![]))
            }
            "browser_type" => {
                let text = required_str(args, "text")?;
                ensure!(text.len() <= 100_000, "Input text exceeds limit");
                page.find_element(required_str(args, "selector")?)
                    .await?
                    .focus()
                    .await?
                    .type_str(text)
                    .await?;
                Ok(observation(json!({"typed":true}), vec![]))
            }
            "browser_screenshot" => {
                let path = output_path(
                    &self.config,
                    &format!("browser-{}", uuid::Uuid::new_v4()),
                    "png",
                )?;
                page.save_screenshot(
                    ScreenshotParams::builder()
                        .format(CaptureScreenshotFormat::Png)
                        .full_page(false)
                        .build(),
                    &path,
                )
                .await?;
                Ok(observation(
                    json!({"path":path,"viewport_only":true}),
                    vec![path],
                ))
            }
            "browser_wait" => {
                let selector = required_str(args, "selector")?;
                loop {
                    if page.find_element(selector).await.is_ok() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Ok(observation(json!({"found":selector}), vec![]))
            }
            "browser_download" => {
                let url = validate_url(required_str(args, "url")?)?;
                let max = download_budget(args);
                // A download navigation never finishes loading, and
                // chromiumoxide runs one navigation per page at a time: on the
                // session page it would hold every later browser_navigate
                // until the request timeout. It runs in a page of its own,
                // closed afterwards, which drops that pending navigation.
                let download_page = session.browser.new_page("about:blank").await?;
                let closing = download_page.clone();
                // The restore runs whether the download succeeds or fails, so a
                // failed attempt cannot leave later pages able to download.
                // Cancellation and timeouts drop this future instead; execute()
                // then closes the whole browser, which is stricter still.
                let (file, begin) = run_then_restore(
                    download_to_staging(session, &download_page, &url, max),
                    async {
                        let denied = session.browser.execute(deny_downloads()).await;
                        let closed = closing.close().await;
                        denied?;
                        closed.context("Failed to close the download page")?;
                        Ok(())
                    },
                )
                .await?;
                let extension = Path::new(&begin.suggested_filename)
                    .extension()
                    .and_then(|s| s.to_str())
                    .filter(|s| s.len() <= 12 && s.chars().all(|c| c.is_ascii_alphanumeric()))
                    .unwrap_or("bin");
                let output = output_path(
                    &self.config,
                    &format!("download-{}", uuid::Uuid::new_v4()),
                    extension,
                )?;
                tokio::fs::rename(file, &output).await?;
                Ok(observation(
                    json!({"path":output,"suggested_filename":begin.suggested_filename,"source_url":begin.url}),
                    vec![output],
                ))
            }
            _ => anyhow::bail!("Unknown browser tool: {name}"),
        }
    }
}

/// Allows downloads into the session's private staging directory, navigates to
/// `url`, and waits for the resulting download to finish within `max` bytes.
///
/// Returns the staged file and its begin event. The caller must re-deny
/// downloads afterwards regardless of the outcome (see [`run_then_restore`]).
async fn download_to_staging(
    session: &Session,
    page: &Page,
    url: &url::Url,
    max: u64,
) -> Result<(PathBuf, Arc<EventDownloadWillBegin>)> {
    session
        .browser
        .execute(
            SetDownloadBehaviorParams::builder()
                .behavior(SetDownloadBehaviorBehavior::AllowAndName)
                .download_path(session.downloads.to_string_lossy().to_string())
                .events_enabled(true)
                .build()
                .map_err(anyhow::Error::msg)?,
        )
        .await?;
    let mut starts = session
        .browser
        .event_listener::<EventDownloadWillBegin>()
        .await?;
    let mut progress = session
        .browser
        .event_listener::<EventDownloadProgress>()
        .await?;
    // chromiumoxide recognizes Page.navigate and waits for normal
    // navigation completion even through execute(). Attachments do
    // not finish a page load: consume the download event while the
    // single navigation request remains in flight.
    let begin = {
        let navigation = page.execute(NavigateParams::new(url.as_str()));
        tokio::pin!(navigation);
        tokio::select! {
            event = starts.next() => event.context("Browser closed before download began")?,
            result = &mut navigation => {
                let _ = result?;
                starts.next().await.context("Browser closed before download began")?
            }
        }
    };
    loop {
        // A closed event stream means completion was never observed; the
        // staged file may be partial, so treat it as a failure.
        let event = progress
            .next()
            .await
            .context("Browser closed before download completed")?;
        if event.guid != begin.guid {
            continue;
        }
        if download_exceeds_budget(event.received_bytes, event.total_bytes, max) {
            // Stop the transfer early; the size check below and the session
            // teardown on error still apply if the cancel itself fails.
            if let Err(error) = session
                .browser
                .execute(CancelDownloadParams::new(begin.guid.clone()))
                .await
            {
                anyhow::bail!("Download exceeded byte budget; cancelling it failed: {error}");
            }
            anyhow::bail!("Download exceeded byte budget");
        }
        match event.state {
            DownloadProgressState::Canceled => anyhow::bail!("Download canceled"),
            DownloadProgressState::Completed => break,
            _ => (),
        }
    }
    let file = session.downloads.join(&begin.guid);
    // Progress events are reported by the page's server and Chromium; the
    // bytes on disk are the authoritative measure.
    enforce_file_budget(&file, max).await?;
    Ok((file, begin))
}

/// Browser-wide parameters that forbid all downloads.
fn deny_downloads() -> SetDownloadBehaviorParams {
    SetDownloadBehaviorParams::new(SetDownloadBehaviorBehavior::Deny)
}

/// The caller's `max_bytes`, defaulted and clamped to the download ceiling.
fn download_budget(args: &Value) -> u64 {
    args.get("max_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_DOWNLOAD_MAX_BYTES)
        .clamp(1, DOWNLOAD_MAX_BYTES_CEILING)
}

/// Whether a progress event shows a download past `max` bytes.
///
/// CDP reports byte counts as floats and `total_bytes` is 0 when unknown.
/// Written as a negated "within budget" test so NaN fails closed.
fn download_exceeds_budget(received: f64, total: f64, max: u64) -> bool {
    let max = max as f64;
    !(received <= max && total <= max)
}

/// Verifies the staged download is a regular file of at most `max` bytes,
/// deleting it when it is not so an oversized payload never lingers.
async fn enforce_file_budget(file: &Path, max: u64) -> Result<u64> {
    // symlink_metadata: a link planted in place of the download must not be
    // followed to some other file's size.
    let metadata = tokio::fs::symlink_metadata(file)
        .await
        .context("Downloaded file is missing")?;
    if metadata.is_file() && metadata.len() <= max {
        return Ok(metadata.len());
    }
    let _ = tokio::fs::remove_file(file).await;
    if metadata.is_file() {
        anyhow::bail!("Download exceeded byte budget");
    }
    anyhow::bail!("Download did not produce a regular file")
}

/// Awaits `body`, then always awaits `restore`, returning the body's value
/// only when both succeed.
///
/// A restore failure after a successful body is reported as the error; when
/// both fail the body's error is kept with the restore failure attached.
async fn run_then_restore<T>(
    body: impl Future<Output = Result<T>>,
    restore: impl Future<Output = Result<()>>,
) -> Result<T> {
    let result = body.await;
    let restored = restore.await;
    match (result, restored) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(error.context("Failed to disable browser downloads again")),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(restore)) => Err(error.context(format!(
            "Also failed to disable browser downloads again: {restore:#}"
        ))),
    }
}

fn validate_url(raw: &str) -> Result<url::Url> {
    let url = url::Url::parse(raw)?;
    ensure!(
        (url.scheme() == "http" || url.scheme() == "https") && url.host_str().is_some(),
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
            "Navigate an isolated managed Chromium tab to an HTTP(S) URL.",
            json!({"url":{"type":"string"}}),
            &["url"],
            true,
        ),
        definition(
            "browser_snapshot",
            "Read the Chromium accessibility tree, including backend DOM identifiers.",
            json!({"max_nodes":{"type":"integer","minimum":1,"maximum":2000}}),
            &[],
            false,
        ),
        definition(
            "browser_click",
            "Click a CSS selector in the managed browser.",
            json!({"selector":{"type":"string"}}),
            &["selector"],
            true,
        ),
        definition(
            "browser_type",
            "Type text into a CSS-selected element.",
            json!({"selector":{"type":"string"},"text":{"type":"string"}}),
            &["selector", "text"],
            true,
        ),
        definition(
            "browser_wait",
            "Wait for a CSS selector, bounded by the operation timeout.",
            json!({"selector":{"type":"string"}}),
            &["selector"],
            false,
        ),
        definition(
            "browser_screenshot",
            "Save the current browser viewport as a PNG artifact.",
            json!({}),
            &[],
            false,
        ),
        definition(
            "browser_download",
            "Download an HTTP(S) URL through the browser's authenticated profile; report source URL and artifact.",
            json!({"url":{"type":"string"},"max_bytes":{"type":"integer","minimum":1,"maximum":104857600}}),
            &["url"],
            true,
        ),
        definition(
            "browser_close",
            "Close and reap the managed browser and delete its temporary profile.",
            json!({}),
            &[],
            true,
        ),
    ]
}

/// Detect existing browsers without opening a personal profile or downloading.
fn detect_browser() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    #[cfg(target_os = "macos")]
    {
        for root in [
            Some(PathBuf::from("/Applications")),
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Applications")),
        ]
        .into_iter()
        .flatten()
        {
            for path in [
                "Google Chrome.app/Contents/MacOS/Google Chrome",
                "Chromium.app/Contents/MacOS/Chromium",
                "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            ] {
                candidates.push(root.join(path));
            }
        }
    }
    #[cfg(windows)]
    for key in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
        if let Some(root) = std::env::var_os(key) {
            for path in [
                "Google/Chrome/Application/chrome.exe",
                "Microsoft/Edge/Application/msedge.exe",
                "Chromium/Application/chrome.exe",
            ] {
                candidates.push(PathBuf::from(&root).join(path));
            }
        }
    }
    for root in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        for executable in [
            "chromium",
            "chromium-browser",
            "google-chrome",
            "google-chrome-stable",
            "microsoft-edge",
        ] {
            candidates.push(root.join(executable));
        }
    }
    candidates
        .into_iter()
        .find_map(|path| validate_executable(&path).ok())
}

#[cfg(test)]
mod tests;
