//! Private size/day rolling logs and opt-in OTLP HTTP/JSON telemetry. Exported spans carry
//! operation names and timing only; prompts, arguments, results and secrets are
//! never attached. Logs roll at 10 MiB and are retained for seven UTC calendar
//! days, with a 100 MiB total cap.

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tracing::{
    Subscriber,
    span::{Attributes, Id},
};
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;
use xylo_core::EngineEvent;

const DAY: u64 = 86_400;
const RETENTION_DAYS: u64 = 7;
const LOG_BYTES: u64 = 10 * 1024 * 1024;
const TOTAL_LOG_BYTES: u64 = 100 * 1024 * 1024;

struct LogFile {
    directory: PathBuf,
    day: u64,
    segment: u64,
    size: u64,
    file: File,
}

impl LogFile {
    fn open(directory: PathBuf) -> Result<Self> {
        Self::open_at(directory, unix_seconds() / DAY)
    }

    fn open_at(directory: PathBuf, day: u64) -> Result<Self> {
        std::fs::create_dir_all(&directory)?;
        let segment = owned_logs(&directory)?
            .into_iter()
            .filter(|entry| entry.day == day)
            .map(|entry| entry.segment)
            .max()
            .unwrap_or(0);
        let file = open_log(&directory, day, segment)?;
        let size = file.metadata()?.len();
        let mut log = Self {
            directory,
            day,
            segment,
            size,
            file,
        };
        if log.size >= LOG_BYTES {
            log.roll()?;
        }
        clean_logs_except(&log.directory, day, TOTAL_LOG_BYTES, Some(&log.path()))?;
        Ok(log)
    }

    fn path(&self) -> PathBuf {
        log_path(&self.directory, self.day, self.segment)
    }

    fn roll(&mut self) -> Result<()> {
        let segment = self
            .segment
            .checked_add(1)
            .context("Log segment overflow")?;
        let file = open_log(&self.directory, self.day, segment)?;
        self.size = file.metadata()?.len();
        self.file = file;
        self.segment = segment;
        Ok(())
    }

    fn write_at(&mut self, bytes: &[u8], day: u64) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if day != self.day {
            *self = Self::open_at(self.directory.clone(), day).map_err(std::io::Error::other)?;
        }
        while self.size >= LOG_BYTES
            || (self.size > 0
                && bytes.len() as u64 <= LOG_BYTES
                && bytes.len() as u64 > LOG_BYTES - self.size)
        {
            self.roll().map_err(std::io::Error::other)?;
        }
        // Keep ordinary diagnostics intact when they fit in a fresh segment.
        // Write returns a partial count for an oversized chunk. write_all then
        // continues in a new segment, so even one huge event cannot exceed it.
        let count = bytes.len().min((LOG_BYTES - self.size) as usize);
        clean_logs_except(
            &self.directory,
            day,
            TOTAL_LOG_BYTES - count as u64,
            Some(&self.path()),
        )
        .map_err(std::io::Error::other)?;
        let written = self.file.write(&bytes[..count])?;
        self.size += written as u64;
        Ok(written)
    }
}

fn log_path(directory: &Path, day: u64, segment: u64) -> PathBuf {
    directory.join(if segment == 0 {
        format!("xylo-{day}.log")
    } else {
        format!("xylo-{day}-{segment}.log")
    })
}

fn open_log(directory: &Path, day: u64, segment: u64) -> Result<File> {
    let path = log_path(directory, day, segment);
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        ensure!(
            metadata.file_type().is_file(),
            "Log path is not a regular file"
        );
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    ensure!(file.metadata()?.is_file(), "Log path is not a regular file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            file.metadata()?.nlink() == 1,
            "Log path has multiple hard links"
        );
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

struct OwnedLog {
    path: PathBuf,
    day: u64,
    segment: u64,
    size: u64,
}

fn owned_logs(directory: &Path) -> Result<Vec<OwnedLog>> {
    let mut logs = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(stem) = name
            .to_str()
            .and_then(|name| name.strip_prefix("xylo-"))
            .and_then(|name| name.strip_suffix(".log"))
        else {
            continue;
        };
        let (day, segment) = stem.split_once('-').unwrap_or((stem, "0"));
        let canonical_number = |value: &str| {
            value
                .parse::<u64>()
                .ok()
                .filter(|number| number.to_string() == value)
        };
        let (Some(day), Some(segment)) = (canonical_number(day), canonical_number(segment)) else {
            continue;
        };
        // symlink_metadata never follows a link into another file or directory.
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if !metadata.is_file() {
            continue;
        }
        logs.push(OwnedLog {
            path: entry.path(),
            day,
            segment,
            size: metadata.len(),
        });
    }
    logs.sort_by(|a, b| (a.day, a.segment, &a.path).cmp(&(b.day, b.segment, &b.path)));
    Ok(logs)
}

