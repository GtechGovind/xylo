//! Rust-only application build tooling, shared by Cargo's build script and CLI.
use anyhow::{Context, Result, bail};
use std::process::Command;

#[path = "build-browser.rs"]
pub mod browser;
#[path = "build-bundle.rs"]
pub mod bundle;

// Exercise the desktop build-script encoder without compiling the UI/browser.
#[cfg(test)]
#[path = "desktop/build_icons.rs"]
mod build_icons;

pub fn run(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("starting {command:?}"))?;
    if !status.success() {
        bail!("command failed ({status}): {command:?}");
    }
    Ok(())
}

pub fn output(command: &mut Command) -> Result<String> {
    let output = command
        .output()
        .with_context(|| format!("starting {command:?}"))?;
    if !output.status.success() {
        bail!(
            "command failed: {command:?}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
