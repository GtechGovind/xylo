//! Offline installed-component overhead, including catalog and package checks.
//!
//! Build with the pinned release graph, then run without a concurrent compiler:
//! CARGO_TARGET_DIR="$PWD/target/performance" cargo build --locked --release \
//!   -p xylo-capabilities --example measure_plugins
//! target/performance/release/examples/measure_plugins --output /absolute/run.json \
//!   --source-fingerprint SHA256
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_capabilities::{CapabilityConfig, CapabilityService};
use xylo_core::{
    NativePluginManifest, PluginBudgets, PluginCatalog, PluginInspection, PluginManagementResult,
    now_ms,
};

const SAMPLES: usize = 80;
const EMBEDDED_LOCK: &[u8] = include_bytes!("../../../Cargo.lock");
const EMBEDDED_HARNESS: &[u8] = include_bytes!("measure_plugins.rs");
const ECHO: &str = r#"(component
  (core module $module
    (memory (export "memory") 1)
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 1024)
    (func (export "run") (param $p i32) (param $n i32) (result i32)
      i32.const 16 i32.const 0 i32.store
      i32.const 20 local.get $p i32.store
      i32.const 24 local.get $n i32.store
      i32.const 16))
  (core instance $instance (instantiate $module))
  (func (export "run") (param "input" string) (result (result string (error string)))
    (canon lift (core func $instance "run")
      (memory (core memory $instance "memory")) (realloc (core func $instance "realloc")))))"#;

fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn file_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}
fn arguments() -> Result<(PathBuf, String)> {
    let mut output = None;
    let mut fingerprint = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .with_context(|| format!("{flag} requires a value"))?;
        match flag.as_str() {
            "--output" if output.is_none() => output = Some(PathBuf::from(value)),
            "--source-fingerprint" if fingerprint.is_none() => fingerprint = Some(value),
            _ => anyhow::bail!("Unknown or duplicate argument: {flag}"),
        }
    }
    let output = output.context("--output PATH is required")?;
    ensure!(output.is_absolute(), "Evidence path must be absolute");
    ensure!(
        !output.exists(),
        "Evidence already exists; choose a new path"
    );
    let fingerprint = fingerprint.context("--source-fingerprint SHA256 is required")?;
    ensure!(
        fingerprint.len() == 64 && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Source fingerprint must match the reviewed source manifest"
    );
    Ok((output, fingerprint))
}
fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.
}
fn statistics(samples: &[Value]) -> Value {
    let mut sorted: Vec<_> = samples
        .iter()
        .map(|sample| sample["native_ms"].as_f64().unwrap())
        .collect();
    sorted.sort_by(f64::total_cmp);
    let rank = |fraction: f64| sorted[(sorted.len() as f64 * fraction).ceil() as usize - 1];
    json!({"unit":"ms","count":sorted.len(),"p50":rank(0.5),"p95":rank(0.95),
        "min":sorted[0],"max":sorted[sorted.len()-1],"raw":samples})
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let (output, fingerprint) = arguments()?;
    let started = Instant::now();
    let cancel = CancellationToken::new();
    let deadline_cancel = cancel.clone();
    let deadline = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(55)).await;
        deadline_cancel.cancel();
    });
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let workspace = root.join("project");
    std::fs::create_dir(&workspace)?;
    let module = wat::parse_str(ECHO)?;
    std::fs::write(workspace.join("echo.wasm"), &module)?;
    let manifest = NativePluginManifest {
        format: 1,
        id: "benchmark_echo".into(),
        name: "Benchmark echo".into(),
        version: "1.0.0".into(),
        entrypoint: "echo.wasm".into(),
        sha256: hash(&module),
        capabilities: Vec::new(),
        budgets: PluginBudgets::default(),
    };
    let manifest_path = workspace.join("plugin.json");
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest)?)?;
    let construction = Instant::now();
    let service = CapabilityService::new(CapabilityConfig {
        workspace,
        artifacts: root.join("artifacts"),
        management_root: Some(root.join("native-management")),
        ..Default::default()
    })?;
    let construction_ms = elapsed_ms(construction);
    let inspect_start = Instant::now();
    let inspection = service
        .execute_control(
            "helper_plugin_inspect",
            &json!({"manifest_path":manifest_path}),
            cancel.clone(),
        )
        .await?;
    let inspect_ms = elapsed_ms(inspect_start);
    let inspection: PluginInspection = serde_json::from_value(inspection)?;
    ensure!(inspection.manifest == manifest, "Inspected package changed");
    let catalog: PluginCatalog = serde_json::from_value(
        service
            .execute_control("helper_plugin_list", &json!({}), cancel.clone())
            .await?,
    )?;
    let install_start = Instant::now();
    let installed = service
        .execute_control(
            "helper_plugin_install",
            &json!({
                "operation_id":Uuid::new_v4(),"expected_revision":catalog.revision,
                "manifest_path":manifest_path,"expected_package_sha256":inspection.package_sha256,
                "grants":[],"enabled":true,"replace":false,
            }),
            cancel.clone(),
        )
        .await?;
    let install_ms = elapsed_ms(install_start);
    let installed: PluginManagementResult = serde_json::from_value(installed)?;
    ensure!(
        installed.catalog.plugins.len() == 1,
        "Package was not installed"
    );
    let mut samples = Vec::with_capacity(SAMPLES);
    let mut first_run = Value::Null;
    for index in 0..=SAMPLES {
        let input = format!("Native installed plugin request {index}: café 🪶");
        let args =
            json!({"id":manifest.id,"package_sha256":inspection.package_sha256,"input":input});
        let timer = Instant::now();
        let observation = service
            .execute("plugin_run_installed", &args, cancel.clone())
            .await?;
        let native_ms = elapsed_ms(timer);
        let decoded: Value = serde_json::from_str(&observation.summary)?;
        ensure!(
            decoded["output"] == input,
            "Guest returned a wrong or reused result"
        );
        ensure!(
            observation.artifacts.is_empty(),
            "Read-only echo produced unexpected files"
        );
        let sample = json!({"index":index,"native_ms":native_ms,"input_bytes":input.len(),
            "response_bytes":observation.summary.len()});
        if index == 0 {
            first_run = sample;
        } else {
            samples.push(sample);
        }
    }
    ensure!(!cancel.is_cancelled(), "Measurement deadline reached");
    service.shutdown().await?;
    deadline.abort();
    let mut system = sysinfo::System::new_all().expect("system monitor");
    system.refresh_memory();
    let load = sysinfo::System::load_average().expect("load average");
    let report = json!({
        "schema_version":1,"measured_at_ms":now_ms(),"source_fingerprint":fingerprint,
        "source_fingerprint_origin":"Reviewed source manifest supplied by caller; not regenerated by harness",
        "harness_binary_sha256":file_hash(&std::env::current_exe()?)?,
        "compiled_harness_source_sha256":hash(EMBEDDED_HARNESS),
        "compiled_workspace_cargo_lock_sha256":hash(EMBEDDED_LOCK),
        "profile":if cfg!(debug_assertions){"debug"}else{"release"},
        "hardware":{"os":std::env::consts::OS,"os_version":sysinfo::System::long_os_version().ok(),
            "architecture":std::env::consts::ARCH,"cpu_threads":std::thread::available_parallelism()?.get(),
            "cpu_brand":system.cpus().first().map(|cpu|cpu.brand()),"total_memory_bytes":system.total_memory(),
            "load_average":{"one":load.one,"five":load.five,"fifteen":load.fifteen}},
        "fixture":{"kind":"Wasm component-model echo","module_bytes":module.len(),
            "module_sha256":manifest.sha256,"package_sha256":inspection.package_sha256,
            "requested_permissions":[],"budgets":manifest.budgets},
        "preparation":{"service_construction_ms":construction_ms,"inspection_ms":inspect_ms,
            "installation_ms":install_ms,"first_run_after_install":first_run},
        "warm_queries":statistics(&samples),"total_workload_ms":elapsed_ms(started),
        "timing_region":"CapabilityService::execute: shared cross-process resource admission, installed catalog/lease/package validation, cached Wasmtime component instantiation, fuel-bounded echo execution and JSON serialization; result decoding/assertions excluded",
        "limitations":["Direct native API excludes helper startup/IPC, host admission, agent approvals and UI input/rendering.",
            "Single tiny component with no host imports, tools, network or models; not a representative plugin or task-quality benchmark.",
            "Installation validates/compiles the component and may populate its cache. First run is one post-install observation, not a cold-launch percentile.",
            "No memory-use, concurrency, OS acceptance or Rust-versus-Python speedup is measured.",
            "Hardware/load are point-in-time observations; ordinary system activity is uncontrolled."]
    });
    std::fs::create_dir_all(output.parent().context("Evidence path has no parent")?)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&output)?;
    serde_json::to_writer_pretty(&mut file, &report)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    println!(
        "{}",
        json!({"output":output,"warm_samples":SAMPLES,
        "p50_ms":report["warm_queries"]["p50"],"p95_ms":report["warm_queries"]["p95"]})
    );
    Ok(())
}
