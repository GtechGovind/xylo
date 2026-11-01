//! Classifying reparse points and parsing the `REPARSE_DATA_BUFFER` that
//! `FSCTL_GET_REPARSE_POINT` returns.
//!
//! Only name-surrogate tags redirect a name to another one, so only they are
//! links. Of those, symbolic links and mount points (directory junctions) are
//! walked; any other surrogate is refused, so an unknown redirection is never
//! treated as a plain file. Other tags (OneDrive and cloud-file placeholders,
//! dedup, compression) keep their data under the name itself and are read
//! as ordinary files and directories, unless reading would first download
//! or recall them. Kept free of Windows APIs so the rules and byte layout
//! are tested on every platform.

use anyhow::{Context, Result, bail, ensure};

/// `IO_REPARSE_TAG_SYMLINK`.
pub(super) const SYMLINK: u32 = 0xA000_000C;
/// `IO_REPARSE_TAG_MOUNT_POINT` (directory junctions and volume mounts).
pub(super) const MOUNT_POINT: u32 = 0xA000_0003;
/// `FILE_ATTRIBUTE_DEVICE`.
const ATTRIBUTE_DEVICE: u32 = 0x40;
/// `FILE_ATTRIBUTE_REPARSE_POINT`.
const ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// `FILE_ATTRIBUTE_OFFLINE`: the data lives in remote storage.
const ATTRIBUTE_OFFLINE: u32 = 0x1000;
/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`: reading the data downloads it,
/// as for an online-only cloud file.
const ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
/// The tag bit (`IsReparseTagNameSurrogate`) marking a reparse point that
/// names another file or directory.
const NAME_SURROGATE: u32 = 0x2000_0000;
/// `SYMLINK_FLAG_RELATIVE`: the substitute name is relative to the link's
/// directory.
const SYMLINK_FLAG_RELATIVE: u32 = 1;
/// The NT object-manager prefix of absolute targets, `\??\`.
const NT_PREFIX: [u16; 4] = [92, 63, 63, 92];
/// The Win32 spelling of the same namespace, `\\?\`.
const VERBATIM_PREFIX: [u16; 4] = [92, 92, 63, 92];

/// How the walk treats an object it opened without following it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// A name surrogate: read its target and walk that instead.
    Link,
    /// Data under this name, reparse point or not: use the open handle.
    Plain,
}

/// Classifies an object from its `FileAttributeTagInfo` attributes and tag.
/// Devices, and data that would have to be downloaded or recalled first,
/// are refused whatever the tag.
pub(super) fn classify(attributes: u32, tag: u32) -> Result<Kind> {
    ensure!(
        attributes & (ATTRIBUTE_DEVICE | ATTRIBUTE_OFFLINE | ATTRIBUTE_RECALL_ON_DATA_ACCESS) == 0,
        "Input cannot be a device or a file that is not available locally"
    );
    Ok(
        if attributes & ATTRIBUTE_REPARSE_POINT != 0 && tag & NAME_SURROGATE != 0 {
            Kind::Link
        } else {
            Kind::Plain
        },
    )
}

/// Where a link points, as UTF-16 in Win32 spelling: a relative symlink's
/// target unchanged, an absolute target with `\??\` rewritten to `\\?\` so
/// it parses as a verbatim path.
pub(super) fn target(buffer: &[u8]) -> Result<Vec<u16>> {
    let tag = u32_at(buffer, 0)?;
    let length = usize::from(u16_at(buffer, 4)?);
    let data = buffer
        .get(8..8 + length)
        .context("Input link data is truncated")?;
    let (names, relative) = match tag {
        SYMLINK => (12, u32_at(data, 8)? & SYMLINK_FLAG_RELATIVE != 0),
        MOUNT_POINT => (8, false),
        _ => bail!("Input crosses an unsupported link (reparse tag 0x{tag:08x})"),
    };
    let offset = usize::from(u16_at(data, 0)?);
    let size = usize::from(u16_at(data, 2)?);
    ensure!(
        offset % 2 == 0 && size % 2 == 0 && size > 0,
        "Input link target is malformed"
    );
    let units: Vec<u16> = data
        .get(names..)
        .and_then(|names| names.get(offset..offset + size))
        .context("Input link target is truncated")?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|unit| u16::from_le_bytes(*unit))
        .collect();
    if relative {
        return Ok(units);
    }
    let rest = units
        .strip_prefix(&NT_PREFIX[..])
        .context("Input link target is not a local absolute path")?;
    Ok(VERBATIM_PREFIX.iter().chain(rest).copied().collect())
}

fn u16_at(buffer: &[u8], offset: usize) -> Result<u16> {
    let bytes = buffer
        .get(offset..offset + 2)
        .context("Input link data is truncated")?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn u32_at(buffer: &[u8], offset: usize) -> Result<u32> {
    let bytes = buffer
        .get(offset..offset + 4)
        .context("Input link data is truncated")?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}
