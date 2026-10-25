//! Transactional macOS replacement on the destination volume. Unlike the
//! upstream privilege fallback, failures never delete the installed bundle.
use super::*;
use std::{
    io::Cursor,
    process::{Command, Stdio},
    thread,
};

pub(super) fn install(
    updater: &Updater,
    bytes: &[u8],
    destination: &Path,
    version: &Version,
    cancel: &CancellationToken,
) -> Result<()> {
    replace(bytes, destination, |bundle| {
        let team = updater
            .policy
            .macos_team_id
            .as_deref()
            .context("macOS publisher identity is not configured")?;
        run(
            "/usr/bin/codesign",
            &[
                "--verify",
                "--deep",
                "--strict",
                bundle.to_str().context("Invalid app path")?,
            ],
            cancel,
        )?;
        let details = run(
            "/usr/bin/codesign",
            &[
                "-d",
                "--verbose=4",
                bundle.to_str().context("Invalid app path")?,
            ],
            cancel,
        )?;
        ensure!(
            details
                .lines()
                .any(|line| line == format!("TeamIdentifier={team}")),
            "Update publisher identity does not match"
        );
        for (field, expected) in [
            ("CFBundleIdentifier", updater.policy.application_id.as_str()),
            ("CFBundleShortVersionString", &version.to_string()),
        ] {
            let plist = bundle.join("Contents/Info.plist");
            let actual = run(
                "/usr/bin/plutil",
                &[
                    "-extract",
                    field,
                    "raw",
                    "-o",
                    "-",
                    plist.to_str().context("Invalid app path")?,
                ],
                cancel,
            )?;
            ensure!(
                actual.trim() == expected,
                "Update application identity or version does not match"
            );
        }
        run(
            "/usr/sbin/spctl",
            &[
                "--assess",
                "--type",
                "execute",
                bundle.to_str().context("Invalid app path")?,
            ],
            cancel,
        )?;
        Ok(())
    })
}
/// Kept separate so tests can prove atomic replacement and failure rollback on
/// private fixture bundles without pretending they have an Apple signature.
pub(super) fn replace(
    bytes: &[u8],
    destination: &Path,
    verify: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let parent = destination.parent().context("Invalid install location")?;
    let staging = tempfile::Builder::new()
        .prefix(".workbench-update-")
        .tempdir_in(parent)
        .context(
            "The application folder is not writable; install the downloaded package manually",
        )?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(Cursor::new(bytes)));
    archive.unpack(staging.path())?;
    let candidates: Vec<_> = fs::read_dir(staging.path())?.collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        candidates.len() == 1
            && candidates[0]
                .path()
                .extension()
                .is_some_and(|ext| ext == "app"),
        "Expected one application bundle"
    );
    let candidate = candidates[0].path();
    verify(&candidate)?;
    for entry in walkdir::WalkDir::new(&candidate).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() {
            File::open(entry.path())?.sync_all()?;
        }
    }
    // Kernel-level exchange has no moment where the installed path is absent.
    // If the process crashes, that path still names either the old or new app.
    // Filesystems without exchange support fail safely; never fall back to two
    // renames or deleting the current app for an administrator retry.
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        &candidate,
        rustix::fs::CWD,
        destination,
        rustix::fs::RenameFlags::EXCHANGE,
    )
    .context("Could not replace the application atomically; install manually")?;
    File::open(staging.path())?.sync_all()?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
fn run(program: &str, args: &[&str], cancel: &CancellationToken) -> Result<String> {
    let output = NamedTempFile::new()?;
    let stream = output.reopen()?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(stream.try_clone()?)
        .stderr(stream)
        .spawn()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(
                status.success(),
                "The operating system could not verify the update"
            );
            break;
        }
        if cancel.is_cancelled() || std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Update verification cancelled or timed out");
        }
        thread::sleep(Duration::from_millis(50));
    }
    let mut bytes = Vec::new();
    output.reopen()?.take(16 * 1024).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
