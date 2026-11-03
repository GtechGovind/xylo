// Shared by test modules with include!; never linked into the application.
// Also type-check the standalone source and run its JSON wire-value tests as
// part of the normal Cargo test harness, not just when rustc builds a fixture.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

/// Compile each std-only protocol fixture once per test binary, retaining its
/// temporary directory until process exit. No Cargo invocation, dependency
/// discovery, network access or non-Rust interpreter is needed.
pub fn executable(kind: &str) -> PathBuf {
    static FIXTURES: OnceLock<Mutex<HashMap<String, (tempfile::TempDir, PathBuf)>>> =
        OnceLock::new();
    let mut fixtures = FIXTURES.get_or_init(Mutex::default).lock().unwrap();
    let (_, executable) = fixtures.entry(kind.to_owned()).or_insert_with(|| {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("protocol_server.rs");
        let executable = directory
            .path()
            .join(format!("protocol-fixture{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&source, include_str!("test_protocol_server.rs")).unwrap();
        let output =
            std::process::Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
                .arg("--edition=2024")
                .arg("--crate-name=xylo_protocol_fixture")
                .arg(&source)
                .arg("-o")
                .arg(&executable)
                .env("XYLO_PROTOCOL_FIXTURE_KIND", kind)
                .output()
                .expect("compile protocol fixture with the Rust toolchain used for tests");
        assert!(
            output.status.success(),
            "Rust protocol fixture failed to compile:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        (directory, executable)
    });
    executable.clone()
}

/// Put a fixture inside the test's sandbox read roots and keep sidecar files
/// (PID, mode, launch counts) local to that test. fs::copy retains execute bits.
#[allow(dead_code)]
pub fn install(kind: &str, destination: &Path) {
    std::fs::copy(executable(kind), destination).unwrap();
}

#[cfg(test)]
#[allow(dead_code)]
mod wire {
    include!("test_protocol_server.rs");
}
