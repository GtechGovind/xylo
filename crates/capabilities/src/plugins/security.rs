use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, io::Read, path::Path};
use tokio_util::sync::CancellationToken;
use xylo_core::NativePluginManifest;

#[cfg(windows)]
#[path = "windows_read.rs"]
mod windows_read;

#[cfg(windows)]
pub(super) use windows_read::open_workspace_read;

/// Open one authorized workspace file and return that same handle to the
/// bounded reader. Every namespace lookup is relative to an already opened
/// directory, so concurrent path/ancestor substitutions cannot redirect IO.
#[cfg(unix)]
pub(super) fn open_workspace_read(
    config: &crate::CapabilityConfig,
    raw: &str,
    cancel: &CancellationToken,
) -> Result<std::fs::File> {
    use std::{ffi::OsString, path::Component};
    ensure!(
        raw.len() <= 4096,
        "Workspace file path exceeds its byte budget"
    );
    checked(cancel)?;
    let requested = Path::new(raw);
    let relative = if requested.is_absolute() {
        requested
            .strip_prefix(&config.workspace)
            .map_err(|_| anyhow::anyhow!("Plugin reads are restricted to the selected workspace"))?
    } else {
        requested
    };
    let mut parts = Vec::<OsString>::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => parts.push(name.to_owned()),
            Component::CurDir => {}
            _ => anyhow::bail!(
                "Workspace file paths cannot traverse outside their directory authority"
            ),
        }
    }
    ensure!(
        !parts.is_empty() && parts.len() <= 256,
        "Workspace file path has an unsupported depth"
    );
    let mut directory = open_directory_authority(&config.workspace, cancel)?;
    let private = config
        .management_root
        .as_ref()
        .and_then(|management| management.parent())
        .map(|path| open_directory_authority(path, cancel))
        .transpose()?;
    reject_private_directory(&directory, private.as_ref())?;
    for part in &parts[..parts.len() - 1] {
        checked(cancel)?;
        directory = open_relative(&directory, part, true)?;
        reject_private_directory(&directory, private.as_ref())?;
    }
    #[cfg(test)]
    super::tests::before_workspace_read_open(&config.workspace)?;
    checked(cancel)?;
    open_relative(&directory, parts.last().unwrap(), false)
}

#[cfg(unix)]
fn open_directory_authority(path: &Path, cancel: &CancellationToken) -> Result<std::fs::File> {
    use std::{os::unix::fs::OpenOptionsExt, path::Component};
    ensure!(
        path.is_absolute(),
        "Workspace directory authority must be absolute"
    );
    let mut directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open("/")?;
    for part in path.components() {
        checked(cancel)?;
        match part {
            Component::RootDir => {}
            Component::Normal(name) => directory = open_relative(&directory, name, true)?,
            _ => anyhow::bail!("Workspace directory authority cannot contain traversal or links"),
        }
    }
    Ok(directory)
}

#[cfg(unix)]
fn reject_private_directory(
    directory: &std::fs::File,
    private: Option<&std::fs::File>,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if let Some(private) = private {
        let candidate = directory.metadata()?;
        let forbidden = private.metadata()?;
        ensure!(
            candidate.dev() != forbidden.dev() || candidate.ino() != forbidden.ino(),
            "Application data files are outside plugin read access"
        );
    }
    Ok(())
}

#[cfg(unix)]
fn open_relative(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    directory: bool,
) -> Result<std::fs::File> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{ffi::OsStrExt, fs::MetadataExt},
        },
    };
    let name = CString::new(name.as_bytes())?;
    let flags = libc::O_RDONLY
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | libc::O_CLOEXEC
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: parent is an owned live directory handle and name is a NUL-terminated
    // single path component. A successful descriptor is immediately owned below.
    let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor < 0 {
        anyhow::bail!(
            "Workspace file could not be opened safely: {}",
            std::io::Error::last_os_error()
        );
    }
    // SAFETY: openat returned a new descriptor whose ownership transfers once.
    let file = unsafe { std::fs::File::from_raw_fd(descriptor) };
    let metadata = file.metadata()?;
    if directory {
        ensure!(
            metadata.is_dir(),
            "Workspace path component must be a directory"
        );
    } else {
        ensure!(
            metadata.is_file() && metadata.nlink() == 1,
            "Plugin reads require a regular workspace file without links"
        );
    }
    Ok(file)
}

