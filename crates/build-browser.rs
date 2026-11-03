//! Native Obscura source build. V8's archive is downloaded by Rust, never by
//! upstream's interpreter-based downloader. V8 itself is a prebuilt dependency.
use crate::{output, run};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

pub const VERSION: &str = "0.2.6";
pub const REVISION: &str = "8534b401898ed17b62f69bdc3d5eb186bffb0d60";
pub const REPOSITORY: &str = "https://github.com/h4ckf0r0day/obscura.git";
const SOURCE_PATCH: &str = include_str!("patches/obscura-native-roots.patch");
const V8_VERSION: &str = "150.4.0";
pub const TARGETS: &[&str] = &[
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    Debug,
    Release,
}
impl Profile {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "debug" => Ok(Self::Debug),
            "release" => Ok(Self::Release),
            _ => bail!("profile must be debug or release"),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
        }
    }
}

#[derive(Debug)]
pub struct Plan {
    pub command: Vec<String>,
    pub artifact: PathBuf,
    pub destination: PathBuf,
    pub licenses: PathBuf,
}

pub fn plan(
    root: &Path,
    destination_dir: &Path,
    profile: Profile,
    target: &str,
    host: &str,
) -> Result<Plan> {
    ensure!(
        TARGETS.contains(&target),
        "unsupported Obscura target: {target}; Windows ARM64 is not qualified"
    );
    ensure!(
        target == host,
        "Obscura requires a native target host (V8 snapshots cannot be cross-built)"
    );
    let suffix = if target.ends_with("windows-msvc") {
        ".exe"
    } else {
        ""
    };
    let cache = root
        .join("target/obscura-build")
        .join(REVISION)
        .join(target);
    let mut command: Vec<String> = ["build", "--locked", "--target", target, "--target-dir"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    command.extend([
        cache.to_string_lossy().into_owned(),
        "-p".into(),
        "obscura-cli".into(),
        "--bin".into(),
        "obscura".into(),
        "--no-default-features".into(),
        "--features".into(),
        "render".into(),
    ]);
    if profile == Profile::Release {
        command.push("--release".into());
    }
    Ok(Plan {
        command,
        artifact: cache
            .join(target)
            .join(profile.name())
            .join(format!("obscura{suffix}")),
        destination: destination_dir.join(format!("xylo-obscura{suffix}")),
        licenses: destination_dir.join("obscura-licenses"),
    })
}

pub fn native_machine(host: &str, machine: &str) -> Result<()> {
    let machine = machine.to_ascii_lowercase();
    let machine = match machine.as_str() {
        "amd64" => "x86_64",
        "arm64" => "aarch64",
        other => other,
    };
    ensure!(
        host.split('-').next() == Some(machine),
        "Obscura requires a native host toolchain, not architecture emulation"
    );
    Ok(())
}

pub fn host(root: &Path) -> Result<String> {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    output(Command::new(rustc).arg("-vV").current_dir(root))?
        .lines()
        .find_map(|line| line.strip_prefix("host: ").map(str::to_owned))
        .context("rustc did not report its host target")
}

fn git(source: &Path, args: &[&str]) -> Result<String> {
    output(Command::new("git").args(args).current_dir(source))
}

fn checkout_values(origin: &str, revision: &str, status: &str) -> Result<()> {
    ensure!(
        origin == REPOSITORY,
        "refusing foreign Obscura checkout: unexpected origin"
    );
    ensure!(
        revision == REVISION,
        "Obscura checkout does not match the pinned revision"
    );
    ensure!(status.is_empty(), "Obscura checkout is not clean");
    Ok(())
}

fn verify_checkout(source: &Path) -> Result<()> {
    checkout_values(
        &git(source, &["remote", "get-url", "origin"])?,
        &git(source, &["rev-parse", "HEAD"])?,
        &git(source, &["status", "--porcelain", "--untracked-files=all"])?,
    )
}

fn patch_digest() -> String {
    hex::encode(Sha256::digest(SOURCE_PATCH.as_bytes()))
}

fn verify_source_patch(source: &Path, patch: &str) -> Result<()> {
    ensure!(
        git(
            source,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--abbrev=7",
                "--unified=3",
                "--no-relative",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                "--diff-algorithm=myers",
                "--indent-heuristic",
                "--binary",
                "HEAD",
                "--",
            ]
        )? == patch.trim(),
        "Obscura source differs from the reviewed patch"
    );
    ensure!(
        git(source, &["ls-files", "--others", "--exclude-standard"])?.is_empty(),
        "Obscura checkout contains untracked source files"
    );
    Ok(())
}

