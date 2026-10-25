//! Durable package catalog. Catalog and per-plugin locks are deliberately
//! separate: a long-running guest does not prevent unrelated installations.

use super::security::{bounded_read, checked, hash, validate_grants, validate_manifest};
use crate::CapabilityConfig;
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workbench_core::{
    InstalledPlugin, NativePluginManifest, PluginCatalog, PluginInspection, PluginManagementResult,
};

const MAX_REGISTRY: usize = 2 * 1024 * 1024;
pub(super) const MAX_MODULE: usize = 16 * 1024 * 1024;
const MAX_PACKAGES: usize = 64;
const MAX_PACKAGE_STORAGE: u64 = 1024 * 1024 * 1024;
// Retain every accepted operation identity rather than forgetting a past write
// and accidentally accepting the same ID as a new request after a restart.
const MAX_OPERATIONS: usize = 4096;

#[derive(Clone)]
pub(super) struct Package {
    pub inspection: PluginInspection,
    pub manifest_bytes: Vec<u8>,
    pub module: Vec<u8>,
}

#[derive(Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default)]
    plugins: BTreeMap<String, InstalledPlugin>,
    #[serde(default)]
    operations: Vec<Operation>,
}

#[derive(Serialize, Deserialize)]
struct Operation {
    id: Uuid,
    digest: String,
    changed: bool,
    message: String,
}

pub(super) fn root(config: &CapabilityConfig) -> Result<PathBuf> {
    let management = config
        .management_root
        .as_ref()
        .context("Plugin management is unavailable: application storage is not configured")?;
    let management = management
        .canonicalize()
        .context("Application management directory is unavailable")?;
    let artifacts = config.artifacts.canonicalize()?;
    ensure!(
        !management.starts_with(&artifacts) && !artifacts.starts_with(&management),
        "Plugin management and artifact storage must be separate"
    );
    let root = management.join("plugins");
    // Check each parent before creating children; do not follow a substituted
    // storage symlink even briefly while attempting to initialize directories.
    for directory in [root.clone(), root.join("packages"), root.join("locks")] {
        match std::fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let metadata = std::fs::symlink_metadata(&directory)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Plugin storage cannot use symbolic links"
        );
    }
    ensure!(
        root.canonicalize()?.starts_with(&management),
        "Plugin storage escapes application management directory"
    );
    Ok(root)
}

