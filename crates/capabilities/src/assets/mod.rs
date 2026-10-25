//! App-owned asset provisioning. Install requests identify a reviewed catalog
//! entry; they cannot supply a URL, digest, filesystem destination or executable.

mod activation;
mod archive;
pub mod catalog;
mod download;

use anyhow::{Context, Result, ensure};
use catalog::{Asset, Variant};
use download::{Downloader, Progress};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobStatus {
    pub job_id: Uuid,
    pub asset_id: String,
    pub phase: String,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub progress: f64,
    pub message: String,
    pub updated_at_ms: u64,
    pub owner_pid: u32,
}
impl JobStatus {
    pub fn active(&self) -> bool {
        matches!(
            self.phase.as_str(),
            "queued" | "downloading" | "extracting" | "building" | "verifying"
        )
    }
}

struct Job {
    status: JobStatus,
    cancel: CancellationToken,
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    asset_id: String,
    revision: String,
    platform: String,
    fingerprint: String,
    files: Vec<archive::InstalledFile>,
    exports: BTreeMap<String, PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Activation {
    pub asset_id: String,
    pub config_patch: Option<Value>,
    pub embedding_config: Option<Value>,
    pub language_server: Option<Value>,
}

#[derive(Clone)]
pub struct AssetManager {
    root: PathBuf,
    platform: String,
    catalog: Arc<Vec<Asset>>,
    downloader: Downloader,
    jobs: Arc<Mutex<BTreeMap<Uuid, Job>>>,
    serial: Arc<Semaphore>,
    stopped: CancellationToken,
}

impl AssetManager {
    /// No network or native model/library load occurs during construction.
    pub fn new(data: &Path) -> Result<Self> {
        ensure!(data.is_absolute(), "Asset data root must be absolute");
        let data = std::fs::canonicalize(data)?;
        let root = data.join("assets");
        std::fs::create_dir_all(&root)?;
        let root = std::fs::canonicalize(root)?;
        ensure!(
            root.starts_with(&data),
            "Asset root escapes application data"
        );
        for name in ["downloads", "installed", "staging", "jobs", "locks"] {
            std::fs::create_dir_all(root.join(name))?;
            ensure!(
                std::fs::canonicalize(root.join(name))?.starts_with(&root),
                "Asset subdirectory escapes owned root"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self {
            downloader: Downloader::new(root.join("downloads"))?,
            root,
            platform: catalog::platform(),
            catalog: Arc::new(catalog::catalog()?),
            jobs: Arc::new(Mutex::new(BTreeMap::new())),
            serial: Arc::new(Semaphore::new(1)),
            stopped: CancellationToken::new(),
        })
    }

    fn asset(&self, id: &str) -> Result<&Asset> {
        self.catalog
            .iter()
            .find(|asset| asset.id == id)
            .with_context(|| format!("Unknown reviewed asset: {id}"))
    }
    fn fingerprint(variant: &Variant) -> Result<String> {
        use sha2::Digest;
        Ok(format!(
            "{:x}",
            sha2::Sha256::digest(serde_json::to_vec(variant)?)
        ))
    }
    fn location(&self, asset: &Asset) -> Result<PathBuf> {
        Ok(self
            .root
            .join("installed")
            .join(&asset.id)
            .join(Self::fingerprint(asset.variant(&self.platform)?)?))
    }

    fn plan(&self, id: &str) -> Result<Vec<String>> {
        fn visit(
            manager: &AssetManager,
            id: &str,
            seen: &mut HashSet<String>,
            visiting: &mut HashSet<String>,
            out: &mut Vec<String>,
        ) -> Result<()> {
            if seen.contains(id) {
                return Ok(());
            }
            ensure!(visiting.insert(id.to_owned()), "Asset dependency cycle");
            let asset = manager.asset(id)?;
            asset.variant(&manager.platform)?;
            for dependency in &asset.requires {
                visit(manager, dependency, seen, visiting, out)?;
            }
            visiting.remove(id);
            seen.insert(id.to_owned());
            out.push(id.to_owned());
            Ok(())
        }
        let mut result = Vec::new();
        visit(
            self,
            id,
            &mut HashSet::new(),
            &mut HashSet::new(),
            &mut result,
        )?;
        Ok(result)
    }

    fn receipt(&self, asset: &Asset) -> Result<(PathBuf, Receipt)> {
        let path = self.location(asset)?;
        let canonical = std::fs::canonicalize(&path)?;
        ensure!(
            canonical.starts_with(&self.root),
            "Installed asset escapes owned root"
        );
        let receipt: Receipt = read_json(&path.join("receipt.json"), 8 * 1024 * 1024)?;
        ensure!(
            receipt.asset_id == asset.id
                && receipt.revision == asset.revision
                && receipt.platform == self.platform
                && receipt.fingerprint == Self::fingerprint(asset.variant(&self.platform)?)?,
            "Installed asset receipt is for another release"
        );
        ensure!(
            receipt.files.len() <= 20_000,
            "Installed receipt exceeds file budget"
        );
        for file in &receipt.files {
            ensure!(
                catalog::safe_relative(file.path.to_str().context("Invalid installed path")?),
                "Unsafe installed asset path"
            );
            let metadata = std::fs::symlink_metadata(path.join(&file.path))?;
            ensure!(
                metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && metadata.len() == file.size_bytes,
                "Installed asset changed; reinstall it"
            );
        }
        Ok((path, receipt))
    }

    pub fn catalog(&self) -> Result<Value> {
        let data = self.root.parent().context("Asset data root missing")?;
        let configuration =
            read_json::<Value>(&data.join("capabilities.json"), 1024 * 1024).unwrap_or(json!({}));
        let embedding =
            read_json::<Value>(&data.join("embedding.json"), 65536).unwrap_or(json!({}));
        let language_servers = read_json::<Value>(&data.join("language-servers.json"), 1024 * 1024)
            .unwrap_or(json!({}));
        let mut entries = Vec::new();
        for asset in self.catalog.iter() {
            let plan = self.plan(&asset.id);
            let supported = plan.is_ok();
            let size_bytes = plan
                .as_ref()
                .ok()
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| self.asset(id).ok())
                        .filter_map(|a| a.variant(&self.platform).ok())
                        .flat_map(|v| &v.files)
                        .map(|f| f.size_bytes)
                        .sum::<u64>()
                })
                .unwrap_or(0);
            let receipt = self.receipt(asset).ok();
            let installed = receipt.is_some();
            let selected = receipt.as_ref().and_then(|(root, receipt)| {
                receipt
                    .exports
                    .get(
                        if matches!(asset.kind.as_str(), "model" | "embedding" | "voice")
                            || asset.id == "whisper-tiny-en"
                        {
                            "model"
                        } else if asset.kind == "pdf" || asset.id == "onnx-runtime" {
                            "library"
                        } else {
                            "executable"
                        },
                    )
                    .map(|p| root.join(p))
            });
            let pointer = match asset.id.as_str() {
                "llama-runtime" => "/llama/executable",
                "whisper-runtime" => "/whisper/executable",
                "whisper-tiny-en" => "/whisper/assets/model",
                "pdfium-runtime" => "/pdfium_library",
                "sherpa-runtime" => "/sherpa/executable",
                "vits-ljs-int8" => "/sherpa/assets/model",
                _ => "/llama/model",
            };
            let active = if asset.kind == "language-server" {
                selected.as_ref().is_some_and(|path| {
                    language_servers
                        .get("servers")
                        .and_then(Value::as_array)
                        .is_some_and(|servers| {
                            servers
                                .iter()
                                .any(|server| server.get("executable") == Some(&json!(path)))
                        })
                })
            } else if asset.kind == "embedding" {
                embedding
                    .get("model_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id.ends_with(&asset.revision))
            } else if asset.id == "onnx-runtime" {
                selected
                    .as_ref()
                    .is_some_and(|path| embedding.get("runtime_library") == Some(&json!(path)))
            } else {
                selected
                    .as_ref()
                    .is_some_and(|path| configuration.pointer(pointer) == Some(&json!(path)))
            };
            entries.push(json!({"id":asset.id,"name":asset.name,"kind":asset.kind,"description":asset.description,"revision":asset.revision,"source_url":asset.project_url,"license":asset.license,"requires":asset.requires,"hidden":asset.hidden,"size_bytes":size_bytes,"supported":supported,"unavailable_reason":plan.err().map(|error|error.to_string()),"installed":installed,"active":active,"source_build":asset.variant(&self.platform).map(|v|v.source_build).unwrap_or(false)}));
        }
        Ok(json!({"platform":self.platform,"assets":entries}))
    }

