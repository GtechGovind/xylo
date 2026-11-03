//! Native release automation. Run from the repository root; secrets stay in
//! process environments and are never included in command diagnostics.
mod assets;
mod package;
mod publish;
mod signatures;
mod util;

use anyhow::{Result, bail, ensure};
use std::{env, fs, path::Path};

fn main() {
    if let Err(error) = run() {
        eprintln!("release-tool: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.as_slice() {
        [command] if command == "package" => package::run(),
        [command] if command == "verify-payload" => assets::verify_payload(),
        [command] if command == "publish" => publish::run(false),
        [command, flag] if command == "publish" && flag == "--allow-partial" => publish::run(true),
        [command, path] if command == "verify-cli-result" => {
            let result: serde_json::Value = serde_json::from_slice(&fs::read(Path::new(path))?)?;
            ensure!(
                result["exit_code"] == 0 && result["text"] == "Listed the workspace.",
                "CLI scripted-provider acceptance failed"
            );
            Ok(())
        }
        _ => bail!(
            "usage: cargo run --manifest-path crates/release-tool/Cargo.toml -- <package|publish [--allow-partial]|verify-payload|verify-cli-result PATH>"
        ),
    }
}

#[cfg(test)]
mod tests;
