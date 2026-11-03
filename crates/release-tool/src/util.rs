use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

pub type Environment = std::collections::BTreeMap<String, String>;

pub fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("Missing string field: {key}"))
}

pub fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("Invalid JSON: {}", path.display()))
}

/// Do not format Command or child stderr: signing tools may echo credentials.
pub fn output(command: &mut Command, label: &str) -> Result<String> {
    let result = command
        .output()
        .with_context(|| format!("Could not start {label}"))?;
    ensure!(
        result.status.success(),
        "{label} failed ({}); command output suppressed to protect signing secrets",
        result.status
    );
    Ok(String::from_utf8(result.stdout)?)
}

pub fn checked(command: &mut Command, label: &str) -> Result<()> {
    output(command, label).map(|_| ())
}

pub fn sha256(path: &Path) -> Result<String> {
    let mut digest = Sha256::new();
    let mut file = fs::File::open(path)?;
    // The Windows release-tool main thread has a 1 MiB stack. Keep the
    // streaming chunk on the heap so package checksums fit that stack too.
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// Never traverse directory symlinks (including links inside an app bundle).
pub fn walk(root: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    if !root.exists() {
        return Ok(paths);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        paths.push(path.clone());
        if entry.file_type()?.is_dir() {
            paths.extend(walk(&path)?);
        }
    }
    paths.sort();
    Ok(paths)
}

pub fn exactly_one(root: &Path, suffix: &str) -> Result<PathBuf> {
    let paths: Vec<_> = walk(root)?
        .into_iter()
        .filter(|p| {
            p.file_name()
                .is_some_and(|s| s.to_string_lossy().ends_with(suffix))
        })
        .collect();
    ensure!(
        paths.len() == 1,
        "Expected one {suffix} release package, found {}",
        paths.len()
    );
    Ok(paths[0].clone())
}

pub fn reset_directory(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    fs::create_dir_all(path)?;
    Ok(())
}

pub fn sidecar(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".sig");
    name.into()
}

pub fn executable(path: &Path) -> Result<()> {
    ensure!(
        path.is_file(),
        "Package is missing executable {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            path.metadata()?.permissions().mode() & 0o111 != 0,
            "Package file is not executable: {}",
            path.display()
        );
    }
    Ok(())
}

pub fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = path.metadata()?.permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        fs::set_permissions(path, permissions)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub fn platform_name(system: &str, machine: &str) -> Result<(&'static str, &'static str)> {
    let platform = match system {
        "macos" | "Darwin" => "macos",
        "windows" | "Windows" => "windows",
        "linux" | "Linux" => "linux",
        _ => anyhow::bail!("Unsupported release platform"),
    };
    let arch = match machine.to_ascii_lowercase().as_str() {
        "arm64" | "aarch64" => "arm64",
        "amd64" | "x86_64" => "x64",
        _ => anyhow::bail!("Unsupported release architecture"),
    };
    Ok((platform, arch))
}