fn clean_logs_except(
    directory: &Path,
    today: u64,
    budget: u64,
    active: Option<&Path>,
) -> Result<()> {
    let logs = owned_logs(directory)?;
    let mut total = logs
        .iter()
        .fold(0_u64, |sum, log| sum.saturating_add(log.size));
    for log in logs {
        if active == Some(log.path.as_path()) {
            continue;
        }
        if log.day.saturating_add(RETENTION_DAYS) <= today || total > budget {
            std::fs::remove_file(log.path)?;
            total = total.saturating_sub(log.size);
        }
    }
    Ok(())
}

#[derive(Clone)]
struct LogWriter(Arc<Mutex<LogFile>>);
impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("Log lock poisoned"))?
            .write_at(bytes, unix_seconds() / DAY)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("Log lock poisoned"))?
            .file
            .flush()
    }
}

#[derive(Clone)]
struct Exporter {
    sender: mpsc::SyncSender<Export>,
}
struct Export {
    signal: &'static str,
    value: Value,
}

struct SpanClock {
    started: Instant,
    unix_start: u64,
    trace: String,
    span: String,
    parent: String,
    name: &'static str,
    status: u8,
}
struct TelemetryLayer {
    exporter: Option<Exporter>,
    spans: Mutex<HashMap<Id, SpanClock>>,
}
impl<S: Subscriber> Layer<S> for TelemetryLayer {
    fn on_new_span(
        &self,
        attrs: &Attributes<'_>,
        id: &Id,
        context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if self.exporter.is_none() {
            return;
        }
        let mut spans = self.spans.lock().unwrap_or_else(|error| error.into_inner());
        let parent = attrs
            .parent()
            .cloned()
            .or_else(|| context.current_span().id().cloned());
        let prior = parent.as_ref().and_then(|id| spans.get(id));
        let trace = prior.map_or_else(
            || Uuid::new_v4().simple().to_string(),
            |prior| prior.trace.clone(),
        );
        let parent = prior.map_or_else(String::new, |prior| prior.span.clone());
        spans.insert(
            id.clone(),
            SpanClock {
                started: Instant::now(),
                unix_start: unix_nanos(),
                trace,
                span: Uuid::new_v4().simple().to_string()[..16].into(),
                parent,
                name: attrs.metadata().name(),
                status: 0,
            },
        );
    }
    fn on_close(&self, id: Id, _: tracing_subscriber::layer::Context<'_, S>) {
        if let Some(clock) = self
            .spans
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&id)
            && let Some(exporter) = &self.exporter
        {
            exporter.span(&clock, clock.started.elapsed());
        }
    }
}

static METRICS: OnceLock<Weak<MetricState>> = OnceLock::new();

/// Observes worker events in the desktop host without exposing their content.
pub fn observe(session: Uuid, turn: Uuid, event: &EngineEvent) {
    if let Some(state) = METRICS.get().and_then(Weak::upgrade) {
        state.event(session, turn, event);
    }
}

struct MetricState {
    exporter: Option<Exporter>,
    input_rate: Option<f64>,
    output_rate: Option<f64>,
    tools: Mutex<HashMap<Uuid, (Instant, u64)>>,
    turns: Mutex<HashMap<(Uuid, Uuid), (Instant, u64)>>,
}

/// Owns exporter shutdown. Keep alive for the whole application lifetime.
pub struct Observability {
    exporter: Option<Exporter>,
    worker: Option<std::thread::JoinHandle<()>>,
    state: Arc<MetricState>,
}

impl Observability {
    /// Initializes logs and optional export before starting services.
    /// Set OTEL_TRACES_EXPORTER=otlp and/or OTEL_METRICS_EXPORTER=otlp to opt in.
    pub fn init(data: &Path, debug: bool) -> Result<Self> {
        let logs = LogWriter(Arc::new(Mutex::new(LogFile::open(data.join("logs"))?)));
        let (exporter, worker) = start_exporter()?;
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| if debug { "debug" } else { "info" }.into());
        let writer = logs.clone();
        tracing_subscriber::registry()
            .with(filter)
            .with(TelemetryLayer {
                exporter: exporter.clone(),
                spans: Mutex::default(),
            })
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(move || writer.clone()),
            )
            .try_init()
            .context("Initialize application logging")?;
        let state = Arc::new(MetricState {
            exporter: exporter.clone(),
            input_rate: rate("XYLO_OTEL_INPUT_USD_PER_MILLION")?,
            output_rate: rate("XYLO_OTEL_OUTPUT_USD_PER_MILLION")?,
            tools: Mutex::default(),
            turns: Mutex::default(),
        });
        let _ = METRICS.set(Arc::downgrade(&state));
        Ok(Self {
            exporter,
            worker,
            state,
        })
    }

    /// Observes operation timing and aggregate usage without exporting content.
    pub fn event(&self, session: Uuid, turn: Uuid, event: &EngineEvent) {
        self.state.event(session, turn, event);
    }
}

