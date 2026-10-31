//! Sandboxed component execution. No WASI linker is installed.

use super::{
    catalog,
    security::{checked, hash, open_workspace_read},
};
use crate::{CapabilityConfig, observation, output_path};
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, VecDeque},
    fs::File,
    io::{Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;
use wasmtime::{
    Config, Engine, Store, StoreLimits, StoreLimitsBuilder,
    component::{
        Component, Linker,
        types::{ComponentItem, Type},
    },
};
use xylo_core::{PluginBudgets, ToolObservation};

#[derive(Clone)]
struct Authority {
    config: CapabilityConfig,
    grants: BTreeSet<String>,
    maximum: usize,
    cancel: CancellationToken,
    lease: Option<Arc<File>>,
    effects: Arc<Effects>,
}

#[derive(Default)]
struct EffectData {
    artifacts: Vec<PathBuf>,
    reserved_bytes: usize,
    pending: usize,
    calls: usize,
    read_bytes: usize,
}

#[derive(Default)]
struct Effects {
    data: Mutex<EffectData>,
    idle: Notify,
}

struct WriteReservation {
    effects: Arc<Effects>,
    bytes: usize,
    committed: bool,
}

impl WriteReservation {
    fn reserve(authority: &Authority, bytes: usize) -> Result<Self> {
        let mut data = authority
            .effects
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("Plugin effect tracker unavailable"))?;
        ensure!(
            data.pending + data.artifacts.len() < 32,
            "Plugin may publish at most 32 artifacts per run"
        );
        ensure!(
            data.reserved_bytes + bytes <= 4 * 1024 * 1024,
            "Plugin aggregate artifact byte budget exceeded"
        );
        data.reserved_bytes += bytes;
        data.pending += 1;
        Ok(Self {
            effects: authority.effects.clone(),
            bytes,
            committed: false,
        })
    }

    fn committed(&mut self, path: PathBuf) -> Result<()> {
        let mut data = self
            .effects
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("Plugin effect tracker unavailable"))?;
        data.artifacts.push(path);
        self.committed = true;
        Ok(())
    }
}

impl Drop for WriteReservation {
    fn drop(&mut self) {
        if let Ok(mut data) = self.effects.data.lock() {
            data.pending = data.pending.saturating_sub(1);
            if !self.committed {
                data.reserved_bytes = data.reserved_bytes.saturating_sub(self.bytes);
            }
        }
        self.effects.idle.notify_waiters();
    }
}

fn host_call(authority: &Authority) -> Result<()> {
    let mut data = authority
        .effects
        .data
        .lock()
        .map_err(|_| anyhow::anyhow!("Plugin effect tracker unavailable"))?;
    ensure!(data.calls < 128, "Plugin host-call budget exceeded");
    data.calls += 1;
    Ok(())
}

fn read_quota(authority: &Authority, bytes: usize) -> Result<()> {
    let mut data = authority
        .effects
        .data
        .lock()
        .map_err(|_| anyhow::anyhow!("Plugin effect tracker unavailable"))?;
    ensure!(
        data.read_bytes + bytes <= 4 * 1024 * 1024,
        "Plugin aggregate read byte budget exceeded"
    );
    data.read_bytes += bytes;
    Ok(())
}

struct PluginState {
    authority: Authority,
    limits: StoreLimits,
}

fn engine() -> Result<Engine> {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    if let Some(engine) = ENGINE.get() {
        return Ok(engine.clone());
    }
    let mut config = Config::new();
    config
        .wasm_component_model(true)
        .consume_fuel(true)
        .max_wasm_stack(512 * 1024);
    let engine = Engine::new(&config)?;
    let _ = ENGINE.set(engine.clone());
    Ok(engine)
}

async fn read_text(authority: Authority, raw: String) -> Result<String> {
    host_call(&authority)?;
    ensure!(
        authority.grants.contains("read_workspace"),
        "read_workspace capability not granted"
    );
    checked(&authority.cancel)?;
    xylo_resources::spawn_blocking(move || {
        // Opening and reading share one authority-bound handle. Retain both
        // native resource and plugin execution leases through actual disk IO.
        let _lease = &authority.lease;
        let mut file = open_workspace_read(&authority.config, &raw, &authority.cancel)?;
        ensure!(
            file.metadata()?.len() <= authority.maximum as u64,
            "Plugin read exceeds byte budget"
        );
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 16 * 1024];
        loop {
            checked(&authority.cancel)?;
            let count = file.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            ensure!(
                bytes.len() + count <= authority.maximum,
                "Plugin read grew beyond its byte budget"
            );
            read_quota(&authority, count)?;
            bytes.extend_from_slice(&chunk[..count]);
        }
        String::from_utf8(bytes).context("Plugin file is not UTF-8 text")
    })
    .await?
}

