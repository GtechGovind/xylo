use crate::util::*;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

/// None means CI is still running (or has not appeared yet).
pub fn ci_passed(runs: &Value) -> Result<Option<()>> {
    let runs = runs.as_array().context("Invalid CI run list")?;
    if let Some(run) = runs.first().filter(|r| r["status"] == "completed") {
        ensure!(
            run["conclusion"] == "success",
            "CI must pass before publishing: {}",
            text(run, "url")?
        );
        return Ok(Some(()));
    }
    Ok(None)
}

fn wait_for_ci(repo: &str, commit: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3600);
    while Instant::now() < deadline {
        let runs = output(
            Command::new("gh").args([
                "run",
                "list",
                "--repo",
                repo,
                "--workflow",
                "ci.yml",
                "--commit",
                commit,
                "--branch",
                "main",
                "--limit",
                "1",
                "--json",
                "status,conclusion,url",
            ]),
            "Matching main CI lookup",
        )?;
        if ci_passed(&serde_json::from_str(&runs)?)?.is_some() {
            return Ok(());
        }
        println!("Waiting for the matching main CI run to finish");
        thread::sleep(Duration::from_secs(30));
    }
    anyhow::bail!("CI has not completed; verified downloads remain in Actions artifacts")
}

#[derive(Debug)]
pub struct AuthenticatedDownloads {
    pub receipts: Vec<Value>,
    pub files: BTreeSet<PathBuf>,
}

// Separate from util::walk: app bundles can contain links, downloads cannot.
fn download_files(root: &Path) -> Result<BTreeSet<PathBuf>> {
    let metadata = fs::symlink_metadata(root)?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "Release downloads cannot contain symlinks"
    );
    ensure!(
        metadata.is_dir(),
        "Release download root must be a directory"
    );
    let mut files = BTreeSet::new();
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "Release downloads cannot contain symlinks"
        );
        if metadata.is_dir() {
            files.extend(download_files(&path)?);
        } else {
            ensure!(
                metadata.is_file(),
                "Release downloads must be regular files"
            );
            files.insert(path);
        }
    }
    Ok(files)
}

pub fn validate_receipts(
    root: &Path,
    version: &str,
    commit: &str,
    partial: bool,
) -> Result<AuthenticatedDownloads> {
    validate_authenticated(root, version, commit, partial, crate::signatures::verify)
}