fn apply_source_patch(source: &Path) -> Result<()> {
    verify_checkout(source)?;
    let mut patch = tempfile::NamedTempFile::new()?;
    patch.write_all(SOURCE_PATCH.as_bytes())?;
    patch.flush()?;
    run(Command::new("git")
        .arg("apply")
        .arg("--check")
        .arg(patch.path())
        .current_dir(source))?;
    run(Command::new("git")
        .arg("apply")
        .arg(patch.path())
        .current_dir(source))?;
    verify_patched_checkout(source)
}

fn verify_patched_checkout(source: &Path) -> Result<()> {
    checkout_values(
        &git(source, &["remote", "get-url", "origin"])?,
        &git(source, &["rev-parse", "HEAD"])?,
        "",
    )?;
    verify_source_patch(source, SOURCE_PATCH)
}

fn checkout(root: &Path) -> Result<PathBuf> {
    let parent = root.join("target/obscura-source");
    fs::create_dir_all(&parent)?;
    // Keep each independent checkout, including failures, for inspection. Never
    // reset/clean a checkout or reuse one supplied by an application setting.
    let source = tempfile::Builder::new()
        .prefix(&format!("{REVISION}-"))
        .tempdir_in(parent)?
        .keep();
    git(&source, &["init", "."])?;
    git(&source, &["remote", "add", "origin", REPOSITORY])?;
    git(&source, &["fetch", "--depth=1", "origin", REVISION])?;
    ensure!(
        git(&source, &["rev-parse", "FETCH_HEAD"])? == REVISION,
        "fetched object does not match immutable pin"
    );
    git(
        &source,
        &[
            "-c",
            "advice.detachedHead=false",
            "checkout",
            "--detach",
            REVISION,
        ],
    )?;
    verify_checkout(&source)?;
    Ok(source)
}

fn license_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_dir() {
            if !matches!(name.as_str(), ".git" | "target" | "node_modules") {
                license_files(&entry.path(), files)?;
            }
        } else if entry.file_type()?.is_file() {
            let upper = name.to_ascii_uppercase();
            if [
                "LICENSE",
                "LICENCE",
                "COPYING",
                "NOTICE",
                "AUTHORS",
                "COPYRIGHT",
            ]
            .iter()
            .any(|prefix| upper.starts_with(prefix))
            {
                files.push(entry.path());
            }
        }
    }
    Ok(())
}

fn copy_notices(source: &Path, destination: &Path) -> Result<()> {
    let mut files = Vec::new();
    license_files(source, &mut files)?;
    for path in files {
        let out = destination.join(path.strip_prefix(source)?);
        fs::create_dir_all(out.parent().context("notice parent")?)?;
        fs::copy(path, out)?;
    }
    Ok(())
}

fn licenses(cargo: &Path, source: &Path, destination: &Path, target: &str) -> Result<()> {
    let metadata: Value = serde_json::from_str(&output(
        Command::new(cargo)
            .args([
                "metadata",
                "--locked",
                "--format-version",
                "1",
                "--filter-platform",
                target,
                "--no-default-features",
                "--features",
                "obscura-cli/render",
            ])
            .current_dir(source),
    )?)?;
    fs::create_dir_all(destination)?;
    copy_notices(source, &destination.join("upstream"))?;
    let mut packages = Vec::new();
    for package in metadata["packages"].as_array().context("Cargo packages")? {
        packages.push(json!({"name": package["name"], "version": package["version"], "license": package["license"], "source": package["source"]}));
        if !package["source"].is_null() {
            let manifest = Path::new(package["manifest_path"].as_str().context("manifest path")?);
            copy_notices(
                manifest.parent().context("package root")?,
                &destination.join("dependencies").join(format!(
                    "{}-{}",
                    package["name"].as_str().context("name")?,
                    package["version"].as_str().context("version")?
                )),
            )?;
        }
    }
    fs::write(
        destination.join("BUILD.json"),
        serde_json::to_vec_pretty(
            &json!({"project":"Obscura", "version": VERSION, "revision": REVISION, "repository": REPOSITORY, "patch_sha256":patch_digest(), "features":["render"], "target":target, "packages":packages}),
        )?,
    )?;
    ensure!(
        destination.join("upstream/LICENSE").is_file(),
        "Obscura license missing; refusing to stage runtime"
    );
    Ok(())
}