impl MetricState {
    fn event(&self, session: Uuid, turn: Uuid, event: &EngineEvent) {
        if let EngineEvent::Error {
            kind, retryable, ..
        } = event
        {
            tracing::error!(%session, %turn, ?kind, retryable, "Turn failed");
        }
        let Some(exporter) = &self.exporter else {
            return;
        };
        if matches!(
            event,
            EngineEvent::Phase {
                phase: xylo_core::TurnPhase::Preparing
            } | EngineEvent::Started
        ) {
            self.turns
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .entry((session, turn))
                .or_insert_with(|| (Instant::now(), unix_nanos()));
        }
        if matches!(
            event,
            EngineEvent::Completed { .. } | EngineEvent::Cancelled | EngineEvent::Error { .. }
        ) && let Some((start, unix_start)) = self
            .turns
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&(session, turn))
        {
            exporter.span(
                &SpanClock {
                    started: start,
                    unix_start,
                    trace: session.simple().to_string(),
                    span: turn.simple().to_string()[..16].into(),
                    parent: String::new(),
                    name: "conversation.run",
                    status: if matches!(event, EngineEvent::Completed { .. }) {
                        1
                    } else {
                        2
                    },
                },
                start.elapsed(),
            );
        }
        match event {
            EngineEvent::ToolStarted { action_id, .. } => {
                self.tools
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .insert(*action_id, (Instant::now(), unix_nanos()));
            }
            EngineEvent::ToolFinished {
                action_id,
                name,
                success,
                ..
            } => {
                if let Some((start, unix_start)) = self
                    .tools
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(action_id)
                {
                    exporter.metric(
                        "xylo.tool.duration",
                        "s",
                        start.elapsed().as_secs_f64(),
                        vec![attribute("tool", name)],
                    );
                    exporter.span(
                        &SpanClock {
                            started: start,
                            unix_start,
                            trace: session.simple().to_string(),
                            span: action_id.simple().to_string()[..16].into(),
                            parent: turn.simple().to_string()[..16].into(),
                            name: "tool.execute",
                            status: if *success { 1 } else { 2 },
                        },
                        start.elapsed(),
                    );
                }
            }
            EngineEvent::Completed { usage, .. } => {
                exporter.metric(
                    "xylo.tokens",
                    "{token}",
                    usage.input_tokens as f64,
                    vec![attribute("direction", "input")],
                );
                exporter.metric(
                    "xylo.tokens",
                    "{token}",
                    usage.output_tokens as f64,
                    vec![attribute("direction", "output")],
                );
                if let (Some(input), Some(output)) = (self.input_rate, self.output_rate) {
                    exporter.metric(
                        "xylo.cost",
                        "USD",
                        (usage.input_tokens as f64 * input + usage.output_tokens as f64 * output)
                            / 1_000_000.0,
                        vec![attribute("kind", "estimate")],
                    );
                }
            }
            _ => {}
        }
    }
}
impl Drop for Observability {
    fn drop(&mut self) {
        // A stop message avoids waiting for tracing's global layer to release its sender.
        if let Some(exporter) = self.exporter.take() {
            let _ = exporter.sender.try_send(Export {
                signal: "shutdown",
                value: Value::Null,
            });
        }
        if let Some(worker) = self.worker.take() {
            // Collector failures must not hold application shutdown indefinitely.
            let deadline = Instant::now() + Duration::from_secs(5);
            while !worker.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
    }
}

impl Exporter {
    fn send(&self, signal: &'static str, value: Value) {
        let _ = self.sender.try_send(Export { signal, value });
    }
    fn span(&self, clock: &SpanClock, elapsed: Duration) {
        self.send("traces", json!({"resourceSpans":[{"resource":resource(),"scopeSpans":[{"scope":{"name":"xylo"},"spans":[{"traceId":clock.trace,"spanId":clock.span,"parentSpanId":clock.parent,"name":clock.name,"kind":1,"startTimeUnixNano":clock.unix_start.to_string(),"endTimeUnixNano":clock.unix_start.saturating_add(elapsed.as_nanos().min(u64::MAX as u128) as u64).to_string(),"status":{"code":clock.status}}]}]}]}));
    }
    fn metric(&self, name: &str, unit: &str, value: f64, attributes: Vec<Value>) {
        self.send("metrics", json!({"resourceMetrics":[{"resource":resource(),"scopeMetrics":[{"scope":{"name":"xylo"},"metrics":[{"name":name,"unit":unit,"sum":{"aggregationTemporality":1,"isMonotonic":true,"dataPoints":[{"timeUnixNano":unix_nanos().to_string(),"asDouble":value,"attributes":attributes}]}}]}]}]}));
    }
}

fn attribute(key: &str, value: &str) -> Value {
    json!({"key":key,"value":{"stringValue":value}})
}
fn resource() -> Value {
    let service = std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "xylo".into());
    let mut attributes = vec![
        attribute("service.name", &service),
        attribute("service.version", env!("CARGO_PKG_VERSION")),
    ];
    if let Ok(values) = std::env::var("OTEL_RESOURCE_ATTRIBUTES") {
        for (key, value) in values.split(',').filter_map(|pair| pair.split_once('=')) {
            attributes.push(attribute(key.trim(), value.trim()));
        }
    }
    json!({"attributes":attributes})
}
fn rate(name: &str) -> Result<Option<f64>> {
    std::env::var(name)
        .ok()
        .map(|value| {
            let rate = value.parse::<f64>()?;
            ensure!(
                rate.is_finite() && rate >= 0.0,
                "{name} must be a nonnegative finite number"
            );
            Ok(rate)
        })
        .transpose()
}
fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