    pub fn active_jobs(&self) -> usize {
        self.jobs
            .lock()
            .map(|jobs| jobs.values().filter(|job| job.status.active()).count())
            .unwrap_or(1)
    }

    pub fn install(&self, id: &str) -> Result<JobStatus> {
        ensure!(!self.stopped.is_cancelled(), "Asset service is stopping");
        let plan = self.plan(id)?;
        // Cross-workspace callers may observe another helper's durable job.
        if let Some(job) = self
            .status(None)?
            .into_iter()
            .find(|job| job.asset_id == id && job.active())
        {
            return Ok(job);
        }
        self.prune_jobs()?;
        let total = plan
            .iter()
            .map(|id| {
                self.asset(id)
                    .unwrap()
                    .variant(&self.platform)
                    .unwrap()
                    .files
                    .iter()
                    .map(|file| file.size_bytes)
                    .sum::<u64>()
            })
            .sum();
        let status = JobStatus {
            job_id: Uuid::new_v4(),
            asset_id: id.to_owned(),
            phase: "queued".into(),
            downloaded_bytes: 0,
            total_bytes: total,
            progress: 0.,
            message: "Waiting to install the selected assets".into(),
            updated_at_ms: now(),
            owner_pid: std::process::id(),
        };
        {
            let mut jobs = self
                .jobs
                .lock()
                .map_err(|_| anyhow::anyhow!("Asset job state unavailable"))?;
            ensure!(
                jobs.values().filter(|job| job.status.active()).count() < 8,
                "Too many selected asset installations"
            );
            if jobs.len() >= 64
                && let Some(old) = jobs
                    .iter()
                    .find(|(_, job)| !job.status.active())
                    .map(|(id, _)| *id)
            {
                jobs.remove(&old);
            }
            jobs.insert(
                status.job_id,
                Job {
                    status: status.clone(),
                    cancel: self.stopped.child_token(),
                },
            );
        }
        write_json(
            &self
                .root
                .join("jobs")
                .join(format!("{}.json", status.job_id)),
            &status,
        )?;
        let manager = self.clone();
        let id = status.job_id;
        tokio::spawn(async move {
            let heartbeat_manager = manager.clone();
            let heartbeat_stop = CancellationToken::new();
            let heartbeat = heartbeat_stop.clone();
            let reporter = tokio::spawn(async move {
                loop {
                    tokio::select! {_=heartbeat.cancelled()=>break,_=tokio::time::sleep(Duration::from_millis(500))=>{}}
                    if heartbeat_manager
                        .root
                        .join("jobs")
                        .join(format!("{id}.cancel"))
                        .is_file()
                        && let Ok(jobs) = heartbeat_manager.jobs.lock()
                        && let Some(job) = jobs.get(&id)
                    {
                        job.cancel.cancel();
                    }
                    let snapshot = heartbeat_manager.snapshot(id);
                    let path = heartbeat_manager
                        .root
                        .join("jobs")
                        .join(format!("{id}.json"));
                    if let Some(snapshot) = snapshot {
                        let _ =
                            tokio::task::spawn_blocking(move || write_json(&path, &snapshot)).await;
                    }
                }
            });
            let result = manager.run_install(id, &plan).await;
            let cancelled = manager
                .jobs
                .lock()
                .ok()
                .and_then(|jobs| jobs.get(&id).map(|job| job.cancel.is_cancelled()))
                .unwrap_or(true);
            manager.update(
                id,
                if result.is_ok() {
                    "installed"
                } else if cancelled {
                    "cancelled"
                } else {
                    "failed"
                },
                result
                    .as_ref()
                    .err()
                    .map(|e| format!("{e:#}"))
                    .unwrap_or_else(|| "Verified assets installed. Choose Use to activate.".into()),
            );
            heartbeat_stop.cancel();
            let _ = reporter.await;
            if let Some(snapshot) = manager.snapshot(id) {
                let path = manager.root.join("jobs").join(format!("{id}.json"));
                let _ = tokio::task::spawn_blocking(move || write_json(&path, &snapshot)).await;
            }
        });
        Ok(status)
    }

