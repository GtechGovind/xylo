//! Platform-native path keys. No lossy UTF-8 conversion participates in identity.
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};
/// Filename and SQLite user_version advance together on format changes. Old
/// catalogues remain untouched; they contain no irreplaceable user data.
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
        "{:x}.v{FORMAT_VERSION}.sqlite",
        Sha256::digest(encode(root))
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
mod tests {
    use super::*;

    fn assert_round_trip(path: &Path) {
        let encoded = encode(path);
        let decoded = decode(encoded.clone()).unwrap();
        assert_eq!(decoded, path);
        assert_eq!(encode(&decoded), encoded);
    }

    #[test]
    fn native_path_keys_preserve_unicode_and_exact_native_spelling() {
        for path in ["", ".", "project/file.rs", "project/你好/e\u{301}🦀.rs"] {
            assert_round_trip(Path::new(path));
        }
    }

    #[test]
    fn invalid_path_keys_never_accept_nul_or_truncate_a_windows_code_unit() {
        assert!(decode(vec![0, 0]).is_err());
        #[cfg(windows)]
        for invalid in [vec![0], vec![0, b'a', 0xff]] {
            assert!(decode(invalid).is_err());
        }
    }

    #[test]
    fn versioned_cache_names_and_subtree_ranges_do_not_alias_siblings() {
        let root = Path::new("project/你好");
        let name = database_name(root);
        assert_eq!(name, database_name(root));
        assert!(name.ends_with(&format!(".v{FORMAT_VERSION}.sqlite")));
        let sibling = root.with_file_name("你好-other");
        assert_ne!(name, database_name(&sibling));
        let (start, end) = descendants(root);
        let child = encode(&root.join("child.rs"));
        assert!(start <= child && child < end);
        for outside in [root.to_path_buf(), sibling] {
            let encoded = encode(&outside);
            assert!(encoded < start || encoded >= end);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_keys_preserve_unpaired_utf16_without_lossy_aliases() {
        use std::os::windows::ffi::OsStringExt;
        let path = PathBuf::from(OsString::from_wide(&[b'f' as u16, 0xd800, b'x' as u16]));
        assert_round_trip(&path);
        assert_ne!(encode(&path), encode(Path::new("f\u{fffd}x")));
    }

    #[cfg(unix)]
    #[test]
    fn unix_path_keys_preserve_non_utf8_without_lossy_aliases() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(OsString::from_vec(vec![b'f', 0xff, b'x']));
        assert_round_trip(&path);
        assert_ne!(encode(&path), encode(Path::new("f\u{fffd}x")));
    }
}
