//! Extraction never trusts an archive's path, file type, permissions or claimed
//! size. Links are materialized as bounded regular copies inside the staging root.

use super::catalog::{Download, Format, safe_relative};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

const MAX_ENTRIES: usize = 20_000;
const MAX_EXPANDED: u64 = 4 * 1024 * 1024 * 1024;
const MAX_ENTRY: u64 = 2 * 1024 * 1024 * 1024;

fn target(root: &Path, path: &Path) -> Result<PathBuf> {
    use std::path::Component;
    ensure!(
        path.to_str().is_some_and(|p| !p.contains('\\')),
        "Invalid archive path encoding"
    );
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => normalized.push(name),
            Component::CurDir => {}
            _ => anyhow::bail!("Unsafe archive path"),
        }
    }
    Ok(root.join(normalized))
}

fn copy_bounded(
    reader: &mut impl Read,
    destination: &Path,
    expected: u64,
    total: &mut u64,
    cancel: &CancellationToken,
) -> Result<()> {
    ensure!(
        expected <= MAX_ENTRY && total.saturating_add(expected) <= MAX_EXPANDED,
        "Archive expands beyond asset budget"
    );
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    let mut buffer = vec![0; 128 * 1024];
    let mut copied = 0u64;
    loop {
        ensure!(!cancel.is_cancelled(), "Asset extraction cancelled");
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        copied += n as u64;
        ensure!(copied <= expected, "Archive entry exceeds declared size");
        output.write_all(&buffer[..n])?;
    }
    ensure!(copied == expected, "Archive entry is truncated");
    *total += copied;
    Ok(())
}

pub(super) fn extract(
    blob: &Path,
    spec: &Download,
    staging: &Path,
    cancel: &CancellationToken,
) -> Result<()> {
    let destination = target(staging, Path::new(&spec.destination))?;
    if matches!(spec.format, Format::Gzip) {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut input = flate2::read::GzDecoder::new(File::open(blob)?);
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(destination)?;
        let mut buffer = vec![0; 128 * 1024];
        let mut total = 0u64;
        loop {
            ensure!(!cancel.is_cancelled(), "Asset extraction cancelled");
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total += count as u64;
            ensure!(
                total <= 256 * 1024 * 1024,
                "Native binary exceeds expanded byte budget"
            );
            output.write_all(&buffer[..count])?;
        }
        ensure!(total > 0, "Empty compressed native binary");
        return Ok(());
    }
    if matches!(spec.format, Format::Raw) {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Cache and installation live on the same app-owned filesystem. A hard
        // link avoids duplicating multi-GB verified weights; copy is a fallback.
        if std::fs::hard_link(blob, &destination).is_err() {
            std::fs::copy(blob, &destination)?;
        }
        return Ok(());
    }
    std::fs::create_dir_all(&destination)?;
    let input = File::open(blob)?;
    let mut total = 0;
    match spec.format {
        Format::Zip => {
            let mut archive = zip::ZipArchive::new(input)?;
            ensure!(archive.len() <= MAX_ENTRIES, "Archive has too many entries");
            for index in 0..archive.len() {
                ensure!(!cancel.is_cancelled(), "Asset extraction cancelled");
                let mut entry = archive.by_index(index)?;
                let path = entry
                    .enclosed_name()
                    .context("Archive path escapes staging")?;
                let output = target(&destination, &path)?;
                if entry.is_dir() {
                    std::fs::create_dir_all(output)?;
                    continue;
                }
                if let Some(mode) = entry.unix_mode() {
                    ensure!(
                        mode & 0o170000 == 0 || mode & 0o170000 == 0o100000,
                        "Archive contains unsupported special/link entry"
                    );
                }
                let size = entry.size();
                copy_bounded(&mut entry, &output, size, &mut total, cancel)?;
            }
        }
        Format::TarGz => extract_tar(
            flate2::read::GzDecoder::new(input),
            &destination,
            &mut total,
            cancel,
        )?,
        Format::TarBz2 => extract_tar(
            bzip2::read::BzDecoder::new(input),
            &destination,
            &mut total,
            cancel,
        )?,
        Format::Raw | Format::Gzip => unreachable!(),
    }
    Ok(())
}

fn extract_tar(
    reader: impl Read,
    root: &Path,
    total: &mut u64,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    let mut links = Vec::new();
    for (index, entry) in archive.entries()?.enumerate() {
        ensure!(index < MAX_ENTRIES, "Archive has too many entries");
        ensure!(!cancel.is_cancelled(), "Asset extraction cancelled");
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let output = target(root, &path)?;
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            std::fs::create_dir_all(output)?;
        } else if kind.is_file() {
            let size = entry.size();
            copy_bounded(&mut entry, &output, size, total, cancel)?;
        } else if kind.is_symlink() || kind.is_hard_link() {
            let link = entry
                .link_name()?
                .context("Archive link has no target")?
                .into_owned();
            ensure!(
                safe_relative(link.to_str().context("Invalid archive link encoding")?),
                "Archive link escapes staging"
            );
            let source = if kind.is_symlink() {
                output
                    .parent()
                    .context("Invalid archive link parent")?
                    .join(link)
            } else {
                target(root, &link)?
            };
            links.push((output, source));
        } else {
            anyhow::bail!("Archive contains a special device or unsupported entry");
        }
    }
    // Resolve only existing regular files; directories, dangling links, link
    // cycles and anything outside staging are refused rather than followed.
    let canonical_root = std::fs::canonicalize(root)?;
    for _ in 0..32 {
        if links.is_empty() {
            return Ok(());
        }
        let mut deferred = Vec::new();
        let mut resolved = 0;
        for (destination, source) in links {
            if !source.exists() {
                deferred.push((destination, source));
                continue;
            }
            let source = std::fs::canonicalize(source)?;
            ensure!(
                source.starts_with(&canonical_root) && source.is_file(),
                "Archive link target escapes staging"
            );
            let size = source.metadata()?.len();
            copy_bounded(&mut File::open(source)?, &destination, size, total, cancel)?;
            resolved += 1;
        }
        ensure!(resolved > 0, "Archive has dangling or cyclic links");
        links = deferred;
    }
    ensure!(links.is_empty(), "Archive link chain exceeds budget");
    Ok(())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(super) struct InstalledFile {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub sha256: String,
}

pub(super) fn manifest(root: &Path, cancel: &CancellationToken) -> Result<Vec<InstalledFile>> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    let mut total = 0u64;
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            ensure!(!cancel.is_cancelled(), "Asset verification cancelled");
            let entry = entry?;
            let kind = entry.file_type()?;
            ensure!(
                !kind.is_symlink(),
                "Installed asset contains an unexpected symlink"
            );
            if kind.is_dir() {
                pending.push(entry.path());
                continue;
            }
            ensure!(
                kind.is_file() && files.len() < MAX_ENTRIES,
                "Installed asset contains special files or too many files"
            );
            let metadata = entry.metadata()?;
            total = total.saturating_add(metadata.len());
            ensure!(
                total <= 16 * 1024 * 1024 * 1024,
                "Installed asset exceeds byte budget"
            );
            let mut input = File::open(entry.path())?;
            let mut hash = Sha256::new();
            let mut buffer = vec![0; 1024 * 1024];
            loop {
                ensure!(!cancel.is_cancelled(), "Asset verification cancelled");
                let n = input.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
            }
            files.push(InstalledFile {
                path: entry.path().strip_prefix(root)?.to_path_buf(),
                size_bytes: metadata.len(),
                sha256: hex::encode(hash.finalize()),
            });
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

#[cfg(test)]
mod tests;
