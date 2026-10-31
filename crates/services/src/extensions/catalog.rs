use super::manifest::*;
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct ExtensionService {
    pub(super) root: PathBuf,
}
impl ExtensionService {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }
    pub(super) fn lock(&self) -> Result<fs::File> {
        fs::create_dir_all(&self.root)?;
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("catalog.lock"))?;
        f.lock_exclusive()?;
        self.remove_unreadable_stores()?;
        Ok(f)
    }
    /// Catalog and runtime setup files that no longer decode (corrupt or from
    /// an older format) are deleted, not migrated: their packages read as not
    /// installed and their setups as not configured. Caller holds the lock.
    fn remove_unreadable_stores(&self) -> Result<()> {
        let catalog = self.root.join("catalog.json");
        if decodes::<Vec<ExtensionPackage>>(&catalog)? == Some(false) {
            fs::remove_file(&catalog)?;
        }
        self.remove_unreadable_runtime()
    }
    pub fn list(&self) -> Result<Vec<ExtensionPackage>> {
        let _lock = self.lock()?;
        self.read_catalog()
    }
    pub(super) fn read_catalog(&self) -> Result<Vec<ExtensionPackage>> {
        match fs::read(self.root.join("catalog.json")) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }
    pub(super) fn save(&self, packages: &[ExtensionPackage]) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(&serde_json::to_vec_pretty(packages)?)?;
        file.as_file().sync_all()?;
        file.persist(self.root.join("catalog.json"))?;
        Ok(())
    }
    pub fn install_local(&self, source: &Path) -> Result<ExtensionPackage> {
        let source = source.canonicalize()?;
        let name = source
            .file_name()
            .context("Source folder name missing")?
            .to_string_lossy();
        self.install_snapshot(&source, &format!("local:{}", source.display()), None, &name)
    }
    pub(super) fn install_snapshot(
        &self,
        source: &Path,
        provenance: &str,
        revision: Option<String>,
        fallback: &str,
    ) -> Result<ExtensionPackage> {
        let _lock = self.lock()?;
        self.remove_abandoned_staging();
        let staging = tempfile::tempdir_in(&self.root)?;
        let content = staging.path().join("content");
        let mut total = 0;
        let mut count = 0;
        copy_tree(source, &content, &mut total, &mut count, 0)?;
        let content = content.canonicalize()?;
        let (name, version, components) = inspect(&content, fallback)?;
        let digest = tree_digest(&content)?;
        let id = format!(
            "extension-{}",
            &hex::encode(Sha256::digest(provenance.as_bytes()))[..24]
        );
        let mut packages = self.read_catalog()?;
        let prior = packages.iter().find(|p| p.id == id);
        let replaced = prior
            .filter(|p| p.digest != digest)
            .map(|p| p.digest.clone());
        // Changed content is a new package to review: an update never keeps
        // the previous version's enablement.
        let mut package = ExtensionPackage {
            id: id.clone(),
            name,
            version,
            source: provenance.into(),
            revision,
            digest: digest.clone(),
            enabled: prior.is_some_and(|p| p.enabled && p.digest == digest),
            components,
        };
        if let Some(prior) = prior.filter(|p| p.digest == digest) {
            package.components = prior.components.clone();
        }
        let snapshots = self.root.join("packages");
        fs::create_dir_all(&snapshots)?;
        let target = snapshots.join(&digest);
        // Reuse a snapshot only when it is complete. One left half-written or
        // half-deleted by an older interrupted install or removal is replaced.
        if target.exists() && !tree_digest(&target).is_ok_and(|existing| existing == digest) {
            self.discard_snapshot(&target)?;
        }
        if !target.exists() {
            fs::rename(&content, &target)?;
            #[cfg(unix)]
            fs::File::open(&snapshots)?.sync_all()?;
        }
        packages.retain(|p| p.id != id);
        packages.push(package.clone());
        self.save(&packages)?;
        // An update leaves the previous version's content unreferenced.
        if let Some(replaced) = replaced
            && !packages.iter().any(|p| p.digest == replaced)
            && let Err(error) = self.discard_snapshot(&snapshots.join(&replaced))
        {
            tracing::warn!(%error, "Could not remove the replaced extension version");
        }
        Ok(package)
    }
    /// Removes a package snapshot without ever leaving a partial folder under
    /// its digest name: it is first renamed to a staging name (atomic), then
    /// deleted. A deletion interrupted midway leaves only staging, which the
    /// next locked operation removes. Caller holds the catalog lock.
    fn discard_snapshot(&self, snapshot: &Path) -> Result<()> {
        if !snapshot.exists() {
            return Ok(());
        }
        let staging = self
            .root
            .join(format!(".tmp-discarded-{}", uuid::Uuid::new_v4().simple()));
        fs::rename(snapshot, &staging)?;
        #[cfg(unix)]
        if let Some(parent) = snapshot.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        fs::remove_dir_all(&staging)?;
        Ok(())
    }
    /// Staging directories left by an interrupted install. Callers hold the
    /// catalog lock, so no install is in progress and every one is abandoned.
    fn remove_abandoned_staging(&self) {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            if entry.file_name().to_string_lossy().starts_with(".tmp")
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
            {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let _lock = self.lock()?;
        let mut packages = self.read_catalog()?;
        packages
            .iter_mut()
            .find(|p| p.id == id)
            .context("Extension no longer installed")?
            .enabled = enabled;
        self.save(&packages)
    }
    pub fn remove(&self, id: &str) -> Result<()> {
        let _lock = self.lock()?;
        let mut packages = self.read_catalog()?;
        let package = packages
            .iter()
            .find(|p| p.id == id)
            .context("Extension no longer installed")?
            .clone();
        self.revoke_runtime_setup_under_lock(id)?;
        packages.retain(|p| p.id != id);
        self.save(&packages)?;
        // Unlink only content no other installed package references. Active turns
        // hold loaded text; subsequent loads must revalidate the catalog.
        if !packages.iter().any(|p| p.digest == package.digest)
            && let Err(error) = self.discard_snapshot(&self.package_root(&package)?)
        {
            tracing::warn!(%error, "Could not remove extension content");
        }
        Ok(())
    }
    pub(super) fn record_setup_state(
        &self,
        id: &str,
        digest: &str,
        name: &str,
        readiness: Readiness,
        requirements: Vec<String>,
    ) -> Result<()> {
        let _lock = self.lock()?;
        let mut packages = self.read_catalog()?;
        let package = packages
            .iter_mut()
            .find(|p| p.id == id && p.digest == digest)
            .context("Extension changed during setup")?;
        let component = package
            .components
            .iter_mut()
            .find(|c| c.name == name)
            .context("Component missing")?;
        component.readiness = readiness;
        component.requirements = requirements;
        component.evidence = None;
        self.save(&packages)
    }
    pub fn record_execution(
        &self,
        id: &str,
        digest: &str,
        component: &str,
        evidence: &str,
    ) -> Result<()> {
        ensure!(
            !evidence.trim().is_empty() && evidence.len() <= 4096,
            "Execution evidence required"
        );
        let _lock = self.lock()?;
        let mut packages = self.read_catalog()?;
        let package = packages
            .iter_mut()
            .find(|p| p.id == id && p.digest == digest)
            .context("Extension changed during execution")?;
        let component = package
            .components
            .iter_mut()
            .find(|c| c.name == component)
            .context("Component missing")?;
        ensure!(
            component.readiness != Readiness::UnsupportedRequirement,
            "Unsupported component cannot be verified"
        );
        component.readiness = Readiness::ExecutionVerified;
        component.evidence = Some(evidence.into());
        self.save(&packages)
    }
    pub fn package_root(&self, package: &ExtensionPackage) -> Result<PathBuf> {
        ensure!(
            package.digest.len() == 64 && package.digest.chars().all(|c| c.is_ascii_hexdigit()),
            "Invalid package digest"
        );
        Ok(self.root.join("packages").join(&package.digest))
    }
    pub fn skill_metadata(&self) -> Result<Vec<SkillMetadata>> {
        let _lock = self.lock()?;
        let mut output = Vec::new();
        for p in self.read_catalog()?.into_iter().filter(|p| p.enabled) {
            let root = self.package_root(&p)?;
            for c in &p.components {
                if !matches!(
                    c.kind,
                    ComponentKind::Skill | ComponentKind::Command | ComponentKind::Agent
                ) || c.readiness == Readiness::UnsupportedRequirement
                {
                    continue;
                }
                let meta = read_metadata(&safe_path(&root, &c.path)?)?;
                output.push(SkillMetadata {
                    id: format!("{}:{}", p.id, c.name),
                    name: c.name.clone(),
                    description: c.description.clone(),
                    automatic: !meta.disable_model_invocation && c.kind != ComponentKind::Agent,
                    user_invocable: meta.user_invocable.unwrap_or(true),
                    kind: c.kind.clone(),
                });
            }
        }
        Ok(output)
    }
    pub fn load_skill(&self, id: &str, arguments: &str, automatic: bool) -> Result<LoadedSkill> {
        ensure!(arguments.len() <= 64 * 1024, "Skill arguments too large");
        let (package_id, name) = id
            .split_once(':')
            .context("Use the namespaced skill identifier")?;
        let _lock = self.lock()?;
        let p = self
            .read_catalog()?
            .into_iter()
            .find(|p| p.id == package_id && p.enabled)
            .context("Extension is disabled or removed")?;
        let c = p
            .components
            .iter()
            .find(|c| {
                c.name == name
                    && matches!(
                        c.kind,
                        ComponentKind::Skill | ComponentKind::Command | ComponentKind::Agent
                    )
            })
            .context("Skill not found")?;
        ensure!(
            c.readiness != Readiness::UnsupportedRequirement,
            "Skill has unsupported requirements: {}",
            c.requirements.join(", ")
        );
        let root = self.package_root(&p)?.canonicalize()?;
        let (mut meta, mut raw) = read_prompt(&safe_path(&root, &c.path)?)?;
        ensure!(
            !automatic || !meta.disable_model_invocation,
            "Skill requires explicit user invocation"
        );
        ensure!(
            automatic || meta.user_invocable.unwrap_or(true),
            "Skill is not user-invocable"
        );
        let mut allowed_tools = tool_names(meta.allowed_tools.as_ref().or(meta.tools.as_ref()));
        let mut tools_restricted = meta.allowed_tools.is_some() || meta.tools.is_some();
        if let Some(agent) = meta.agent.as_deref() {
            let (role, instructions) = if let Some(component) = p
                .components
                .iter()
                .find(|item| item.kind == ComponentKind::Agent && item.name == agent)
            {
                ensure!(
                    component.readiness != Readiness::UnsupportedRequirement,
                    "Selected agent has unsupported requirements"
                );
                read_prompt(&safe_path(&root, &component.path)?)?
            } else {
                match agent {
                    "Explore" | "Plan" => (
                        Frontmatter {
                            tools: Some(serde_json::Value::String("Read, Grep, Glob".into())),
                            ..Default::default()
                        },
                        format!(
                            "Act as the {agent} agent. Inspect evidence and report findings; do not change files."
                        ),
                    ),
                    "general-purpose" => (
                        Frontmatter::default(),
                        "Carry out the delegated task using the parent application's permissions."
                            .into(),
                    ),
                    _ => anyhow::bail!("Required agent is unavailable: {agent}"),
                }
            };
            ensure!(
                role.agent.is_none(),
                "Nested agent-role selection is unsupported"
            );
            ensure!(
                role.hooks.is_none(),
                "Agent-scoped hooks require explicit lifecycle setup"
            );
            if role.allowed_tools.is_some() || role.tools.is_some() {
                let role_tools = tool_names(role.allowed_tools.as_ref().or(role.tools.as_ref()));
                allowed_tools = if tools_restricted {
                    allowed_tools
                        .into_iter()
                        .filter(|tool| role_tools.contains(tool))
                        .collect()
                } else {
                    role_tools
                };
                tools_restricted = true;
            }
            if meta.model.as_deref().is_none_or(|model| model == "inherit") {
                meta.model = role.model;
            }
            raw = format!("{instructions}\n\n{raw}");
        }
        // Extract commands before inserting user arguments: arguments can never
        // introduce dynamic execution syntax.
        let dynamic_commands = dynamic_commands(&raw)?;
        ensure!(!raw.contains('\u{1f}'), "Reserved skill control marker");
        let mut template = raw;
        for (i, command) in dynamic_commands.iter().enumerate() {
            template = template.replacen(
                &format!("!`{command}`"),
                &format!("\u{1f}xylo-context-{i}\u{1f}"),
                1,
            );
        }
        let body = substitute(&template, arguments, &root.to_string_lossy());
        Ok(LoadedSkill {
            id: id.into(),
            description: c.description.clone(),
            body,
            package_root: root,
            allowed_tools,
            tools_restricted,
            agent: meta.agent,
            fork: meta.context.as_deref() == Some("fork") || c.kind == ComponentKind::Agent,
            model: meta.model,
            dynamic_commands,
        })
    }
    pub fn read_resource(&self, id: &str, relative: &str) -> Result<String> {
        let _lock = self.lock()?;
        let p = self
            .read_catalog()?
            .into_iter()
            .find(|p| p.id == id && p.enabled)
            .context("Extension is disabled or removed")?;
        let path = safe_path(&self.package_root(&p)?, relative)?;
        ensure!(
            fs::metadata(&path)?.len() <= 256 * 1024,
            "Resource exceeds 256 KiB; use a scoped file tool"
        );
        Ok(fs::read_to_string(path)?)
    }
}
/// Whether the JSON file at `path` decodes as `T`; `None` when it is absent.
pub(super) fn decodes<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<bool>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice::<T>(&bytes).is_ok())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn copy_tree(
    source: &Path,
    target: &Path,
    total: &mut u64,
    count: &mut usize,
    depth: usize,
) -> Result<()> {
    ensure!(depth <= 32, "Package directory depth exceeded");
    fs::create_dir_all(target)?;
    for e in fs::read_dir(source)? {
        let e = e?;
        let name = e.file_name();
        if name == ".git" {
            continue;
        }
        *count += 1;
        ensure!(*count <= 10_000, "Package file count exceeded");
        let t = e.file_type()?;
        ensure!(
            !t.is_symlink(),
            "Symlinks are not allowed in extension packages"
        );
        if t.is_dir() {
            copy_tree(&e.path(), &target.join(name), total, count, depth + 1)?;
        } else {
            ensure!(t.is_file(), "Only regular package files are supported");
            *total += e.metadata()?.len();
            ensure!(*total <= 64 * 1024 * 1024, "Package exceeds 64 MiB");
            fs::copy(e.path(), target.join(name))?;
        }
    }
    Ok(())
}
/// Content digest of a package folder: relative paths, executable bits and bytes.
fn tree_digest(root: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    hash_tree(root, root, &mut hash)?;
    Ok(hex::encode(hash.finalize()))
}
fn hash_tree(root: &Path, dir: &Path, hash: &mut Sha256) -> Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if e.file_type()?.is_dir() {
            hash_tree(root, &e.path(), hash)?;
        } else {
            let path = e.path();
            hash.update(path.strip_prefix(root)?.to_string_lossy().as_bytes());
            hash.update([0]);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                hash.update((fs::metadata(&path)?.permissions().mode() & 0o111).to_le_bytes());
            }
            let bytes = fs::read(path)?;
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
    }
    Ok(())
}
fn dynamic_commands(body: &str) -> Result<Vec<String>> {
    let mut rest = body;
    let mut commands = Vec::new();
    while let Some((_, tail)) = rest.split_once("!`") {
        let (command, tail) = tail
            .split_once('`')
            .context("Unclosed dynamic context command")?;
        ensure!(commands.len() < 16, "Too many dynamic context commands");
        commands.push(command.to_string());
        rest = tail;
    }
    Ok(commands)
}
fn substitute(body: &str, args: &str, root: &str) -> String {
    // Single pass prevents substitutions inside user-provided argument text.
    let words = shell_words::split(args).unwrap_or_else(|_| vec![args.to_owned()]);
    let mut output = String::new();
    let mut rest = body;
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix("${CLAUDE_PLUGIN_ROOT}") {
            output.push_str(root);
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix("$ARGUMENTS") {
            if let Some((index, tail)) = tail.strip_prefix('[').and_then(|s| s.split_once(']'))
                && let Ok(i) = index.parse::<usize>()
            {
                output.push_str(words.get(i).map(String::as_str).unwrap_or(""));
                rest = tail;
                continue;
            }
            output.push_str(args);
            rest = tail;
        } else if rest.starts_with('$') && rest.as_bytes().get(1).is_some_and(u8::is_ascii_digit) {
            let end = rest[1..]
                .find(|c: char| !c.is_ascii_digit())
                .map(|i| i + 1)
                .unwrap_or(rest.len());
            let i = rest[1..end].parse::<usize>().unwrap_or(0);
            output.push_str(words.get(i).map(String::as_str).unwrap_or(""));
            rest = &rest[end..];
        } else {
            let c = rest.chars().next().unwrap();
            output.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    output
}