fn v8_asset(target: &str) -> String {
    if target.ends_with("windows-msvc") {
        format!("rusty_v8_simdutf_release_{target}.lib.gz")
    } else {
        format!("librusty_v8_simdutf_release_{target}.a.gz")
    }
}

fn local_v8(root: &Path, target: &str) -> Result<PathBuf> {
    let parent = root.join("target/v8-archives").join(V8_VERSION);
    fs::create_dir_all(&parent)?;
    let destination = parent.join(v8_asset(target));
    if !destination.is_file() {
        let url = format!(
            "https://github.com/denoland/rusty_v8/releases/download/v{V8_VERSION}/{}",
            v8_asset(target)
        );
        eprintln!("BUILD-TIME NETWORK: downloading {url} with Rust");
        let mut response = reqwest::blocking::Client::builder()
            .https_only(true)
            .timeout(Duration::from_secs(1800))
            .build()?
            .get(url)
            .send()?
            .error_for_status()?;
        let mut stage = tempfile::NamedTempFile::new_in(&parent)?;
        std::io::copy(&mut response, &mut stage)?;
        stage.flush()?;
        let mut magic = [0; 2];
        fs::File::open(stage.path())?.read_exact(&mut magic)?;
        ensure!(
            magic == [0x1f, 0x8b],
            "V8 archive was not gzip; refusing to cache"
        );
        stage.persist(&destination)?;
    }
    Ok(destination.canonicalize()?)
}

/// Do not allow upstream to enter its Python-based source-build or downloader
/// branches. Crate-distributed Rust bindings and a local gzip archive suffice.
fn configure_v8(command: &mut Command, archive: &Path) {
    command.env("RUSTY_V8_ARCHIVE", archive);
    for variable in [
        "V8_FROM_SOURCE",
        "V8_FORCE_DEBUG",
        "RUSTY_V8_MIRROR",
        "RUSTY_V8_SRC_BINDING_PATH",
    ] {
        command.env_remove(variable);
    }
}

fn digest(path: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    let mut file = fs::File::open(path)?;
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hex::encode(hash.finalize()))
}

fn current(plan: &Plan, profile: Profile, target: &str) -> bool {
    let stamp = plan.licenses.join("XYLO-BUILD.json");
    let Ok(bytes) = fs::read(stamp) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return false;
    };
    value["revision"] == REVISION
        && value["builder"] == 1
        && value["patch_sha256"] == patch_digest()
        && value["profile"] == profile.name()
        && value["target"] == target
        && plan.licenses.join("upstream/LICENSE").is_file()
        && digest(&plan.destination).is_ok_and(|hash| value["sha256"] == hash)
}