async fn write_text(authority: Authority, name: String, text: String) -> Result<String> {
    host_call(&authority)?;
    ensure!(
        authority.grants.contains("write_artifacts"),
        "write_artifacts capability not granted"
    );
    ensure!(
        text.len() <= authority.maximum,
        "Plugin write exceeds byte budget"
    );
    let mut reservation = WriteReservation::reserve(&authority, text.len())?;
    // The blocking phase retains the run lease and records a committed artifact
    // even if the async caller has gone away while the filesystem is syncing.
    xylo_resources::spawn_blocking(move || {
        let _lease = &authority.lease;
        checked(&authority.cancel)?;
        let path = output_path(&authority.config, &name, "txt")?;
        let mut staging = tempfile::NamedTempFile::new_in(&authority.config.artifacts)?;
        staging.write_all(text.as_bytes())?;
        staging.as_file().sync_all()?;
        #[cfg(test)]
        super::tests::before_write_commit(&authority.config.artifacts, &authority.cancel)?;
        checked(&authority.cancel)?;
        staging
            .persist_noclobber(&path)
            .map_err(|error| error.error)?;
        reservation.committed(path.clone())?;
        #[cfg(unix)]
        File::open(&authority.config.artifacts)?.sync_all()?;
        Ok(path.to_string_lossy().into_owned())
    })
    .await?
}

fn approved_url(authority: &Authority, raw: &str) -> Result<url::Url> {
    let url = url::Url::parse(raw)?;
    ensure!(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port_or_known_default() == Some(443)
            && url.fragment().is_none(),
        "Plugin requests require credential-free HTTPS on port 443"
    );
    let host = url.host_str().context("HTTPS request has no host")?;
    ensure!(
        authority.grants.contains(&format!("https:{host}")),
        "HTTPS host capability not granted"
    );
    Ok(url)
}

async fn http_get(authority: Authority, raw: String) -> Result<String> {
    host_call(&authority)?;
    checked(&authority.cancel)?;
    let url = approved_url(&authority, &raw)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()?;
    let response = tokio::select! {
        response = client.get(url).send() => response?.error_for_status()?,
        _ = authority.cancel.cancelled() => anyhow::bail!("Plugin operation cancelled"),
    };
    ensure!(
        !response.status().is_redirection(),
        "Plugin redirects are not followed"
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= authority.maximum as u64),
        "Plugin HTTP body exceeds byte budget"
    );
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    loop {
        let chunk = tokio::select! {
            chunk = stream.next() => chunk,
            _ = authority.cancel.cancelled() => anyhow::bail!("Plugin operation cancelled"),
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk?;
        ensure!(
            bytes.len() + chunk.len() <= authority.maximum,
            "Plugin HTTP body exceeds byte budget"
        );
        read_quota(&authority, chunk.len())?;
        bytes.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8(bytes)?)
}

fn linker(engine: &Engine) -> Result<Linker<PluginState>> {
    let mut linker: Linker<PluginState> = Linker::new(engine);
    linker
        .root()
        .func_wrap_async("read-text", |ctx, (path,): (String,)| {
            let authority = ctx.data().authority.clone();
            Box::new(async move {
                Ok((read_text(authority, path)
                    .await
                    .map_err(|error| error.to_string()),))
            })
        })?;
    linker
        .root()
        .func_wrap_async("write-text", |ctx, (name, text): (String, String)| {
            let authority = ctx.data().authority.clone();
            Box::new(async move {
                Ok((write_text(authority, name, text)
                    .await
                    .map_err(|error| error.to_string()),))
            })
        })?;
    linker
        .root()
        .func_wrap_async("http-get", |ctx, (url,): (String,)| {
            let authority = ctx.data().authority.clone();
            Box::new(async move {
                Ok((http_get(authority, url)
                    .await
                    .map_err(|error| error.to_string()),))
            })
        })?;
    Ok(linker)
}