fn validate_authenticated(
    root: &Path,
    version: &str,
    commit: &str,
    partial: bool,
    verify: impl Fn(&Path) -> Result<()>,
) -> Result<AuthenticatedDownloads> {
    let downloaded = download_files(root)?;
    let mut allowed = BTreeSet::new();
    let mut receipts = Vec::new();
    let mut seen = BTreeSet::new();
    let authenticate = |path: &Path, allowed: &mut BTreeSet<PathBuf>| -> Result<()> {
        ensure!(downloaded.contains(path), "Signed release file is missing");
        ensure!(
            downloaded.contains(&sidecar(path)),
            "Release file signature is missing"
        );
        verify(path)?;
        ensure!(allowed.insert(path.to_owned()), "Duplicate release file");
        ensure!(allowed.insert(sidecar(path)), "Duplicate release signature");
        Ok(())
    };
    for path in &downloaded {
        let filename = path.file_name().unwrap_or_default().to_string_lossy();
        if !path.is_file() || !filename.starts_with("Xylo-") || !filename.ends_with(".json") {
            continue;
        }
        // Authenticate before parsing or trusting any receipt fields.
        authenticate(path, &mut allowed)?;
        let receipt = read_json(path)?;
        ensure!(
            receipt["version"] == version && receipt["commit"] == commit,
            "Release downloads come from different versions or commits"
        );
        ensure!(
            receipt["artifact_signatures"] == "verified",
            "An artifact signature was not verified"
        );
        ensure!(
            seen.insert(text(&receipt, "platform")?.to_owned()),
            "Duplicate platform receipt"
        );
        let parent = path.parent().context("Receipt parent missing")?;
        let mut checksums = String::new();
        let artifacts = receipt["files"]
            .as_array()
            .context("Missing receipt files")?;
        ensure!(!artifacts.is_empty(), "Receipt must list release artifacts");
        for artifact in receipt["files"]
            .as_array()
            .context("Missing receipt files")?
        {
            let filename = text(artifact, "name")?;
            // Check both separators on every host; Windows traversal is invalid on Linux too.
            ensure!(
                !filename.is_empty()
                    && !filename.contains(['/', '\\', ':'])
                    && filename != "."
                    && filename != ".."
                    && !filename.chars().any(char::is_control)
                    && !filename.ends_with(['.', ' ']),
                "Artifact must be a filename"
            );
            let artifact_path = parent.join(filename);
            ensure!(
                artifact_path.metadata()?.len()
                    == artifact["size"].as_u64().context("Invalid artifact size")?
                    && sha256(&artifact_path)? == text(artifact, "sha256")?,
                "Release download does not match its verified receipt"
            );
            authenticate(&artifact_path, &mut allowed)?;
            checksums.push_str(&format!("{}  {filename}\n", text(artifact, "sha256")?));
        }
        let checksum = path.with_extension("sha256");
        authenticate(&checksum, &mut allowed)?;
        ensure!(
            fs::read_to_string(&checksum)? == checksums,
            "Checksum file does not match signed receipt"
        );
        let public = parent.join(crate::signatures::PUBLIC_FILENAME);
        ensure!(
            downloaded.contains(&public)
                && fs::read(&public)? == crate::signatures::PINNED_PUBLIC.as_bytes(),
            "Downloaded public key does not match repository trust anchor"
        );
        allowed.insert(public);
        receipts.push(receipt);
    }
    let required: BTreeSet<String> = if partial {
        ["macos"].as_slice()
    } else {
        ["macos", "windows", "linux"].as_slice()
    }
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    ensure!(
        seen == required,
        "All three platform builds must be present before publishing (or explicitly request macOS-only publication)"
    );
    receipts.sort_by_key(|r| r["platform"].as_str().unwrap_or_default().to_owned());
    ensure!(downloaded == allowed, "Unexpected release download files");
    Ok(AuthenticatedDownloads {
        receipts,
        files: allowed,
    })
}

// Structural tests explicitly replace only the cryptographic boundary. This API
// does not exist in production builds; the publisher always uses signatures::verify.
#[cfg(test)]
pub fn validate_with_verifier(
    root: &Path,
    version: &str,
    commit: &str,
    partial: bool,
    verify: impl Fn(&Path) -> Result<()>,
) -> Result<AuthenticatedDownloads> {
    validate_authenticated(root, version, commit, partial, verify)
}

pub fn validate_existing(current: &Value, partial: bool) -> Result<BTreeSet<String>> {
    ensure!(
        current["isDraft"] == true || current["isPrerelease"] == true,
        "Refusing to change a stable release"
    );
    let names: BTreeSet<String> = current["assets"]
        .as_array()
        .context("Invalid existing release assets")?
        .iter()
        .map(|asset| text(asset, "name").map(str::to_owned))
        .collect::<Result<_>>()?;
    ensure!(
        !partial
            || !names
                .iter()
                .any(|s| s.contains("-windows-") || s.contains("-linux-")),
        "Cannot replace complete release notes with partial-build notes"
    );
    Ok(names)
}

