//! Offline release validation; this binary is a build tool, not an app sidecar.
use anyhow::{Context, Result, ensure};
use std::{fs, path::PathBuf};
use tokio_util::sync::CancellationToken;
use workbench_distribution::{UpdatePolicy, Updater};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(
        args.len() == 5,
        "Usage: workbench-release-verify POLICY PLATFORM MANIFEST SIGNATURE ARTIFACT"
    );
    let data = tempfile::tempdir()?;
    let policy: UpdatePolicy = serde_json::from_slice(&fs::read(&args[0])?)?;
    let updater = Updater::new(
        policy,
        "0.0.0".parse()?,
        args[1].to_str().context("Invalid platform")?.to_owned(),
        data.path().join("updates"),
        std::env::current_exe()?,
    )?;
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let offer = updater
        .verify_manifest(&fs::read(&args[2])?, &fs::read_to_string(&args[3])?, at)?
        .context("Release version must be newer than 0.0.0")?;
    updater.verify_artifact(&offer, &PathBuf::from(&args[4]), &CancellationToken::new())?;
    println!(
        "Verified {} for {}",
        offer.summary().version,
        offer.summary().platform
    );
    Ok(())
}
