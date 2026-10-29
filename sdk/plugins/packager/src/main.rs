//! Offline developer tool. Encodes a Rust guest module as a component and
//! creates a new reviewable package; never installs, grants or runs a plugin.

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::Path,
};
use xylo_core::{NativePluginManifest, PluginBudgets};

const MODULE_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Template {
    id: String,
    name: String,
    version: String,
    capabilities: Vec<String>,
    budgets: PluginBudgets,
}

fn read_bounded(path: &Path, maximum: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= maximum, "Input exceeds its byte limit");
    Ok(bytes)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn pack(module: &Path, template: &Path, output: &Path) -> Result<()> {
    let input = read_bounded(module, MODULE_LIMIT)?;
    let template: Template = serde_json::from_slice(&read_bounded(template, 32 * 1024)?)?;
    // No WASI adapter: imports not described by the guest's WIT cause encoding
    // to fail. The app independently checks its exact supported ABI on install.
    let bytes = wit_component::ComponentEncoder::default()
        .module(&input)
        .context("Read the guest module and its WIT metadata")?
        .validate(true)
        .encode()
        .context("Encode a WebAssembly component without WASI")?;
    ensure!(
        bytes.len() <= MODULE_LIMIT,
        "Encoded component exceeds 16 MiB"
    );
    let manifest = NativePluginManifest {
        format: 1,
        id: template.id,
        name: template.name,
        version: template.version,
        entrypoint: "component.wasm".into(),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        capabilities: template.capabilities,
        budgets: template.budgets,
    };
    let mut json = serde_json::to_vec_pretty(&manifest)?;
    json.push(b'\n');
    // An existing directory, file or link is refused. Failures leave the new
    // directory for inspection; they never remove or overwrite caller files.
    fs::create_dir(output).context("Choose a new, nonexistent output directory")?;
    write_new(&output.join("component.wasm"), &bytes)?;
    write_new(&output.join("plugin.json"), &json)?;
    #[cfg(unix)]
    fs::File::open(output)?.sync_all()?;
    Ok(())
}

fn main() -> Result<()> {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    ensure!(
        arguments.len() == 3,
        "Usage: xylo-plugin-pack <guest.wasm> <package-template.json> <new-output-directory>"
    );
    pack(
        Path::new(&arguments[0]),
        Path::new(&arguments[1]),
        Path::new(&arguments[2]),
    )?;
    println!("Package created. Review plugin.json and install it through Xylo Settings → Plugins.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_inputs_and_create_new_never_replace_existing_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("preserved");
        fs::write(&file, b"keep").unwrap();
        assert!(write_new(&file, b"replace").is_err());
        assert_eq!(fs::read(&file).unwrap(), b"keep");
        assert!(read_bounded(&file, 3).is_err());
        assert_eq!(read_bounded(&file, 4).unwrap(), b"keep");
    }

    #[test]
    fn invalid_module_creates_no_package_and_preserves_source() {
        let temp = tempfile::tempdir().unwrap();
        let module = temp.path().join("module.wasm");
        let template = temp.path().join("package.json");
        let output = temp.path().join("package");
        fs::write(&module, b"not WebAssembly").unwrap();
        fs::write(
            &template,
            include_bytes!("../../examples/text-report/package.json"),
        )
        .unwrap();
        assert!(pack(&module, &template, &output).is_err());
        assert!(!output.exists());
        assert_eq!(fs::read(&module).unwrap(), b"not WebAssembly");
    }
}