fn preflight(bytes: &[u8], cancel: &CancellationToken) -> Result<Vec<u8>> {
    checked(cancel)?;
    let binary = wat::parse_bytes(bytes)?.into_owned();
    ensure!(
        binary.len() <= 32 * 1024 * 1024,
        "Expanded plugin component exceeds 32MiB"
    );
    let mut functions = 0usize;
    let mut instructions = 0usize;
    let mut depth = 0usize;
    for payload in wasmtime::wasmparser::Parser::new(0).parse_all(&binary) {
        checked(cancel)?;
        match payload? {
            wasmtime::wasmparser::Payload::Version { .. } => {
                depth += 1;
                ensure!(depth <= 32, "Plugin component nesting exceeds its budget");
            }
            wasmtime::wasmparser::Payload::End(_) => {
                depth = depth.saturating_sub(1);
            }
            wasmtime::wasmparser::Payload::CodeSectionEntry(body) => {
                functions += 1;
                ensure!(functions <= 10_000, "Plugin has too many functions");
                let mut operators = body.get_operators_reader()?;
                while !operators.eof() {
                    operators.read()?;
                    instructions += 1;
                    ensure!(
                        instructions <= 1_000_000,
                        "Plugin instruction count exceeds its compilation budget"
                    );
                    if instructions.is_multiple_of(1024) {
                        checked(cancel)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(binary)
}

fn validate_interface(engine: &Engine, component: &Component) -> Result<()> {
    linker(engine)?.instantiate_pre(component).map_err(|error| anyhow::anyhow!(error.to_string())).context("Plugin requests unsupported imports; only read-text, write-text and http-get are available")?;
    let ty = component.component_type();
    let export = ty
        .get_export(engine, "run")
        .context("Plugin must export run(input: string) -> result<string,string>")?;
    let ComponentItem::ComponentFunc(function) = export.ty else {
        anyhow::bail!("Plugin run export must be a function");
    };
    let parameters: Vec<_> = function.params().collect();
    let results: Vec<_> = function.results().collect();
    ensure!(
        parameters.len() == 1 && parameters[0].1 == Type::String && results.len() == 1,
        "Plugin run signature must accept one string and return one result"
    );
    let Type::Result(result) = &results[0] else {
        anyhow::bail!("Plugin run must return result<string,string>");
    };
    ensure!(
        result.ok() == Some(Type::String) && result.err() == Some(Type::String),
        "Plugin run must return result<string,string>"
    );
    Ok(())
}

pub(super) async fn compile(
    bytes: Vec<u8>,
    root: PathBuf,
    lease: Option<Arc<File>>,
    cancel: CancellationToken,
) -> Result<Component> {
    static ADMISSION: OnceLock<Arc<Semaphore>> = OnceLock::new();
    static CACHE: OnceLock<Mutex<VecDeque<(String, Component)>>> = OnceLock::new();
    let key = hash(&bytes);
    let cache = CACHE.get_or_init(|| Mutex::new(VecDeque::new()));
    if let Some(component) = cache
        .lock()
        .map_err(|_| anyhow::anyhow!("Plugin compile cache unavailable"))?
        .iter()
        .find(|(hash, _)| hash == &key)
        .map(|(_, component)| component.clone())
    {
        checked(&cancel)?;
        return Ok(component);
    }
    let admission = ADMISSION
        .get_or_init(|| Arc::new(Semaphore::new(1)))
        .clone();
    let permit = tokio::select! {
        permit = admission.acquire_owned() => permit?,
        _ = cancel.cancelled() => anyhow::bail!("Plugin compilation cancelled"),
    };
    xylo_resources::spawn_blocking(move || {
        let _permit = permit;
        let _lease = lease;
        let _compile = catalog::lock(&root.join("compile.lock"), false, true, &cancel)?;
        let binary = preflight(&bytes, &cancel)?;
        let engine = engine()?;
        let component = Component::new(&engine, binary)?;
        checked(&cancel)?;
        validate_interface(&engine, &component)?;
        let mut cache = CACHE
            .get()
            .unwrap()
            .lock()
            .map_err(|_| anyhow::anyhow!("Plugin compile cache unavailable"))?;
        cache.push_front((key, component.clone()));
        cache.truncate(2);
        Ok(component)
    })
    .await?
}

pub(super) struct Invocation {
    pub config: CapabilityConfig,
    pub module: Vec<u8>,
    pub input: String,
    pub grants: BTreeSet<String>,
    pub budgets: PluginBudgets,
    pub root: PathBuf,
    pub lease: Option<Arc<File>>,
    pub identity: Value,
}

pub(super) async fn run(
    invocation: Invocation,
    cancel: CancellationToken,
) -> Result<ToolObservation> {
    let Invocation {
        config,
        module,
        input,
        grants,
        budgets,
        root,
        lease,
        identity,
    } = invocation;
    ensure!(
        input.len() as u64 <= budgets.max_input_bytes,
        "Plugin input exceeds its byte budget"
    );
    // JSON escaping can expand each output byte sixfold. Reserve enough envelope
    // room so CapabilityService never truncates an installed plugin JSON result.
    let maximum = usize::try_from(budgets.max_output_bytes)?
        .min(config.max_output_bytes.saturating_sub(2048) / 6);
    ensure!(maximum > 0, "Plugin response budget is too small");
    let child = cancel.child_token();
    let _cancel_on_drop = child.clone().drop_guard();
    let effects = Arc::new(Effects::default());
    let authority = Authority {
        config: config.clone(),
        grants,
        maximum,
        cancel: child.clone(),
        lease: lease.clone(),
        effects: effects.clone(),
    };
    let memory = budgets.memory_mib as usize * 1024 * 1024;
    let task = async {
        let component = compile(module, root, lease, child.clone()).await?;
        checked(&child)?;
        let engine = engine()?;
        let mut store = Store::new(
            &engine,
            PluginState {
                authority,
                limits: StoreLimitsBuilder::new()
                    .memory_size(memory)
                    .table_elements(10_000)
                    .instances(16)
                    .memories(1)
                    .tables(4)
                    .build(),
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_fuel(budgets.fuel)?;
        store.fuel_async_yield_interval(Some(10_000))?;
        let instance = linker(&engine)?
            .instantiate_async(&mut store, &component)
            .await?;
        let function = instance
            .get_typed_func::<(String,), (std::result::Result<String, String>,)>(
                &mut store, "run",
            )?;
        let (output,) = function.call_async(&mut store, (input,)).await?;
        let bytes = match &output {
            Ok(output) | Err(output) => output.len(),
        };
        ensure!(bytes <= maximum, "Plugin result exceeds its byte budget");
        let output = output.map_err(anyhow::Error::msg)?;
        Ok::<_, anyhow::Error>(
            json!({"output": output, "fuel_remaining": store.get_fuel()?, "memory_budget_bytes": memory, "plugin": identity}),
        )
    };
    let result = tokio::select! {
        result = task => result,
        _ = cancel.cancelled() => Err(anyhow::anyhow!("Plugin cancelled")),
        _ = tokio::time::sleep(Duration::from_secs(budgets.timeout_seconds)) => Err(anyhow::anyhow!("Plugin exceeded wall-clock budget")),
    };
    child.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let notified = effects.idle.notified();
            if effects
                .data
                .lock()
                .map(|data| data.pending == 0)
                .unwrap_or(true)
            {
                break;
            }
            notified.await;
        }
    })
    .await;
    let data = effects
        .data
        .lock()
        .map_err(|_| anyhow::anyhow!("Plugin effect tracker unavailable"))?;
    let artifacts = data.artifacts.clone();
    if data.pending > 0 {
        let mut observation = observation(
            json!({"error":"Plugin stopped while an artifact write was completing; inspect artifacts before retrying", "side_effects":"outcome_unknown"}),
            artifacts,
        );
        observation.status = "outcome_unknown".to_owned();
        return Ok(observation);
    }
    drop(data);
    match result {
        Ok(output) => {
            let result = observation(output, artifacts);
            ensure!(
                result.summary.len() <= config.max_output_bytes,
                "Plugin result exceeds the serialized response budget"
            );
            Ok(result)
        }
        Err(error) if artifacts.is_empty() => Err(anyhow::anyhow!(bounded_error(
            &error,
            config.max_output_bytes
        ))),
        Err(error) => {
            let mut result = observation(
                json!({"error": bounded_error(&error, config.max_output_bytes), "side_effects": "artifacts_committed"}),
                artifacts,
            );
            result.status = "failed".to_owned();
            Ok(result)
        }
    }
}

fn bounded_error(error: &anyhow::Error, output_budget: usize) -> String {
    let mut message = error.to_string();
    let maximum = 2048.min(output_budget.saturating_sub(1024) / 6);
    if message.len() > maximum {
        let mut end = maximum;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    message
}

#[cfg(test)]
pub(super) fn validate_url_for_test(
    config: CapabilityConfig,
    grants: &[&str],
    url: &str,
) -> Result<()> {
    let authority = Authority {
        config,
        grants: grants.iter().map(|value| (*value).to_owned()).collect(),
        maximum: 1024,
        cancel: CancellationToken::new(),
        lease: None,
        effects: Arc::new(Effects::default()),
    };
    approved_url(&authority, url).map(|_| ())
}
