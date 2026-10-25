use std::{env, fs, path::PathBuf};
fn main() {
    println!("cargo:rerun-if-env-changed=WORKBENCH_UPDATE_POLICY");
    let source = env::var_os("WORKBENCH_UPDATE_POLICY")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packaging/update-policy.json")
        });
    println!("cargo:rerun-if-changed={}", source.display());
    fs::copy(
        source,
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("update-policy.json"),
    )
    .expect("copy immutable update policy");
}