pub fn build(
    root: &Path,
    destination: &Path,
    profile: Profile,
    target: &str,
    host: &str,
) -> Result<()> {
    let plan = plan(root, destination, profile, target, host)?;
    let machine =
        std::env::var("PROCESSOR_ARCHITEW6432").unwrap_or_else(|_| std::env::consts::ARCH.into());
    native_machine(host, &machine)?;
    fs::create_dir_all(root.join("target"))?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("target/obscura-build.lock"))?;
    lock.lock_exclusive()?;
    if current(&plan, profile, target) {
        return Ok(());
    }
    eprintln!(
        "BUILD-TIME NETWORK: fetching pinned Obscura v{VERSION} ({REVISION}) and locked dependencies"
    );
    let source = checkout(root)?;
    apply_source_patch(&source)?;
    let archive = local_v8(root, target)?;
    let cargo = PathBuf::from(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    let mut command = Command::new(&cargo);
    command.args(&plan.command).current_dir(&source);
    configure_v8(&mut command, &archive);
    // A nested Cargo invocation must not inherit Xylo build-script compiler
    // flags or target-directory settings; its target directory is explicit.
    command
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS");
    run(&mut command)?;
    verify_patched_checkout(&source)?;
    fs::create_dir_all(destination)?;
    let stage = tempfile::Builder::new()
        .prefix("xylo-obscura-stage-")
        .tempdir_in(destination)?;
    licenses(&cargo, &source, &stage.path().join("licenses"), target)?;
    run(Command::new(&plan.artifact)
        .args(["fetch", "data:text/html,<title>ok</title>", "--eval", "1+1"])
        .current_dir(&source))?;
    let screenshot = stage.path().join("smoke.png");
    run(Command::new(&plan.artifact)
        .args([
            "fetch",
            "data:text/html,<body>Obscura</body>",
            "--screenshot",
        ])
        .arg(&screenshot)
        .current_dir(&source))?;
    ensure!(
        fs::read(screenshot)?.starts_with(b"\x89PNG\r\n\x1a\n"),
        "render smoke test did not produce a PNG"
    );
    let binary = stage
        .path()
        .join(plan.destination.file_name().context("binary name")?);
    fs::copy(&plan.artifact, &binary)?;
    fs::write(
        stage.path().join("licenses/XYLO-BUILD.json"),
        serde_json::to_vec_pretty(
            &json!({"revision": REVISION, "builder":1, "patch_sha256":patch_digest(), "profile":profile.name(), "target":target, "sha256":digest(&binary)?}),
        )?,
    )?;
    // Only replace application-owned staged outputs, never source checkouts.
    if plan.licenses.exists() {
        fs::remove_dir_all(&plan.licenses)?;
    }
    fs::rename(stage.path().join("licenses"), &plan.licenses)?;
    if plan.destination.exists() {
        fs::remove_file(&plan.destination)?;
    }
    fs::rename(binary, &plan.destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_platforms_have_locked_render_builds() {
        for target in TARGETS {
            for profile in [Profile::Debug, Profile::Release] {
                let p = plan(
                    Path::new("project"),
                    Path::new("output"),
                    profile,
                    target,
                    target,
                )
                .unwrap();
                assert!(p.command.contains(&"--locked".into()));
                assert!(
                    p.command
                        .windows(2)
                        .any(|args| args == ["--features", "render"])
                );
                assert!(p.command.contains(&"--no-default-features".into()));
                assert_eq!(
                    p.command.contains(&"--release".into()),
                    profile == Profile::Release
                );
                assert!(p.artifact.to_string_lossy().contains(REVISION));
                assert_eq!(p.artifact.extension().is_some(), target.contains("windows"));
                assert!(p.destination.starts_with("output"));
            }
        }
    }
    #[test]
    fn rejects_cross_build_and_unqualified_platforms() {
        assert!(
            plan(
                Path::new("."),
                Path::new("."),
                Profile::Debug,
                TARGETS[0],
                TARGETS[1]
            )
            .unwrap_err()
            .to_string()
            .contains("native")
        );
        assert!(
            plan(
                Path::new("."),
                Path::new("."),
                Profile::Debug,
                "aarch64-pc-windows-msvc",
                "aarch64-pc-windows-msvc"
            )
            .unwrap_err()
            .to_string()
            .contains("Windows ARM64")
        );
        assert!(Profile::parse("dist").is_err());
        assert!(native_machine(TARGETS[4], "ARM64").is_err());
        native_machine(TARGETS[4], "AMD64").unwrap();
        native_machine(TARGETS[0], "arm64").unwrap();
    }
    #[test]
    fn checkout_guards_foreign_dirty_and_wrong_revision() {
        assert!(checkout_values("https://example.invalid/foreign.git", REVISION, "").is_err());
        assert!(
            checkout_values(REPOSITORY, "0000000000000000000000000000000000000000", "").is_err()
        );
        assert!(checkout_values(REPOSITORY, REVISION, " M Cargo.toml").is_err());
        checkout_values(REPOSITORY, REVISION, "").unwrap();
    }
    #[test]
    fn patched_checkout_rejects_extra_changes_and_untracked_sources() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "."]).unwrap();
        fs::write(dir.path().join("Cargo.toml"), "original\n").unwrap();
        git(dir.path(), &["add", "Cargo.toml"]).unwrap();
        git(
            dir.path(),
            &[
                "-c",
                "user.name=Xylo Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "fixture",
            ],
        )
        .unwrap();
        fs::write(dir.path().join("Cargo.toml"), "patched\n").unwrap();
        let patch = git(dir.path(), &["diff", "--binary", "HEAD", "--"]).unwrap();
        git(dir.path(), &["config", "core.abbrev", "12"]).unwrap();
        git(dir.path(), &["config", "diff.noprefix", "true"]).unwrap();
        git(dir.path(), &["config", "diff.context", "8"]).unwrap();
        verify_source_patch(dir.path(), &patch).unwrap();
        fs::write(dir.path().join("unexpected.rs"), "extra source").unwrap();
        assert!(
            verify_source_patch(dir.path(), &patch)
                .unwrap_err()
                .to_string()
                .contains("untracked")
        );
        fs::remove_file(dir.path().join("unexpected.rs")).unwrap();
        fs::write(dir.path().join("Cargo.toml"), "unexpected modification\n").unwrap();
        assert!(
            verify_source_patch(dir.path(), &patch)
                .unwrap_err()
                .to_string()
                .contains("reviewed patch")
        );
    }
    #[test]
    fn collects_vendor_notices_not_build_caches() {
        let dir = tempfile::tempdir().unwrap();
        for path in [
            "LICENSE",
            "vendor/render/NOTICE",
            "target/LICENSE",
            ".git/NOTICE",
        ] {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "notice").unwrap();
        }
        let mut files = Vec::new();
        license_files(dir.path(), &mut files).unwrap();
        let mut relative: Vec<_> = files
            .iter()
            .map(|p| p.strip_prefix(dir.path()).unwrap().to_owned())
            .collect();
        relative.sort();
        assert_eq!(
            relative,
            [
                PathBuf::from("LICENSE"),
                PathBuf::from("vendor/render/NOTICE")
            ]
        );
    }
    #[test]
    fn upstream_receives_local_archive_and_no_interpreter_build_overrides() {
        let mut command = Command::new("cargo");
        configure_v8(&mut command, Path::new("local/archive.gz"));
        let env: std::collections::HashMap<_, _> = command.get_envs().collect();
        assert_eq!(
            env[std::ffi::OsStr::new("RUSTY_V8_ARCHIVE")],
            Some(std::ffi::OsStr::new("local/archive.gz"))
        );
        for key in [
            "V8_FROM_SOURCE",
            "V8_FORCE_DEBUG",
            "RUSTY_V8_MIRROR",
            "RUSTY_V8_SRC_BINDING_PATH",
        ] {
            assert_eq!(env[std::ffi::OsStr::new(key)], None);
        }
        assert_eq!(
            v8_asset(TARGETS[4]),
            "rusty_v8_simdutf_release_x86_64-pc-windows-msvc.lib.gz"
        );
        assert_eq!(
            v8_asset(TARGETS[0]),
            "librusty_v8_simdutf_release_aarch64-apple-darwin.a.gz"
        );
    }
    #[test]
    fn cache_rejects_unpatched_and_wrong_patch_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let p = plan(
            dir.path(),
            dir.path(),
            Profile::Debug,
            TARGETS[0],
            TARGETS[0],
        )
        .unwrap();
        fs::create_dir_all(p.licenses.join("upstream")).unwrap();
        fs::write(p.licenses.join("upstream/LICENSE"), "license").unwrap();
        fs::write(&p.destination, "binary").unwrap();
        let mut stamp = json!({"revision":REVISION,"builder":1,"profile":"debug","target":TARGETS[0],"sha256":digest(&p.destination).unwrap()});
        fs::write(
            p.licenses.join("XYLO-BUILD.json"),
            serde_json::to_vec(&stamp).unwrap(),
        )
        .unwrap();
        assert!(
            !current(&p, Profile::Debug, TARGETS[0]),
            "unpatched runtime must be rebuilt"
        );
        stamp["patch_sha256"] = json!("incorrect");
        fs::write(
            p.licenses.join("XYLO-BUILD.json"),
            serde_json::to_vec(&stamp).unwrap(),
        )
        .unwrap();
        assert!(
            !current(&p, Profile::Debug, TARGETS[0]),
            "changed patch must rebuild runtime"
        );
    }
    #[test]
    fn cache_requires_matching_stamp_and_binary_hash() {
        let dir = tempfile::tempdir().unwrap();
        let p = plan(
            dir.path(),
            dir.path(),
            Profile::Debug,
            TARGETS[4],
            TARGETS[4],
        )
        .unwrap();
        fs::create_dir_all(p.licenses.join("upstream")).unwrap();
        fs::write(p.licenses.join("upstream/LICENSE"), "license").unwrap();
        fs::write(&p.destination, "binary").unwrap();
        assert!(!current(&p, Profile::Debug, TARGETS[4]));
        fs::write(p.licenses.join("XYLO-BUILD.json"), serde_json::to_vec(&json!({"revision":REVISION,"builder":1,"patch_sha256":patch_digest(),"profile":"debug","target":TARGETS[4],"sha256":digest(&p.destination).unwrap()})).unwrap()).unwrap();
        assert!(current(&p, Profile::Debug, TARGETS[4]));
        assert!(!current(&p, Profile::Release, TARGETS[4]));
        fs::write(&p.destination, "changed").unwrap();
        assert!(!current(&p, Profile::Debug, TARGETS[4]));
    }
}