pub(super) fn lock(
    path: &Path,
    shared: bool,
    wait: bool,
    cancel: &CancellationToken,
) -> Result<Arc<File>> {
    let file = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        checked(cancel)?;
        let result = if shared {
            FileExt::try_lock_shared(&file)
        } else {
            FileExt::try_lock_exclusive(&file)
        };
        match result {
            Ok(()) => return Ok(Arc::new(file)),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
            {
                ensure!(
                    wait && Instant::now() < deadline,
                    "Plugin is busy; stop its active run and retry this action"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn read_registry(root: &Path) -> Result<Registry> {
    let path = root.join("catalog.json");
    if !path.exists() {
        return Ok(Registry::default());
    }
    let registry: Registry = serde_json::from_slice(&bounded_read(&path, MAX_REGISTRY)?)?;
    ensure!(
        registry.plugins.len() <= MAX_PACKAGES && registry.operations.len() <= MAX_OPERATIONS,
        "Plugin catalog exceeds its record limit"
    );
    for (id, plugin) in &registry.plugins {
        validate_manifest(&plugin.manifest)?;
        ensure!(
            id == &plugin.manifest.id,
            "Plugin catalog identity is inconsistent"
        );
        validate_grants(&plugin.grants, &plugin.manifest.capabilities)?;
        ensure!(
            super::security::is_hash(&plugin.package_sha256),
            "Plugin catalog package identity is invalid"
        );
    }
    Ok(registry)
}

fn catalog(registry: &Registry, config: &CapabilityConfig) -> Result<PluginCatalog> {
    let plugins: Vec<_> = registry.plugins.values().cloned().collect();
    let revision = hash(&serde_json::to_vec(&plugins)?);
    let result = PluginCatalog { revision, plugins };
    ensure!(
        serde_json::to_vec(&result)?.len() <= config.max_output_bytes.saturating_sub(1024),
        "Plugin catalog exceeds the response budget"
    );
    Ok(result)
}

pub(super) fn list(config: &CapabilityConfig, cancel: &CancellationToken) -> Result<PluginCatalog> {
    let root = root(config)?;
    let _lock = lock(&root.join("catalog.lock"), true, true, cancel)?;
    catalog(&read_registry(&root)?, config)
}

fn operation_digest(name: &str, args: &Value) -> Result<String> {
    let mut bytes = name.as_bytes().to_vec();
    bytes.extend_from_slice(&serde_json::to_vec(args)?);
    Ok(hash(&bytes))
}

/// Reconcile a lost response without requiring the original picked package to
/// remain on disk. The mutation rechecks this under its exclusive catalog lock.
pub(super) fn replay(
    config: &CapabilityConfig,
    name: &str,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<Option<PluginManagementResult>> {
    let id: Uuid = serde_json::from_value(
        args.get("operation_id")
            .cloned()
            .context("operation_id is required")?,
    )?;
    ensure!(!id.is_nil(), "Choose a valid plugin operation identity");
    let digest = operation_digest(name, args)?;
    let root = root(config)?;
    let _lock = lock(&root.join("catalog.lock"), true, true, cancel)?;
    let registry = read_registry(&root)?;
    registry
        .operations
        .iter()
        .find(|operation| operation.id == id)
        .map(|previous| {
            ensure!(
                previous.digest == digest,
                "Plugin operation ID was reused with different arguments"
            );
            Ok(PluginManagementResult {
                operation_id: id,
                catalog: catalog(&registry, config)?,
                changed: previous.changed,
                message: previous.message.clone(),
            })
        })
        .transpose()
}

fn resolve_entry(manifest: &Path, relative: &str) -> Result<PathBuf> {
    let relative = Path::new(relative);
    ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "Plugin entrypoint must be a relative file below the package directory"
    );
    let parent = manifest
        .parent()
        .context("Plugin manifest has no package directory")?
        .canonicalize()?;
    let mut path = parent.clone();
    for part in relative.components() {
        path.push(part);
        ensure!(
            !std::fs::symlink_metadata(&path)?.file_type().is_symlink(),
            "Plugin entrypoint cannot use symbolic links"
        );
    }
    let canonical = path.canonicalize()?;
    ensure!(
        canonical.starts_with(parent) && canonical.is_file(),
        "Plugin entrypoint escapes its package directory"
    );
    Ok(canonical)
}

pub(super) fn inspect(path: &str, cancel: &CancellationToken) -> Result<Package> {
    checked(cancel)?;
    let path = Path::new(path);
    ensure!(
        path.is_absolute(),
        "Choose an absolute plugin package manifest path"
    );
    ensure!(
        !std::fs::symlink_metadata(path)?.file_type().is_symlink(),
        "Plugin manifest cannot be a symbolic link"
    );
    let manifest_path = path.canonicalize()?;
    let manifest_bytes = bounded_read(&manifest_path, 32 * 1024)?;
    let manifest: NativePluginManifest = serde_json::from_slice(&manifest_bytes)
        .context("Plugin manifest is not valid format 1 JSON")?;
    validate_manifest(&manifest)?;
    let module = bounded_read(
        &resolve_entry(&manifest_path, &manifest.entrypoint)?,
        MAX_MODULE,
    )?;
    ensure!(
        hash(&module) == manifest.sha256,
        "Plugin component checksum does not match the manifest"
    );
    checked(cancel)?;
    let package_sha256 = package_hash(&manifest_bytes, &module);
    Ok(Package {
        inspection: PluginInspection {
            manifest,
            module_bytes: module.len() as u64,
            package_sha256,
        },
        manifest_bytes,
        module,
    })
}

fn package_hash(manifest: &[u8], module: &[u8]) -> String {
    let mut bytes = b"workbench-native-plugin-v1\0".to_vec();
    bytes.extend_from_slice(&(manifest.len() as u64).to_le_bytes());
    bytes.extend_from_slice(manifest);
    bytes.extend_from_slice(hash(module).as_bytes());
    hash(&bytes)
}

fn stage_package(root: &Path, package: &Package, cancel: &CancellationToken) -> Result<()> {
    let destination = root
        .join("packages")
        .join(&package.inspection.package_sha256);
    if destination.exists() {
        verify_package(
            &destination,
            &package.inspection.manifest,
            &package.inspection.package_sha256,
        )?;
        return Ok(());
    }
    let staging = tempfile::Builder::new()
        .prefix(".install-")
        .tempdir_in(root.join("packages"))?;
    for (name, bytes) in [
        ("manifest.json", &package.manifest_bytes),
        ("component.wasm", &package.module),
    ] {
        checked(cancel)?;
        let mut file = File::options()
            .write(true)
            .create_new(true)
            .open(staging.path().join(name))?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    checked(cancel)?;
    // Rename of a fresh directory publishes all bytes together. Catalog readers
    // can reference it only after the subsequent durable catalog commit.
    std::fs::rename(staging.path(), &destination)?;
    sync_directory(&root.join("packages"))?;
    Ok(())
}

/// Called only under the exclusive catalog lock. A live run prevents its ID
/// from being updated/removed, so a package absent from the current catalog
/// cannot still have an admitted run. Install validation owns bytes in memory,
/// not a published package path. Terminal replay records need only metadata.
fn collect_unreferenced(
    root: &Path,
    registry: &Registry,
    cancel: &CancellationToken,
) -> Result<u64> {
    let referenced: BTreeSet<_> = registry
        .plugins
        .values()
        .map(|plugin| plugin.package_sha256.as_str())
        .collect();
    let mut total = 0u64;
    for (index, entry) in std::fs::read_dir(root.join("packages"))?.enumerate() {
        ensure!(
            index < 4096,
            "Plugin package directory exceeds its maintenance budget"
        );
        checked(cancel)?;
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Plugin storage contains an invalid filename"))?;
        ensure!(
            super::security::is_hash(&name) || name.starts_with(".install-"),
            "Plugin storage contains an unrecognized package directory"
        );
        let metadata = std::fs::symlink_metadata(entry.path())?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Plugin package storage is not a regular directory"
        );
        if !referenced.contains(name.as_str()) {
            std::fs::remove_dir_all(entry.path())?;
            continue;
        }
        for (file_index, file) in std::fs::read_dir(entry.path())?.enumerate() {
            ensure!(
                file_index < 2,
                "Installed package contains unexpected files"
            );
            let file = file?;
            ensure!(
                matches!(
                    file.file_name().to_str(),
                    Some("manifest.json" | "component.wasm")
                ),
                "Installed package contains an unexpected file"
            );
            let metadata = std::fs::symlink_metadata(file.path())?;
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "Installed package contains an unsupported file"
            );
            total = total
                .checked_add(metadata.len())
                .context("Plugin package storage size overflow")?;
        }
    }
    Ok(total)
}

struct PackageRollback {
    root: PathBuf,
    package: PathBuf,
    active: bool,
}

impl Drop for PackageRollback {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        // Atomic publication can have succeeded even if its directory sync
        // reported an error. Preserve a package whenever the reread catalog
        // references it, or whenever the catalog cannot be read conclusively.
        if let Ok(registry) = read_registry(&self.root)
            && !registry.plugins.values().any(|plugin| {
                self.package
                    .file_name()
                    .is_some_and(|name| name == plugin.package_sha256.as_str())
            })
        {
            let _ = std::fs::remove_dir_all(&self.package);
        }
    }
}

fn verify_package(path: &Path, manifest: &NativePluginManifest, expected: &str) -> Result<Vec<u8>> {
    let manifest_bytes = bounded_read(&path.join("manifest.json"), 32 * 1024)?;
    let stored: NativePluginManifest = serde_json::from_slice(&manifest_bytes)?;
    ensure!(
        &stored == manifest,
        "Installed plugin manifest was changed; reinstall this package"
    );
    let module = bounded_read(&path.join("component.wasm"), MAX_MODULE)?;
    ensure!(
        hash(&module) == manifest.sha256 && package_hash(&manifest_bytes, &module) == expected,
        "Installed plugin checksum failed; reinstall this package"
    );
    Ok(module)
}

pub(super) struct Loaded {
    pub plugin: InstalledPlugin,
    pub module: Vec<u8>,
    pub lease: Arc<File>,
    pub root: PathBuf,
}

pub(super) fn load(
    config: &CapabilityConfig,
    id: &str,
    expected: &str,
    cancel: &CancellationToken,
) -> Result<Loaded> {
    super::security::validate_id(id)?;
    ensure!(
        super::security::is_hash(expected),
        "Run requires the reviewed package SHA256"
    );
    let root = root(config)?;
    let _catalog = lock(&root.join("catalog.lock"), true, true, cancel)?;
    let registry = read_registry(&root)?;
    let plugin = registry
        .plugins
        .get(id)
        .context("Plugin is not installed")?
        .clone();
    ensure!(
        plugin.enabled,
        "Plugin is disabled; enable it in Plugins before running"
    );
    ensure!(
        plugin.package_sha256 == expected,
        "Plugin changed since review; inspect the installed package and approve its current identity"
    );
    let lease = lock(
        &root.join("locks").join(format!("{id}.lock")),
        true,
        false,
        cancel,
    )?;
    let module = verify_package(
        &root.join("packages").join(expected),
        &plugin.manifest,
        expected,
    )?;
    checked(cancel)?;
    Ok(Loaded {
        plugin,
        module,
        lease,
        root,
    })
}

fn persist(root: &Path, registry: &Registry) -> Result<()> {
    let bytes = serde_json::to_vec(registry)?;
    ensure!(
        bytes.len() <= MAX_REGISTRY,
        "Plugin registry exceeds its storage budget"
    );
    let mut staging = tempfile::NamedTempFile::new_in(root)?;
    staging.write_all(&bytes)?;
    staging.as_file().sync_all()?;
    staging
        .persist(root.join("catalog.json"))
        .map_err(|error| error.error)?;
    sync_directory(root)
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InstallArgs {
    pub operation_id: Uuid,
    pub expected_revision: String,
    pub manifest_path: String,
    pub expected_package_sha256: String,
    pub grants: Vec<String>,
    pub enabled: bool,
    #[serde(default)]
    pub replace: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MutationArgs {
    operation_id: Uuid,
    expected_revision: String,
    id: String,
    enabled: Option<bool>,
    grants: Option<Vec<String>>,
}

pub(super) fn mutate(
    config: &CapabilityConfig,
    name: &str,
    args: Value,
    package: Option<Package>,
    cancel: &CancellationToken,
) -> Result<PluginManagementResult> {
    checked(cancel)?;
    let operation_id: Uuid = serde_json::from_value(
        args.get("operation_id")
            .cloned()
            .context("operation_id is required")?,
    )?;
    ensure!(
        !operation_id.is_nil(),
        "Choose a valid plugin operation identity"
    );
    let digest = operation_digest(name, &args)?;
    let root = root(config)?;
    let _catalog = lock(&root.join("catalog.lock"), false, true, cancel)?;
    let mut registry = read_registry(&root)?;
    if let Some(previous) = registry
        .operations
        .iter()
        .find(|operation| operation.id == operation_id)
    {
        ensure!(
            previous.digest == digest,
            "Plugin operation ID was reused with different arguments"
        );
        return Ok(PluginManagementResult {
            operation_id,
            catalog: catalog(&registry, config)?,
            changed: previous.changed,
            message: previous.message.clone(),
        });
    }
    ensure!(
        registry.operations.len() < MAX_OPERATIONS,
        "Plugin management history is full; no action was applied. Export this application profile before resetting its plugin management storage"
    );
    let revision = args
        .get("expected_revision")
        .and_then(Value::as_str)
        .context("expected_revision is required")?
        .to_owned();
    ensure!(
        catalog(&registry, config)?.revision == revision,
        "Plugin catalog changed; refresh and review this action again"
    );
    let before = serde_json::to_vec(&registry.plugins)?;
    let stored_bytes = collect_unreferenced(&root, &registry, cancel)?;
    let mut rollback = None;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let message;
    if name == "helper_plugin_install" {
        let request: InstallArgs = serde_json::from_value(args)?;
        let package = package.context("Plugin installation has no inspected package")?;
        ensure!(
            request.operation_id == operation_id,
            "Plugin operation identity changed"
        );
        ensure!(
            request.expected_revision == revision,
            "Plugin catalog revision changed"
        );
        ensure!(
            request.expected_package_sha256 == package.inspection.package_sha256,
            "Plugin package changed since inspection; choose it again and review its permissions"
        );
        let manifest = &package.inspection.manifest;
        validate_grants(&request.grants, &manifest.capabilities)?;
        let previous = registry.plugins.get(&manifest.id);
        ensure!(
            request.replace || previous.is_none(),
            "Plugin ID already exists; use the reviewed Update action"
        );
        ensure!(
            !request.replace || previous.is_some(),
            "Plugin is not installed; use Install"
        );
        if let Some(previous) = previous {
            ensure!(
                previous.package_sha256 == package.inspection.package_sha256
                    || previous.manifest.version != manifest.version,
                "A changed plugin package must declare a different version"
            );
        }
        ensure!(
            previous.is_some() || registry.plugins.len() < MAX_PACKAGES,
            "At most 64 plugins can be installed"
        );
        let _run = lock(
            &root.join("locks").join(format!("{}.lock", manifest.id)),
            false,
            false,
            cancel,
        )?;
        let package_path = root
            .join("packages")
            .join(&package.inspection.package_sha256);
        let was_present = package_path.exists();
        ensure!(
            was_present
                || stored_bytes
                    .saturating_add(package.module.len() as u64)
                    .saturating_add(package.manifest_bytes.len() as u64)
                    <= MAX_PACKAGE_STORAGE,
            "Plugin storage would exceed 1GiB; remove an unused installed plugin first"
        );
        stage_package(&root, &package, cancel)?;
        if !was_present {
            rollback = Some(PackageRollback {
                root: root.clone(),
                package: package_path,
                active: true,
            });
        }
        let plugin = InstalledPlugin {
            manifest: manifest.clone(),
            package_sha256: package.inspection.package_sha256,
            enabled: request.enabled,
            grants: request.grants,
            installed_at: previous.map_or(now, |plugin| plugin.installed_at),
            updated_at: now,
        };
        registry.plugins.insert(manifest.id.clone(), plugin);
        message = if request.replace {
            "Plugin updated"
        } else {
            "Plugin installed"
        }
        .to_owned();
    } else {
        let request: MutationArgs = serde_json::from_value(args)?;
        ensure!(
            request.operation_id == operation_id && request.expected_revision == revision,
            "Plugin mutation identity changed"
        );
        super::security::validate_id(&request.id)?;
        let _run = lock(
            &root.join("locks").join(format!("{}.lock", request.id)),
            false,
            false,
            cancel,
        )?;
        let plugin = registry
            .plugins
            .get_mut(&request.id)
            .context("Plugin is not installed")?;
        match name {
            "helper_plugin_set_enabled" => {
                ensure!(
                    request.grants.is_none(),
                    "Enable action cannot change grants"
                );
                plugin.enabled = request.enabled.context("enabled is required")?;
                message = if plugin.enabled {
                    "Plugin enabled"
                } else {
                    "Plugin disabled"
                }
                .to_owned();
            }
            "helper_plugin_set_grants" => {
                ensure!(
                    request.enabled.is_none(),
                    "Grant action cannot change enablement"
                );
                let grants = request.grants.context("grants is required")?;
                validate_grants(&grants, &plugin.manifest.capabilities)?;
                plugin.grants = grants;
                message = "Plugin permissions updated".to_owned();
            }
            "helper_plugin_remove" => {
                ensure!(
                    request.grants.is_none() && request.enabled.is_none(),
                    "Remove action cannot include grants or enablement"
                );
                registry.plugins.remove(&request.id);
                message = "Plugin removed".to_owned();
            }
            _ => bail!("Unknown plugin management action"),
        }
        if let Some(plugin) = registry.plugins.get_mut(&request.id) {
            plugin.updated_at = now;
        }
    }
    let changed = before != serde_json::to_vec(&registry.plugins)?;
    // Check the response bound before committing a catalog that cannot be read.
    let result_catalog = catalog(&registry, config)?;
    registry.operations.push(Operation {
        id: operation_id,
        digest,
        changed,
        message: message.clone(),
    });
    checked(cancel)?;
    persist(&root, &registry)?;
    if let Some(rollback) = &mut rollback {
        rollback.active = false;
    }
    // The transaction is already committed. A cleanup failure must not turn it
    // into an ambiguous failed mutation; retain the exact accepted receipt.
    if let Err(error) = collect_unreferenced(&root, &registry, &CancellationToken::new()) {
        tracing::warn!(error = %error, "Plugin change committed; deferred package cleanup failed");
    }
    Ok(PluginManagementResult {
        operation_id,
        catalog: result_catalog,
        changed,
        message,
    })
}