    fn prune_jobs(&self) -> Result<()> {
        let mut terminal = Vec::new();
        for entry in std::fs::read_dir(self.root.join("jobs"))? {
            let entry = entry?;
            if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(status) = read_json::<JobStatus>(&entry.path(), 32768)
                && (!status.active() || !process_alive(status.owner_pid))
            {
                terminal.push((status.updated_at_ms, status.job_id, entry.path()));
            }
        }
        terminal.sort_by_key(|item| std::cmp::Reverse(item.0));
        for (_, id, path) in terminal.into_iter().skip(48) {
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(self.root.join("jobs").join(format!("{id}.cancel")));
        }
        Ok(())
    }

    fn snapshot(&self, id: Uuid) -> Option<JobStatus> {
        self.jobs.lock().ok()?.get(&id).map(|job| {
            let mut status = job.status.clone();
            status.updated_at_ms = now();
            status
        })
    }
    fn update(&self, id: Uuid, phase: &str, message: String) {
        if let Ok(mut jobs) = self.jobs.lock()
            && let Some(job) = jobs.get_mut(&id)
        {
            job.status.phase = phase.into();
            job.status.message = message;
            job.status.updated_at_ms = now();
            if phase == "installed" {
                job.status.downloaded_bytes = job.status.total_bytes;
                job.status.progress = 1.;
            }
        }
    }

