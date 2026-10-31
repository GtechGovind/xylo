//! Platform-native path keys. No lossy UTF-8 conversion participates in identity.
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};
/// Filename and SQLite user_version advance together on format changes. A
/// project's catalogues of other versions are deleted when it is opened; they
/// hold no user data.
pub(super) const FORMAT_VERSION: i64 = 1;

#[cfg(unix)]
pub(super) fn encode(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}
#[cfg(unix)]
pub(super) fn decode(bytes: Vec<u8>) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    ensure!(!bytes.contains(&0), "Invalid file path in project index");
    Ok(OsString::from_vec(bytes).into())
}
#[cfg(windows)]
pub(super) fn encode(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_be_bytes)
        .collect()
}
#[cfg(windows)]
pub(super) fn decode(bytes: Vec<u8>) -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    ensure!(
        bytes.len().is_multiple_of(2),
        "Invalid file path in project index"
    );
    let words: Vec<_> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .copied()
        .map(u16::from_be_bytes)
        .collect();
    ensure!(!words.contains(&0), "Invalid file path in project index");
    Ok(OsString::from_wide(&words).into())
}
pub(super) fn database_name(root: &Path) -> String {
    format!(
        "{}.v{FORMAT_VERSION}.sqlite",
        hex::encode(Sha256::digest(encode(root)))
    )
}
/// Strict descendants occupy one binary-key range; `a` cannot alias `ab`.
pub(super) fn descendants(path: &Path) -> (Vec<u8>, Vec<u8>) {
    let mut prefix = encode(&path.join("__workspace_boundary__"));
    prefix.truncate(prefix.len() - encode(Path::new("__workspace_boundary__")).len());
    let mut end = prefix.clone();
    while let Some(last) = end.last_mut() {
        if *last < u8::MAX {
            *last += 1;
            break;
        }
        end.pop();
    }
    (prefix, end)
}

#[cfg(test)]
#[path = "paths/tests.rs"]
mod tests;