fn start_exporter() -> Result<(Option<Exporter>, Option<std::thread::JoinHandle<()>>)> {
    if std::env::var("OTEL_SDK_DISABLED").is_ok_and(|value| value.eq_ignore_ascii_case("true")) {
        return Ok((None, None));
    }
    let signals: Vec<_> = ["traces", "metrics"]
        .into_iter()
        .filter(|signal| {
            std::env::var(format!("OTEL_{}_EXPORTER", signal.to_uppercase()))
                .is_ok_and(|value| value.split(',').any(|exporter| exporter == "otlp"))
        })
        .collect();
    if signals.is_empty() {
        return Ok((None, None));
    }
    let mut endpoints = HashMap::new();
    for signal in signals {
        let prefix = format!("OTEL_EXPORTER_OTLP_{}", signal.to_uppercase());
        let protocol = std::env::var(format!("{prefix}_PROTOCOL"))
            .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL"))
            .unwrap_or_else(|_| "http/json".into());
        ensure!(
            protocol == "http/json",
            "Xylo OTLP requires OTEL_EXPORTER_OTLP_PROTOCOL=http/json"
        );
        let endpoint = std::env::var(format!("{prefix}_ENDPOINT")).unwrap_or_else(|_| {
            format!(
                "{}/v1/{signal}",
                std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
                    .unwrap_or_else(|_| "http://localhost:4318".into())
                    .trim_end_matches('/')
            )
        });
        let endpoint = reqwest::Url::parse(&endpoint).context("Invalid OTLP endpoint")?;
        ensure!(
            matches!(endpoint.scheme(), "http" | "https"),
            "OTLP endpoint must use HTTP or HTTPS"
        );
        let headers = std::env::var(format!("{prefix}_HEADERS"))
            .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_HEADERS"))
            .unwrap_or_default();
        let mut parsed = reqwest::header::HeaderMap::new();
        for (key, value) in headers
            .split(',')
            .filter(|header| !header.is_empty())
            .map(|header| header.split_once('=').context("Invalid OTLP header"))
            .collect::<Result<Vec<_>>>()?
        {
            let encoded = format!("{}={}", key.trim(), value.trim()).replace('+', "%2B");
            let (key, value) = url::form_urlencoded::parse(encoded.as_bytes())
                .next()
                .context("Invalid OTLP header")?;
            parsed.insert(
                reqwest::header::HeaderName::from_bytes(key.as_bytes())
                    .context("Invalid OTLP header name")?,
                reqwest::header::HeaderValue::from_str(&value)
                    .context("Invalid OTLP header value")?,
            );
        }
        let timeout = std::env::var(format!("{prefix}_TIMEOUT"))
            .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_TIMEOUT"))
            .ok()
            .map(|value| value.parse::<u64>())
            .transpose()?
            .unwrap_or(5000)
            .clamp(1, 30_000);
        endpoints.insert(signal, (endpoint, parsed, timeout));
    }
    let (sender, receiver) = mpsc::sync_channel::<Export>(128);
    let worker = std::thread::Builder::new()
        .name("xylo-otlp".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async move {
                let client = reqwest::Client::new();
                while let Ok(export) = receiver.recv() {
                    if export.signal == "shutdown" {
                        break;
                    }
                    if let Some((endpoint, headers, timeout)) = endpoints.get(export.signal) {
                        // Export errors carry no collector response, URL or credentials into logs.
                        let _ = client
                            .post(endpoint.clone())
                            .headers(headers.clone())
                            .timeout(Duration::from_millis(*timeout))
                            .json(&export.value)
                            .send()
                            .await;
                    }
                }
            });
        })?;
    Ok((Some(Exporter { sender }), Some(worker)))
}

#[cfg(test)]
#[path = "observability/tests.rs"]
mod tests;