    pub fn status(&self, id: Option<Uuid>) -> Result<Vec<JobStatus>> {
        let mut result = BTreeMap::new();
        for entry in std::fs::read_dir(self.root.join("jobs"))?.take(512) {
            let entry = entry?;
            if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(mut status) = read_json::<JobStatus>(&entry.path(), 32768) else {
                continue;
            };
            if status.active() && !process_alive(status.owner_pid) {
                status.phase = "interrupted".into();
                status.message =
                    "Installation was interrupted. Install again to resume verified downloads."
                        .into();
            }
            if id.is_none_or(|id| id == status.job_id) {
                result.insert(status.job_id, status);
            }
        }
        if let Ok(jobs) = self.jobs.lock() {
            for (job_id, job) in jobs.iter() {
                if id.is_none_or(|id| id == *job_id) {
                    result.insert(*job_id, job.status.clone());
                }
            }
        }
        let mut statuses: Vec<_> = result.into_values().collect();
        statuses.sort_by_key(|job| std::cmp::Reverse(job.updated_at_ms));
        if id.is_none() {
            statuses.truncate(64);
        }
        Ok(statuses)
    }

    pub fn cancel(&self, id: Uuid) -> Result<()> {
        ensure!(
            !self.status(Some(id))?.is_empty(),
            "Unknown asset installation"
        );
        if let Ok(jobs) = self.jobs.lock()
            && let Some(job) = jobs.get(&id)
        {
            job.cancel.cancel();
        }
        std::fs::write(
            self.root.join("jobs").join(format!("{id}.cancel")),
            b"cancel",
        )?;
        Ok(())
    }