/// A path comparison after opening is insufficient to protect directory races.
/// Keep unsupported platforms closed until they have an equivalent
/// relative-handle implementation.
#[cfg(not(any(unix, windows)))]
pub(super) fn open_workspace_read(
    _config: &crate::CapabilityConfig,
    _raw: &str,
    _cancel: &CancellationToken,
) -> Result<std::fs::File> {
    anyhow::bail!(
        "Plugin workspace reading is unavailable on this platform until safe file-handle access is supported"
    )
}

pub(super) fn checked(cancel: &CancellationToken) -> Result<()> {
    ensure!(!cancel.is_cancelled(), "Plugin operation cancelled");
    Ok(())
}

pub(super) fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
pub(super) fn is_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) fn bounded_read(path: &Path, maximum: usize) -> Result<Vec<u8>> {
    ensure!(
        std::fs::symlink_metadata(path)?.is_file(),
        "Plugin input must be a regular non-symlink file"
    );
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "Plugin input must be a regular file"
    );
    ensure!(
        file.metadata()?.len() <= maximum as u64,
        "Plugin file exceeds its byte budget"
    );
    let mut bytes = Vec::new();
    (&mut file)
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= maximum,
        "Plugin file grew beyond its byte budget"
    );
    Ok(bytes)
}

pub(super) fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 64
            && id.bytes().all(|byte| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.'))
            && !id.starts_with('.')
            && !id.ends_with('.')
            && !id.contains(".."),
        "Plugin ID must contain 1–64 lowercase letters, digits, '.', '_' or '-'"
    );
    let stem = id.split('.').next().unwrap_or(id);
    ensure!(
        !matches!(stem, "con" | "prn" | "aux" | "nul")
            && !(stem.len() == 4
                && (stem.starts_with("com") || stem.starts_with("lpt"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9')),
        "Plugin ID is reserved by the operating system"
    );
    Ok(())
}

pub(super) fn validate_capability(capability: &str) -> Result<()> {
    if matches!(capability, "read_workspace" | "write_artifacts") {
        return Ok(());
    }
    let host = capability.strip_prefix("https:").unwrap_or("");
    ensure!(
        !host.is_empty()
            && host.len() <= 253
            && host == host.to_ascii_lowercase()
            && host.split('.').all(|label| !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')),
        "Unknown plugin capability: {capability}; HTTPS grants require an exact lowercase host"
    );
    let url = url::Url::parse(&format!("https://{host}/"))?;
    ensure!(
        url.host_str() == Some(host),
        "HTTPS host capability is not canonical"
    );
    Ok(())
}

pub(super) fn validate_grants(grants: &[String], requested: &[String]) -> Result<()> {
    ensure!(
        grants.len() <= 32 && grants.iter().collect::<BTreeSet<_>>().len() == grants.len(),
        "Plugin grants must contain at most 32 unique capabilities"
    );
    for grant in grants {
        validate_capability(grant)?;
        ensure!(
            requested.contains(grant),
            "Permission was not requested by the plugin package: {grant}"
        );
    }
    Ok(())
}

pub(super) fn validate_manifest(manifest: &NativePluginManifest) -> Result<()> {
    ensure!(manifest.format == 1, "Unsupported plugin manifest format");
    validate_id(&manifest.id)?;
    ensure!(
        !manifest.name.trim().is_empty()
            && manifest.name.len() <= 120
            && !manifest.name.chars().any(char::is_control),
        "Plugin name must contain 1–120 display characters"
    );
    ensure!(
        !manifest.version.is_empty()
            && manifest.version.len() <= 64
            && manifest
                .version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+')),
        "Plugin version must be a bounded version identifier"
    );
    ensure!(
        manifest.entrypoint.len() <= 512,
        "Plugin entrypoint path exceeds its budget"
    );
    ensure!(
        is_hash(&manifest.sha256),
        "Plugin manifest requires a lowercase SHA256 checksum"
    );
    validate_grants(&manifest.capabilities, &manifest.capabilities)?;
    let budget = &manifest.budgets;
    ensure!(
        (8..=256).contains(&budget.memory_mib)
            && (1..=100_000_000).contains(&budget.fuel)
            && (1..=60).contains(&budget.timeout_seconds)
            && (1..=1024 * 1024).contains(&budget.max_input_bytes)
            && (1..=1024 * 1024).contains(&budget.max_output_bytes),
        "Plugin resource budgets exceed the supported limits"
    );
    Ok(())
}