fn notes(commit: &str, receipts: &[Value], partial: bool) -> Result<String> {
    let availability = if partial {
        "The macOS build is available now. Windows and Linux builds are still running and will be added after verification."
    } else {
        "macOS, Windows, and Linux downloads have completed native builds and signature verification."
    };
    let mut platform_status = Vec::new();
    for receipt in receipts {
        platform_status.push(format!(
            "- {} ({}): project signature verified; platform signing: {}.",
            text(receipt, "platform")?,
            text(receipt, "arch")?,
            text(receipt, "platform_signing")?.replace('_', " ")
        ));
    }
    let platform_status = platform_status.join("\n");
    Ok(format!(
        r#"Release candidate for testing, built from `{commit}`.

{availability}

This release includes Rust build and release tooling, the bundled Obscura browser, cleaner light and dark surfaces with consistent application icons, safer draft and document recovery, retryable approvals, and reliable search and quit shortcuts.

Every download has a SHA-256 checksum and a project-key signature (`.sig`). The verification public key is included as `Xylo-release-signing.pub`. These are cargo-packager/Minisign signatures; **they do not provide Apple notarization or Windows publisher trust**. This preview may display operating-system security warnings.

macOS app bundles are also sealed and checked with `codesign --verify --deep --strict`, including the copies inside the DMG and app archive. Without Developer ID credentials this is an ad-hoc signature, not Apple publisher verification. If macOS blocks this preview because the developer cannot be verified, follow [Apple's Open Anyway instructions](https://support.apple.com/102445) in System Settings → Privacy & Security. No global Gatekeeper changes are required.

{platform_status}

Available architectures: macOS Apple Silicon (`arm64`), Windows `x64`, and Linux `x64`. Linux packages target Ubuntu 24.04 and require glibc 2.39 or later; the AppImage does not bundle glibc.

The macOS `.app.tar.gz` preserves the app bundle's executable permissions. The `.dmg`, Windows installer, and Linux `.deb`/`.AppImage` are native packages. Executable tools require Node.js 22.12 or later. Linux also requires `bubblewrap`, `socat`, and `ripgrep`; the `.deb` declares these dependencies, while AppImage users install them separately. Linux host policy must allow Bubblewrap to create user and network namespaces; some Ubuntu installations require AppArmor configuration.

Artifact signatures use cargo-packager's Base64 encoding of Minisign signatures. Verification instructions and the public key are in the repository's distribution documentation.
"#
    ))
}

pub fn run(partial: bool) -> Result<()> {
    let tag = env::var("GITHUB_REF_NAME")?;
    ensure!(
        tag.starts_with('v') && tag.contains("-rc."),
        "This publisher is for release-candidate tags only"
    );
    let version = &tag[1..];
    let repo = env::var("GITHUB_REPOSITORY")?;
    let commit = env::var("GITHUB_SHA")?;
    if !partial {
        wait_for_ci(&repo, &commit)?;
    }
    let root = Path::new("dist/downloads");
    let authenticated = validate_receipts(root, version, &commit, partial)?;
    let destination = Path::new("dist/publish");
    reset_directory(destination)?;
    let mut upload_files = BTreeSet::new();
    for path in &authenticated.files {
        let target = destination.join(path.file_name().context("Missing release asset filename")?);
        ensure!(
            !target.exists() || sha256(path)? == sha256(&target)?,
            "Conflicting release asset names"
        );
        fs::copy(path, &target)?;
        upload_files.insert(target);
    }
    let existing = Command::new("gh")
        .args([
            "release",
            "view",
            &tag,
            "--repo",
            &repo,
            "--json",
            "isDraft,isPrerelease,assets",
        ])
        .output()?;
    let temporary = tempfile::tempdir()?;
    let notes_file = temporary.path().join("notes.md");
    // Close before gh opens it, including on Windows.
    fs::write(
        &notes_file,
        notes(&commit, &authenticated.receipts, partial)?,
    )?;
    let existing_names = if existing.status.success() {
        let names = validate_existing(&serde_json::from_slice(&existing.stdout)?, partial)?;
        checked(
            Command::new("gh")
                .args(["release", "edit", &tag, "--repo", &repo, "--notes-file"])
                .arg(&notes_file),
            "Release notes update",
        )?;
        names
    } else {
        checked(
            Command::new("gh")
                .args([
                    "release",
                    "create",
                    &tag,
                    "--repo",
                    &repo,
                    "--verify-tag",
                    "--draft",
                    "--prerelease",
                    "--title",
                ])
                .arg(format!("Xylo {version}"))
                .arg("--notes-file")
                .arg(&notes_file),
            "Draft RC creation",
        )?;
        BTreeSet::new()
    };
    // No --clobber: downloads already published remain immutable.
    let pending: Vec<_> = upload_files
        .into_iter()
        .filter(|p| {
            !existing_names.contains(p.file_name().unwrap_or_default().to_string_lossy().as_ref())
        })
        .collect();
    if !pending.is_empty() {
        checked(
            Command::new("gh")
                .args(["release", "upload", &tag, "--repo", &repo])
                .args(pending),
            "Release asset upload",
        )?;
    }
    checked(
        Command::new("gh").args([
            "release",
            "edit",
            &tag,
            "--repo",
            &repo,
            "--draft=false",
            "--prerelease",
            "--latest=false",
        ]),
        "RC publication",
    )
}
