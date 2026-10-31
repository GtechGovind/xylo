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
