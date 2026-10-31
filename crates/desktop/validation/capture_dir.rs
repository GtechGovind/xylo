//! Where native captures are written. Each harness owns one directory under
//! the validation crate's `target/native-preview` (or under `XYLO_RENDER_DIR`
//! when set) and clears it at start, so a rerun never meets the evidence of an
//! earlier run and an overwrite guard only catches two captures of one run.
use std::path::PathBuf;

/// Returns the empty capture directory owned by `harness`.
pub fn fresh(harness: &str) -> PathBuf {
    let output = std::env::var_os("XYLO_RENDER_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/native-preview"))
        .join(harness);
    match std::fs::remove_dir_all(&output) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("Clear {}: {error}", output.display()),
    }
    std::fs::create_dir_all(&output)
        .unwrap_or_else(|error| panic!("Create {}: {error}", output.display()));
    output
}
