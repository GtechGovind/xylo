//! Preflight before upstream extraction. Signed packages must still have a safe,
//! bounded layout; a signature is not a reason to allow traversal or special files.
use super::*;
use std::{collections::BTreeSet, io::Cursor, path::Component};
pub(super) fn preflight(target: &str, format: &str, bytes: &[u8]) -> Result<()> {
    validate_format(target, format)?;
    if format != "app" {
        return Ok(());
    }
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(Cursor::new(bytes)));
    let mut root: Option<PathBuf> = None;
    let mut entries = 0;
    let mut unpacked = 0u64;
    let mut seen = BTreeSet::new();
    let mut symlinks = BTreeSet::new();
    let mut regular = BTreeSet::new();
    for entry in archive.entries()? {
        let entry = entry?;
        let path = entry.path()?.into_owned();
        let components: Vec<_> = path.components().collect();
        ensure!(
            !components.is_empty()
                && components
                    .iter()
                    .all(|component| matches!(component, Component::Normal(_))),
            "Unsafe update archive path"
        );
        let first = PathBuf::from(components[0].as_os_str());
        ensure!(
            first
                .extension()
                .is_some_and(|extension| extension == "app"),
            "Expected one application bundle"
        );
        if let Some(root) = &root {
            ensure!(root == &first, "Update contains multiple application roots");
        } else {
            root = Some(first);
        }
        ensure!(seen.insert(path.clone()), "Duplicate update archive entry");
        entries += 1;
        unpacked = unpacked
            .checked_add(entry.size())
            .context("Update size overflow")?;
        ensure!(
            entries <= 100_000 && unpacked <= 2 * 1024 * 1024 * 1024,
            "Update archive exceeds extraction limit"
        );
        let kind = entry.header().entry_type();
        ensure!(
            entry.header().mode()? & 0o7000 == 0,
            "Special permissions in update archive"
        );
        if kind.is_file() {
            regular.insert(path.clone());
        }
        ensure!(
            kind.is_file() || kind.is_dir() || kind.is_symlink(),
            "Unsupported special file in update archive"
        );
        for parent in path.ancestors().skip(1) {
            ensure!(!symlinks.contains(parent), "Unsafe symlink ancestor");
        }
        if kind.is_symlink() {
            ensure!(
                !seen
                    .iter()
                    .any(|existing| existing != &path && existing.starts_with(&path)),
                "Symlink replaces an archive directory"
            );
            symlinks.insert(path.clone());
            let link = entry
                .link_name()?
                .context("Missing update symlink target")?;
            ensure!(!link.is_absolute(), "Absolute symlink in update archive");
            let mut depth = components.len() - 1;
            for component in link.components() {
                match component {
                    Component::Normal(_) => depth += 1,
                    Component::CurDir => (),
                    Component::ParentDir => {
                        ensure!(depth > 1, "Symlink escapes application bundle");
                        depth -= 1;
                    }
                    _ => anyhow::bail!("Unsafe symlink in update archive"),
                }
            }
        }
    }
    let root = root.context("Update archive is empty")?;
    for required in [
        "Contents/Info.plist",
        "Contents/MacOS/agent-workbench",
        "Contents/MacOS/workbench-worker",
        "Contents/MacOS/workbench-helper",
    ] {
        ensure!(
            regular.contains(&root.join(required)),
            "Update application is incomplete"
        );
    }
    Ok(())
}