    pub async fn shutdown(&self) {
        self.stopped.cancel();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while self.active_jobs() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn install_bundled(
        &self,
        stage: &Path,
        asset: &Asset,
        variant: &Variant,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        if !variant.source_build || asset.id != "whisper-runtime" {
            return Ok(false);
        }
        let executable = std::env::current_exe()?;
        let (base, bundle) = packaged_asset_location(&executable, &asset.id)?;
        if !bundle.is_dir() {
            return Ok(false);
        }
        let base = std::fs::canonicalize(base)?;
        let bundle = std::fs::canonicalize(bundle)?;
        ensure!(
            bundle.starts_with(&base),
            "Bundled native assets escape the application package"
        );
        #[derive(Deserialize)]
        struct Bundled {
            asset_id: String,
            revision: String,
            source_sha256: String,
            files: Vec<archive::InstalledFile>,
        }
        let manifest: Bundled = read_json(&bundle.join("manifest.json"), 1024 * 1024)?;
        ensure!(
            manifest.asset_id == asset.id
                && manifest.revision == asset.revision
                && variant
                    .files
                    .first()
                    .is_some_and(|file| file.sha256 == manifest.source_sha256),
            "Bundled native component provenance does not match the pinned source"
        );
        ensure!(
            !manifest.files.is_empty() && manifest.files.len() <= 64,
            "Bundled native file count exceeds budget"
        );
        let mut total = 0u64;
        for file in manifest.files {
            ensure!(
                catalog::safe_relative(file.path.to_str().context("Invalid bundled native path")?),
                "Unsafe bundled native path"
            );
            total = total.saturating_add(file.size_bytes);
            ensure!(
                total <= 256 * 1024 * 1024,
                "Bundled native bytes exceed budget"
            );
            let source = std::fs::canonicalize(bundle.join(&file.path))?;
            ensure!(
                source.starts_with(&bundle),
                "Bundled native file escapes package"
            );
            download::verify(&source, file.size_bytes, &file.sha256, cancel).await?;
            let destination = stage.join(&file.path);
            std::fs::create_dir_all(destination.parent().context("Missing native file parent")?)?;
            tokio::fs::copy(source, destination).await?;
        }
        Ok(true)
    }

    async fn run_install(&self, id: Uuid, plan: &[String]) -> Result<()> {
        let cancel = self
            .jobs
            .lock()
            .map_err(|_| anyhow::anyhow!("Asset job state unavailable"))?
            .get(&id)
            .context("Unknown asset job")?
            .cancel
            .clone();
        let _slot = tokio::select! {slot=self.serial.acquire()=>slot?,_=cancel.cancelled()=>anyhow::bail!("Asset installation cancelled")};
        let mut completed = 0;
        for asset_id in plan {
            ensure!(!cancel.is_cancelled(), "Asset installation cancelled");
            let asset = self.asset(asset_id)?.clone();
            let variant = asset.variant(&self.platform)?.clone();
            if self.verified_exports(&asset.id, &cancel).await.is_ok() {
                completed += variant
                    .files
                    .iter()
                    .map(|file| file.size_bytes)
                    .sum::<u64>();
                continue;
            }
            let asset_lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(self.root.join("locks").join(format!("{}.lock", asset.id)))?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                match fs2::FileExt::try_lock_exclusive(&asset_lock) {
                    Ok(()) => break,
                    Err(error)
                        if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {}
                    Err(error) => return Err(error.into()),
                }
                tokio::select! {_=cancel.cancelled()=>anyhow::bail!("Asset installation cancelled"),_=tokio::time::sleep_until(deadline)=>anyhow::bail!("Another workspace is installing this native dependency. Retry to resume after it finishes."),_=tokio::time::sleep(Duration::from_millis(50))=>{}}
            }
            if self.verified_exports(&asset.id, &cancel).await.is_ok() {
                completed += variant
                    .files
                    .iter()
                    .map(|file| file.size_bytes)
                    .sum::<u64>();
                continue;
            }
            let stage = self.root.join("staging").join(format!("{}-{id}", asset.id));
            std::fs::create_dir(&stage)?;
            let attempt = async {
                let bundled = self
                    .install_bundled(&stage, &asset, &variant, &cancel)
                    .await?;
                for file in variant.files.iter().filter(|_| !bundled) {
                    self.update(id, "downloading", format!("Downloading {}", asset.name));
                    let jobs = self.jobs.clone();
                    let base = completed;
                    let progress: Progress = Arc::new(move |bytes| {
                        if let Ok(mut jobs) = jobs.lock()
                            && let Some(job) = jobs.get_mut(&id)
                        {
                            job.status.downloaded_bytes = base + bytes;
                            job.status.progress = (job.status.downloaded_bytes as f64
                                / job.status.total_bytes.max(1) as f64)
                                .min(1.);
                        }
                    });
                    let blob = self.downloader.fetch(file, &cancel, progress).await?;
                    self.update(id, "extracting", format!("Preparing {}", asset.name));
                    let file = file.clone();
                    let size = file.size_bytes;
                    let root = stage.clone();
                    let token = cancel.clone();
                    tokio::task::spawn_blocking(move || {
                        archive::extract(&blob, &file, &root, &token)
                    })
                    .await??;
                    completed += size;
                }
                if bundled {
                    completed += variant
                        .files
                        .iter()
                        .map(|file| file.size_bytes)
                        .sum::<u64>();
                }
                if variant.source_build && !bundled {
                    self.update(
                        id,
                        "building",
                        format!("Building {} from the pinned source", asset.name),
                    );
                    build_whisper(&stage, &cancel).await?;
                }
                self.update(
                    id,
                    "verifying",
                    format!("Verifying installed {}", asset.name),
                );
                let root = stage.clone();
                let token = cancel.clone();
                let files =
                    tokio::task::spawn_blocking(move || archive::manifest(&root, &token)).await??;
                let mut exports = BTreeMap::new();
                for (key, export) in &variant.exports {
                    let matches: Vec<_> = files
                        .iter()
                        .filter(|file| {
                            file.path.file_name().and_then(|name| name.to_str())
                                == Some(export.basename.as_str())
                        })
                        .collect();
                    ensure!(
                        matches.len() == 1,
                        "Verified archive must contain exactly one {}",
                        export.basename
                    );
                    let relative = matches[0].path.clone();
                    #[cfg(unix)]
                    if export.executable {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(
                            stage.join(&relative),
                            std::fs::Permissions::from_mode(0o755),
                        )?;
                    }
                    exports.insert(key.clone(), relative);
                }
                let receipt = Receipt {
                    asset_id: asset.id.clone(),
                    revision: asset.revision.clone(),
                    platform: self.platform.clone(),
                    fingerprint: Self::fingerprint(&variant)?,
                    files,
                    exports,
                };
                write_json(&stage.join("receipt.json"), &receipt)?;
                write_json(&stage.join("ORIGIN.json"), &asset)?;
                let destination = self.location(&asset)?;
                std::fs::create_dir_all(
                    destination
                        .parent()
                        .context("Invalid installed asset root")?,
                )?;
                if destination.exists() {
                    if self.verified_exports(&asset.id, &cancel).await.is_ok() {
                        tokio::fs::remove_dir_all(&stage).await?;
                        return Ok(());
                    }
                    let quarantine = self.root.join("staging").join(format!(
                        "recovery-{}-{}",
                        asset.id,
                        Uuid::new_v4()
                    ));
                    tokio::fs::rename(&destination, &quarantine)
                        .await
                        .context("Stop active native tasks before repairing this installation")?;
                }
                tokio::fs::rename(&stage, &destination).await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if attempt.is_err() {
                let _ = tokio::fs::remove_dir_all(&stage).await;
            }
            attempt?;
        }
        Ok(())
    }
}

/// macOS reserves Contents/MacOS for signed code. Manifests and prebuilt asset
/// payloads belong in Resources; ordinary executables retain adjacent assets.
fn packaged_asset_location(executable: &Path, id: &str) -> Result<(PathBuf, PathBuf)> {
    let directory = executable.parent().context("Helper location missing")?;
    #[cfg(target_os = "macos")]
    if directory.file_name().is_some_and(|name| name == "MacOS")
        && let Some(contents) = directory
            .parent()
            .filter(|path| path.file_name().is_some_and(|name| name == "Contents"))
    {
        return Ok((
            contents.to_owned(),
            contents.join("Resources").join("native-assets").join(id),
        ));
    }
    Ok((
        directory.to_owned(),
        directory.join("native-assets").join(id),
    ))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path, limit: u64) -> Result<T> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    ensure!(
        file.metadata()?.len() <= limit,
        "Asset metadata exceeds budget"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "Asset metadata exceeds budget");
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    use std::io::Write;
    let bytes = serde_json::to_vec_pretty(value)?;
    ensure!(
        bytes.len() <= 8 * 1024 * 1024,
        "Asset metadata exceeds byte budget"
    );
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        #[cfg(unix)]
        std::fs::File::open(path.parent().context("Missing metadata parent")?)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        pid > 0
            && (unsafe { libc::kill(pid as i32, 0) } == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        };
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code = 0;
            let alive = GetExitCodeProcess(handle, &mut code) != 0 && code == 259;
            CloseHandle(handle);
            alive
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

async fn build_whisper(stage: &Path, cancel: &CancellationToken) -> Result<()> {
    use crate::process::{ProcessSpec, run};
    let source = std::fs::read_dir(stage.join("source"))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.join("CMakeLists.txt").is_file())
        .context("Pinned whisper source layout changed")?;
    let cmake=std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).map(|path|path.join(if cfg!(windows){"cmake.exe"}else{"cmake"})).find(|path|path.is_file()).context("This development build needs CMake and native compiler tools to build whisper.cpp. Distribution packages must include the prebuilt native component.")?;
    let cmake = std::fs::canonicalize(cmake)?;
    let build = source.join("build-native");
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let memory_jobs = (system.available_memory().saturating_sub(512 * 1024 * 1024)
        / (512 * 1024 * 1024))
        .max(1) as usize;
    let jobs = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(memory_jobs);
    let configure = vec![
        "-S".into(),
        source.to_string_lossy().into_owned(),
        "-B".into(),
        build.to_string_lossy().into_owned(),
        "-DCMAKE_BUILD_TYPE=Release".into(),
        "-DWHISPER_BUILD_TESTS=OFF".into(),
        "-DWHISPER_BUILD_EXAMPLES=ON".into(),
        "-DWHISPER_BUILD_SERVER=OFF".into(),
        "-DWHISPER_CURL=OFF".into(),
        "-DWHISPER_SDL2=OFF".into(),
        "-DBUILD_SHARED_LIBS=OFF".into(),
        "-DGGML_NATIVE=OFF".into(),
        "-DGGML_METAL_EMBED_LIBRARY=ON".into(),
    ];
    for args in [
        configure,
        vec![
            "--build".into(),
            build.to_string_lossy().into_owned(),
            "--config".into(),
            "Release".into(),
            "--target".into(),
            "whisper-cli".into(),
            "--parallel".into(),
            jobs.to_string(),
        ],
    ] {
        let result = run(
            ProcessSpec {
                executable: cmake.clone(),
                args,
                cwd: source.clone(),
                env: BTreeMap::new(),
                timeout: Duration::from_secs(1800),
                output_limit: 1024 * 1024,
            },
            cancel.clone(),
        )
        .await?;
        ensure!(
            result.success,
            "Pinned whisper.cpp build failed: {}",
            result.stderr
        );
    }
    let executable = build.join("bin").join(if cfg!(windows) {
        "whisper-cli.exe"
    } else {
        "whisper-cli"
    });
    ensure!(
        executable.is_file(),
        "Whisper build did not produce the native CLI"
    );
    std::fs::create_dir(stage.join("runtime"))?;
    std::fs::copy(
        executable,
        stage.join("runtime").join(if cfg!(windows) {
            "whisper-cli.exe"
        } else {
            "whisper-cli"
        }),
    )?;
    std::fs::create_dir(stage.join("notices"))?;
    for (name, path) in [
        ("whisper-LICENSE", source.join("LICENSE")),
        ("ggml-LICENSE", source.join("ggml/LICENSE")),
    ] {
        if path.is_file() {
            std::fs::copy(path, stage.join("notices").join(name))?;
        }
    }
    tokio::fs::remove_dir_all(stage.join("source")).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_asset_locations_match_packaged_and_development_layouts() {
        use std::path::Path;
        let (root, asset) =
            super::packaged_asset_location(Path::new("/build/workbench-helper"), "whisper-runtime")
                .unwrap();
        assert_eq!(root, Path::new("/build"));
        assert_eq!(asset, Path::new("/build/native-assets/whisper-runtime"));
        #[cfg(target_os = "macos")]
        {
            let (root, asset) = super::packaged_asset_location(
                Path::new("/Applications/Xylo.app/Contents/MacOS/workbench-helper"),
                "whisper-runtime",
            )
            .unwrap();
            assert_eq!(root, Path::new("/Applications/Xylo.app/Contents"));
            assert_eq!(
                asset,
                Path::new(
                    "/Applications/Xylo.app/Contents/Resources/native-assets/whisper-runtime"
                )
            );
        }
    }
    use super::*;
    #[test]
    fn reviewed_catalog_is_pinned_and_models_require_an_explicit_selection() {
        let root = tempfile::tempdir().unwrap();
        let manager = AssetManager::new(root.path()).unwrap();
        let catalog = manager.catalog().unwrap();
        assert_eq!(manager.active_jobs(), 0);
        assert!(manager.status(None).unwrap().is_empty());
        assert!(
            catalog["assets"]
                .as_array()
                .unwrap()
                .iter()
                .all(|asset| asset["installed"] == false && asset["active"] == false)
        );
        assert!(
            manager
                .plan("qwen3-0.6b-q8")
                .unwrap()
                .contains(&"llama-runtime".into())
        );
        assert!(manager.install("https://arbitrary.example/binary").is_err());
    }

    #[tokio::test]
    async fn installation_is_durable_verified_and_rejects_changed_bytes_on_activation() {
        let root = tempfile::tempdir().unwrap();
        let mut manager = AssetManager::new(root.path()).unwrap();
        let body = b"deterministic native fixture".to_vec();
        let (url, _, server) = download::tests::fixture(body.clone(), "normal").await;
        let mut spec = download::tests::specification(url, &body);
        spec.destination = "native-fixture".into();
        manager.catalog = Arc::new(vec![Asset {
            id: "fixture".into(),
            name: "Fixture".into(),
            kind: "model".into(),
            description: "fixture".into(),
            revision: "1".into(),
            project_url: "https://example.com".into(),
            license: catalog::License {
                identifier: "MIT".into(),
                url: "https://example.com/license".into(),
                notice: "fixture".into(),
            },
            requires: vec![],
            hidden: false,
            variants: vec![Variant {
                platform: "any".into(),
                files: vec![spec],
                exports: BTreeMap::from([(
                    "model".into(),
                    catalog::Export {
                        basename: "native-fixture".into(),
                        executable: false,
                    },
                )]),
                source_build: false,
            }],
        }]);
        manager.downloader = download::tests::local_downloader(manager.root.join("downloads"));
        let job = manager.install("fixture").unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if manager.snapshot(job.job_id).unwrap().active() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                } else {
                    break;
                }
            }
        })
        .await
        .unwrap();
        let status = manager.status(Some(job.job_id)).unwrap();
        assert_eq!(status[0].phase, "installed", "{}", status[0].message);
        assert_eq!(status[0].progress, 1.);
        let (directory, _) = manager
            .verified_exports("fixture", &CancellationToken::new())
            .await
            .unwrap();
        assert!(directory.starts_with(root.path().canonicalize().unwrap()));
        assert!(directory.join("receipt.json").is_file());
        std::fs::write(directory.join("native-fixture"), vec![0; body.len()]).unwrap();
        assert!(
            manager
                .verified_exports("fixture", &CancellationToken::new())
                .await
                .unwrap_err()
                .to_string()
                .contains("SHA256")
        );
        assert!(
            manager
                .activation("fixture", &CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(manager.catalog().unwrap()["assets"][0]["installed"], false);
        let repair = manager.install("fixture").unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while manager.snapshot(repair.job_id).unwrap().active() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(manager.snapshot(repair.job_id).unwrap().phase, "installed");
        manager
            .verified_exports("fixture", &CancellationToken::new())
            .await
            .unwrap();
        manager.shutdown().await;
        server.abort();
    }
}
