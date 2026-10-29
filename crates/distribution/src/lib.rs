//! Authenticated cargo-packager updates. Call blocking methods off the UI thread.
//! Both the manifest and installer bytes are signed. Installation is a separate,
//! explicit action after the host has saved edits and stopped running work.
mod archive;
#[cfg(target_os = "macos")]
mod macos;
mod state;
mod transport;

use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cargo_packager_updater::{Config as PackagerConfig, Update, UpdateFormat};
use fs2::FileExt;
use minisign_verify::{PublicKey, Signature};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;
use url::Url;

/// Trusted update configuration. Fresh builds have no configured feed or key.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatePolicy {
    pub enabled: bool,
    pub application_id: String,
    pub channel: String,
    pub manifest_url: Option<Url>,
    pub signature_url: Option<Url>,
    /// Base64 of the complete minisign public-key file, as emitted by packager.
    pub public_key: Option<String>,
    pub allowed_origins: Vec<String>,
    pub max_artifact_bytes: u64,
    #[serde(default)]
    pub macos_team_id: Option<String>,
}
impl Default for UpdatePolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            application_id: "com.qurkos.xylo".into(),
            channel: "stable".into(),
            manifest_url: None,
            signature_url: None,
            public_key: None,
            allowed_origins: Vec::new(),
            max_artifact_bytes: 512 * 1024 * 1024,
            macos_team_id: None,
        }
    }
}
impl UpdatePolicy {
    fn key(&self) -> Result<PublicKey> {
        let bytes = STANDARD.decode(
            self.public_key
                .as_deref()
                .context("Update signing is not configured")?,
        )?;
        Ok(PublicKey::decode(std::str::from_utf8(&bytes)?)?)
    }
    fn validate(&self) -> Result<()> {
        ensure!(self.enabled, "Updates are not configured for this build");
        ensure!(
            !self.application_id.is_empty() && !self.channel.is_empty(),
            "Invalid update policy"
        );
        ensure!(
            self.max_artifact_bytes > 0 && self.max_artifact_bytes <= 2 * 1024 * 1024 * 1024,
            "Invalid update size limit"
        );
        self.key()?;
        for url in [&self.manifest_url, &self.signature_url] {
            self.allow_url(url.as_ref().context("Update feed is not configured")?)?;
        }
        Ok(())
    }
    fn allow_url(&self, url: &Url) -> Result<()> {
        ensure!(
            url.scheme() == "https"
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none(),
            "Update URL must be credential-free HTTPS"
        );
        ensure!(
            self.allowed_origins
                .iter()
                .any(|origin| origin == &url.origin().ascii_serialization()),
            "Update origin is not trusted"
        );
        Ok(())
    }
}
/// Signed release metadata. Sign these exact UTF-8 bytes; do not reserialize feeds.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifest {
    pub schema: u32,
    pub application_id: String,
    pub channel: String,
    pub sequence: u64,
    pub version: Version,
    pub published_at: i64,
    pub expires_at: i64,
    pub notes: String,
    pub platforms: BTreeMap<String, ReleaseArtifact>,
}
/// A platform artifact uses cargo-packager's signature and format values.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseArtifact {
    pub url: Url,
    pub signature: String,
    pub format: String,
    pub sha256: String,
    pub size_bytes: u64,
}
/// Safe product metadata. The host retains the opaque verified candidate.
#[derive(Clone, Debug, Serialize)]
pub struct UpdateSummary {
    pub version: String,
    pub notes: String,
    pub size_bytes: u64,
    pub platform: String,
}
#[derive(Clone, Debug)]
pub struct VerifiedUpdate {
    manifest: ReleaseManifest,
    artifact: ReleaseArtifact,
    manifest_hash: String,
    signature: String,
    bytes: Vec<u8>,
    target: String,
}
impl VerifiedUpdate {
    pub fn summary(&self) -> UpdateSummary {
        UpdateSummary {
            version: self.manifest.version.to_string(),
            notes: self.manifest.notes.clone(),
            size_bytes: self.artifact.size_bytes,
            platform: self.target.clone(),
        }
    }
}
/// Opaque on-disk bytes. Modification is detected again immediately before install.
pub struct StagedUpdate {
    file: NamedTempFile,
    offer: VerifiedUpdate,
}
impl StagedUpdate {
    pub fn summary(&self) -> UpdateSummary {
        self.offer.summary()
    }
    pub fn path(&self) -> &Path {
        self.file.path()
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct UpdateProgress {
    pub phase: &'static str,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
}
/// The OS-specific installer may exit the process (Windows). Persisted intent is
/// reconciled against the compiled version at the next launch.
#[derive(Clone, Debug, Serialize)]
pub struct InstallOutcome {
    pub version: String,
    pub restart_required: bool,
}
/// Blocking, cloneable service suitable for `tokio::task::spawn_blocking`.
#[derive(Clone)]
pub struct Updater {
    policy: UpdatePolicy,
    current: Version,
    target: String,
    data: PathBuf,
    executable: PathBuf,
}
impl Updater {
    pub fn for_application(data: PathBuf, executable: PathBuf) -> Result<Option<Self>> {
        let policy = UpdatePolicy::default();
        if !policy.enabled {
            return Ok(None);
        }
        // Inside an AppImage, current_exe points into its read-only mount.
        #[cfg(target_os = "linux")]
        let executable = std::env::var_os("APPIMAGE")
            .map(PathBuf::from)
            .unwrap_or(executable);
        Ok(Some(Self::new(
            policy,
            Version::parse(env!("CARGO_PKG_VERSION"))?,
            native_target()?,
            data,
            executable,
        )?))
    }
    /// Explicit policy constructor is for trusted application configuration and
    /// isolated integration fixtures, never a remotely supplied document.
    pub fn new(
        policy: UpdatePolicy,
        current: Version,
        target: String,
        data: PathBuf,
        executable: PathBuf,
    ) -> Result<Self> {
        policy.validate()?;
        validate_target(&target)?;
        if target.starts_with("macos-") {
            let team = policy
                .macos_team_id
                .as_deref()
                .context("macOS publisher identity is not configured")?;
            ensure!(
                team.len() == 10
                    && team
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()),
                "Invalid macOS publisher identity"
            );
        }
        fs::create_dir_all(&data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&data, fs::Permissions::from_mode(0o700))?;
        }
        let updater = Self {
            policy,
            current,
            target,
            data: fs::canonicalize(data)?,
            executable,
        };
        let _lock = updater.lock()?;
        updater.reconcile()?;
        Ok(updater)
    }
    fn lock(&self) -> Result<File> {
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.data.join("update.lock"))?;
        FileExt::try_lock_exclusive(&lock).context("Another update action is in progress")?;
        Ok(lock)
    }
    pub fn check(&self, cancel: &CancellationToken) -> Result<Option<VerifiedUpdate>> {
        let _lock = self.lock()?;
        self.reconcile()?;
        let bytes = transport::get_bytes(
            &self.policy,
            self.policy
                .manifest_url
                .as_ref()
                .context("Update feed is not configured")?,
            1024 * 1024,
            cancel,
        )?;
        let signature = String::from_utf8(transport::get_bytes(
            &self.policy,
            self.policy
                .signature_url
                .as_ref()
                .context("Update feed is not configured")?,
            16 * 1024,
            cancel,
        )?)?;
        self.verify_manifest(&bytes, &signature, now()?)
    }
    /// Verify signature before parsing any attacker-controlled release metadata.
    pub fn verify_manifest(
        &self,
        bytes: &[u8],
        signature: &str,
        at: i64,
    ) -> Result<Option<VerifiedUpdate>> {
        ensure!(
            bytes.len() <= 1024 * 1024 && signature.len() <= 16 * 1024,
            "Update metadata is too large"
        );
        self.policy
            .key()?
            .verify(bytes, &decode_signature(signature)?, false)
            .context("Release metadata signature is invalid")?;
        let manifest: ReleaseManifest = serde_json::from_slice(bytes)?;
        ensure!(
            self.policy.channel != "stable" || manifest.version.pre.is_empty(),
            "A prerelease cannot be installed from the stable channel"
        );
        ensure!(
            manifest.schema == 1
                && manifest.application_id == self.policy.application_id
                && manifest.channel == self.policy.channel,
            "Release identity or channel does not match"
        );
        ensure!(
            manifest.sequence > 0
                && manifest.published_at <= at.saturating_add(300)
                && manifest.expires_at > at
                && manifest.expires_at > manifest.published_at,
            "Release metadata has expired or has an invalid time"
        );
        let lifetime = manifest
            .expires_at
            .checked_sub(manifest.published_at)
            .context("Release metadata has an invalid time")?;
        ensure!(
            lifetime <= 31 * 86400 && manifest.notes.len() <= 65536,
            "Release metadata exceeds policy limits"
        );
        let state = self.load_state()?;
        let floor = state
            .highest_version
            .as_ref()
            .unwrap_or(&self.current)
            .max(&self.current);
        ensure!(
            &manifest.version >= floor,
            "An older application version cannot be installed"
        );
        ensure!(
            manifest.sequence >= state.highest_sequence,
            "Release sequence cannot move backwards"
        );
        let manifest_hash = hex(&Sha256::digest(bytes));
        if manifest.sequence == state.highest_sequence && state.highest_sequence > 0 {
            ensure!(
                state.highest_manifest.as_deref() == Some(&manifest_hash),
                "Release sequence was reused for different metadata"
            );
        }
        let artifact = manifest
            .platforms
            .get(&self.target)
            .context("This release is not available for this computer")?
            .clone();
        self.policy.allow_url(&artifact.url)?;
        ensure!(
            artifact.size_bytes > 0 && artifact.size_bytes <= self.policy.max_artifact_bytes,
            "Update exceeds the download limit"
        );
        ensure!(
            artifact.sha256.len() == 64
                && artifact
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "Invalid artifact digest"
        );
        validate_format(&self.target, &artifact.format)?;
        decode_signature(&artifact.signature)?;
        if manifest.version == self.current {
            return Ok(None);
        }
        Ok(Some(VerifiedUpdate {
            manifest,
            artifact,
            manifest_hash,
            signature: signature.to_owned(),
            bytes: bytes.to_vec(),
            target: self.target.clone(),
        }))
    }
    pub fn stage(
        &self,
        offer: &VerifiedUpdate,
        cancel: &CancellationToken,
        progress: impl Fn(UpdateProgress),
    ) -> Result<StagedUpdate> {
        let _lock = self.lock()?;
        self.revalidate(offer)?;
        ensure!(!cancel.is_cancelled(), "Update cancelled");
        let mut file = NamedTempFile::new_in(&self.data)?;
        transport::download(&self.policy, &offer.artifact, &mut file, cancel, &progress)?;
        file.as_file_mut().sync_all()?;
        self.verify_file(file.path(), &offer.artifact, cancel)?;
        progress(UpdateProgress {
            phase: "ready",
            downloaded_bytes: offer.artifact.size_bytes,
            total_bytes: offer.artifact.size_bytes,
        });
        Ok(StagedUpdate {
            file,
            offer: offer.clone(),
        })
    }
    /// Verify an existing release artifact without installing it. Used by the
    /// offline release tooling to catch a signing-key/configuration mismatch.
    pub fn verify_artifact(
        &self,
        offer: &VerifiedUpdate,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.revalidate(offer)?;
        self.verify_file(path, &offer.artifact, cancel)
    }
    /// Explicit install boundary. The host MUST first save edits and shut down
    /// tasks/helpers. Windows installation can terminate this process.
    pub fn install(
        &self,
        staged: StagedUpdate,
        cancel: &CancellationToken,
    ) -> Result<InstallOutcome> {
        self.install_staged(&staged, cancel)
    }
    /// Borrowed variant lets the host retain a verified download when native
    /// installation fails. Bytes and metadata are reverified for every attempt.
    pub fn install_staged(
        &self,
        staged: &StagedUpdate,
        cancel: &CancellationToken,
    ) -> Result<InstallOutcome> {
        let _lock = self.lock()?;
        self.revalidate(&staged.offer)?;
        ensure!(!cancel.is_cancelled(), "Update cancelled");
        let bytes = read_bounded(staged.path(), staged.offer.artifact.size_bytes)?;
        verify_bytes(&self.policy, &staged.offer.artifact, &bytes)?;
        let extract_path = install_path(&self.target, &self.executable)?;
        archive::preflight(&self.target, &staged.offer.artifact.format, &bytes)?;
        let mut state = self.load_state()?;
        state.pending = Some(state::Pending {
            version: staged.offer.manifest.version.clone(),
            sequence: staged.offer.manifest.sequence,
            manifest: staged.offer.manifest_hash.clone(),
        });
        self.save_state(&state)?; // Windows' installer exits; next launch reconciles.
        let update = Update {
            config: PackagerConfig {
                endpoints: vec![staged.offer.artifact.url.clone()],
                pubkey: self
                    .policy
                    .public_key
                    .clone()
                    .context("Update signing is not configured")?,
                windows: None,
            },
            body: Some(staged.offer.manifest.notes.clone()),
            current_version: self.current.to_string(),
            version: staged.offer.manifest.version.to_string(),
            date: None,
            target: self.target.clone(),
            extract_path,
            download_url: staged.offer.artifact.url.clone(),
            signature: staged.offer.artifact.signature.clone(),
            timeout: Some(Duration::from_secs(30)),
            headers: Default::default(),
            format: packager_format(&staged.offer.artifact.format)?,
        };
        #[cfg(target_os = "macos")]
        let result = macos::install(
            self,
            &bytes,
            &update.extract_path,
            &staged.offer.manifest.version,
            cancel,
        );
        #[cfg(not(target_os = "macos"))]
        let result = update
            .install(bytes)
            .context("The application update could not be installed");
        // Keep intent on error: replacement may have succeeded just before a
        // durability error. Only the next running version can reconcile it.
        result?;
        state.accept_pending();
        self.save_state(&state)?;
        Ok(InstallOutcome {
            version: staged.offer.manifest.version.to_string(),
            restart_required: true,
        })
    }
    fn revalidate(&self, offer: &VerifiedUpdate) -> Result<()> {
        ensure!(
            offer.target == self.target,
            "Update belongs to a different platform"
        );
        ensure!(
            self.verify_manifest(&offer.bytes, &offer.signature, now()?)?
                .is_some(),
            "This update is no longer applicable"
        );
        Ok(())
    }
    fn verify_file(
        &self,
        path: &Path,
        artifact: &ReleaseArtifact,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let mut file = File::open(path)?;
        let mut digest = Sha256::new();
        let signature = decode_signature(&artifact.signature)?;
        let key = self.policy.key()?;
        let mut verifier = key.verify_stream(&signature)?;
        let mut total = 0;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            ensure!(!cancel.is_cancelled(), "Update cancelled");
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total += count as u64;
            ensure!(total <= artifact.size_bytes, "Update size does not match");
            digest.update(&buffer[..count]);
            verifier.update(&buffer[..count]);
        }
        ensure!(
            total == artifact.size_bytes && hex(&digest.finalize()) == artifact.sha256,
            "Update digest or size does not match"
        );
        verifier.finalize().context("Update signature is invalid")?;
        Ok(())
    }
}
fn decode_signature(encoded: &str) -> Result<Signature> {
    Ok(Signature::decode(std::str::from_utf8(
        &STANDARD.decode(encoded.trim())?,
    )?)?)
}
fn verify_bytes(policy: &UpdatePolicy, artifact: &ReleaseArtifact, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() as u64 == artifact.size_bytes && hex(&Sha256::digest(bytes)) == artifact.sha256,
        "Update digest or size does not match"
    );
    policy
        .key()?
        .verify(bytes, &decode_signature(&artifact.signature)?, false)
        .context("Update signature is invalid")?;
    Ok(())
}
fn read_bounded(path: &Path, size: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(size + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 == size, "Update size does not match");
    Ok(bytes)
}
pub fn native_target() -> Result<String> {
    cargo_packager_updater::target().context("Updates are not supported on this platform")
}
fn validate_target(target: &str) -> Result<()> {
    ensure!(
        matches!(
            target,
            "macos-aarch64"
                | "macos-x86_64"
                | "windows-x86_64"
                | "windows-aarch64"
                | "linux-x86_64"
                | "linux-aarch64"
        ),
        "Unsupported update platform"
    );
    Ok(())
}
fn validate_format(target: &str, format: &str) -> Result<()> {
    ensure!(
        (target.starts_with("macos-") && format == "app")
            || (target.starts_with("linux-") && format == "appimage")
            || (target.starts_with("windows-") && matches!(format, "nsis" | "wix")),
        "Update format does not match this computer"
    );
    Ok(())
}
fn packager_format(format: &str) -> Result<UpdateFormat> {
    Ok(match format {
        "app" => UpdateFormat::App,
        "appimage" => UpdateFormat::AppImage,
        "nsis" => UpdateFormat::Nsis,
        "wix" => UpdateFormat::Wix,
        _ => anyhow::bail!("Unsupported update format"),
    })
}
fn install_path(target: &str, executable: &Path) -> Result<PathBuf> {
    ensure!(
        target == native_target()?,
        "Cannot install an update for another platform"
    );
    let executable = fs::canonicalize(executable)?;
    if target.starts_with("macos-") {
        let macos = executable.parent().context("Invalid app location")?;
        let contents = macos.parent().context("Invalid app location")?;
        let app = contents.parent().context("Invalid app location")?;
        ensure!(
            macos.file_name().is_some_and(|n| n == "MacOS")
                && contents.file_name().is_some_and(|n| n == "Contents")
                && app.extension().is_some_and(|n| n == "app"),
            "Install updates from the application bundle"
        );
        Ok(app.to_path_buf())
    } else if target.starts_with("linux-") {
        ensure!(
            executable.extension().is_some_and(|n| n == "AppImage"),
            "Use the system package manager to update this installation"
        );
        Ok(executable)
    } else {
        Ok(executable
            .parent()
            .context("Invalid app location")?
            .to_path_buf())
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn now() -> Result<i64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64)
}
/// Product-safe summary; preserve the source error in engineering logs only.
pub fn product_error(error: &anyhow::Error) -> &'static str {
    let text = error.to_string().to_lowercase();
    if text.contains("cancel") {
        "Update cancelled."
    } else if text.contains("signature") || text.contains("digest") {
        "This update could not be verified. Please try again later."
    } else if text.contains("older") || text.contains("sequence") {
        "This update is older than the installed version."
    } else if text.contains("configured") {
        "Updates are not configured for this build."
    } else {
        "The update could not finish. Please try again later."
    }
}

#[cfg(test)]
mod tests;
