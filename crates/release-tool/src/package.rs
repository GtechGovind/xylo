use crate::{assets, util::*};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

pub fn configure(source: &Value, environment: &Environment, system: &str) -> (Value, bool, bool) {
    let mut config = source.clone();
    let apple = system == "macos"
        && [
            "APPLE_CERTIFICATE",
            "APPLE_SIGNING_IDENTITY",
            "APPLE_ID",
            "APPLE_PASSWORD",
            "APPLE_TEAM_ID",
        ]
        .iter()
        .all(|name| environment.get(*name).is_some_and(|s| !s.is_empty()));
    if system == "macos" {
        config
            .as_object_mut()
            .expect("manifest object")
            .remove("licenseFile");
        if !config["macos"].is_object() {
            config["macos"] = json!({});
        }
        config["macos"]["signingIdentity"] = json!(if apple {
            environment["APPLE_SIGNING_IDENTITY"].as_str()
        } else {
            "-"
        });
    } else if let Some(macos) = config.get_mut("macos").and_then(Value::as_object_mut) {
        macos.remove("signingIdentity");
    }
    let minisign = environment
        .get("MINISIGN_KEY")
        .is_some_and(|s| !s.is_empty());
    (config, apple, minisign)
}

pub fn dependency_list(config: &Value, metadata: &str) -> Result<Vec<String>> {
    let line = metadata
        .lines()
        .find_map(|line| line.strip_prefix("shlibs:Depends="))
        .context("Could not determine Linux runtime dependencies")?;
    let mut dependencies: Vec<String> = line.split(", ").map(str::to_owned).collect();
    ensure!(
        !dependencies.is_empty() && dependencies.iter().all(|s| !s.is_empty()),
        "Could not determine Linux runtime dependencies"
    );
    dependencies.extend(
        [
            "libvulkan1",
            "libegl1",
            "libwayland-client0",
            "bubblewrap",
            "socat",
            "ripgrep",
        ]
        .map(str::to_owned),
    );
    if let Some(existing) = config["deb"]["depends"].as_array() {
        for item in existing {
            dependencies.push(
                item.as_str()
                    .context("Invalid Debian dependency")?
                    .to_owned(),
            );
        }
    }
    dependencies.sort();
    dependencies.dedup();
    Ok(dependencies)
}

fn linux_dependencies(config: &mut Value) -> Result<()> {
    let temporary = tempfile::Builder::new()
        .prefix("xylo-dependencies-")
        .tempdir()?;
    fs::create_dir(temporary.path().join("debian"))?;
    fs::write(
        temporary.path().join("debian/control"),
        "Source: xylo\n\nPackage: xylo\nArchitecture: any\nDescription: Xylo desktop workspace\n",
    )?;
    let mut command = Command::new("dpkg-shlibdeps");
    command.arg("-O").current_dir(temporary.path());
    for binary in config["binaries"].as_array().context("Missing binaries")? {
        let path = Path::new(text(config, "binariesDir")?)
            .join(text(binary, "path")?)
            .canonicalize()?;
        command.arg(format!("-e{}", path.display()));
    }
    let dependencies = dependency_list(
        config,
        &output(&mut command, "Linux dependency inspection")?,
    )?;
    println!("Linux runtime dependencies: {}", dependencies.join(", "));
    if !config["deb"].is_object() {
        config["deb"] = json!({});
    }
    config["deb"]["depends"] = json!(dependencies);
    Ok(())
}

struct Keychain {
    path: PathBuf,
    original: Vec<String>,
}

impl Drop for Keychain {
    fn drop(&mut self) {
        let _ = Command::new("security")
            .args(["list-keychains", "-d", "user", "-s"])
            .args(&self.original)
            .output();
        let _ = Command::new("security")
            .arg("delete-keychain")
            .arg(&self.path)
            .output();
    }
}

fn security(arguments: &[&str]) -> Result<String> {
    output(
        Command::new("security").args(arguments),
        "Apple signing setup; check certificate and signing secrets",
    )
}

fn setup_apple(root: &Path, environment: &mut Environment) -> Result<Keychain> {
    let certificate = root.join("certificate.p12");
    // Strict Base64; no secret material is attached to parse errors.
    let bytes = STANDARD
        .decode(&environment["APPLE_CERTIFICATE"])
        .map_err(|_| anyhow::anyhow!("Invalid Apple certificate encoding"))?;
    fs::write(&certificate, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&certificate, fs::Permissions::from_mode(0o600))?;
    }
    let original = shlex::split(&security(&["list-keychains", "-d", "user"])?)
        .context("Invalid keychain search list")?;
    // The guard also cleans up if any setup operation fails midway.
    let guard = Keychain {
        path: root.join("release.keychain-db"),
        original,
    };
    let keychain = guard.path.to_str().context("Invalid keychain path")?;
    let mut random = [0; 32];
    getrandom::fill(&mut random)
        .map_err(|_| anyhow::anyhow!("Could not generate keychain password"))?;
    let password = STANDARD.encode(random);
    security(&["create-keychain", "-p", &password, keychain])?;
    security(&["unlock-keychain", "-p", &password, keychain])?;
    security(&[
        "import",
        certificate.to_str().context("Invalid certificate path")?,
        "-k",
        keychain,
        "-P",
        environment
            .get("APPLE_CERTIFICATE_PASSWORD")
            .map(String::as_str)
            .unwrap_or(""),
        "-T",
        "/usr/bin/codesign",
    ])?;
    security(&[
        "set-key-partition-list",
        "-S",
        "apple-tool:,apple:",
        "-s",
        "-k",
        &password,
        keychain,
    ])?;
    checked(
        Command::new("security")
            .args(["list-keychains", "-d", "user", "-s"])
            .args(&guard.original)
            .arg(keychain),
        "Apple keychain search list setup",
    )?;
    environment.remove("APPLE_CERTIFICATE");
    environment.remove("APPLE_CERTIFICATE_PASSWORD");
    Ok(guard)
}

struct GeneratedConfig(PathBuf);
impl Drop for GeneratedConfig {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn validate_release(config: &Value, environment: &Environment, minisign: bool) -> Result<()> {
    if let Some(tag) = environment
        .get("GITHUB_REF")
        .and_then(|s| s.strip_prefix("refs/tags/v"))
    {
        ensure!(
            text(config, "version")? == tag,
            "Release tag must match packager.json version"
        );
    }
    ensure!(
        minisign,
        "MINISIGN_KEY is required for verifiable release downloads"
    );
    Ok(())
}

pub fn run() -> Result<()> {
    let system = env::consts::OS;
    let mut environment: Environment = env::vars().collect();
    let (mut config, apple, minisign) = configure(
        &read_json(Path::new("packager.json"))?,
        &environment,
        system,
    );
    validate_release(&config, &environment, minisign)?;
    let version = text(&config, "version")?.to_owned();
    let system_directory = if system == "macos" { "darwin" } else { system };
    config["outDir"] = json!(format!(
        "dist/packages/{version}/{system_directory}-{}",
        env::consts::ARCH
    ));
    for binary in config["binaries"].as_array().context("Missing binaries")? {
        let path = Path::new(text(&config, "binariesDir")?).join(format!(
            "{}{}",
            text(binary, "path")?,
            env::consts::EXE_SUFFIX
        ));
        ensure!(
            path.is_file(),
            "A binary from packager.json is missing: {}",
            path.display()
        );
    }
    if system == "linux" {
        linux_dependencies(&mut config)?;
    }
    // Upstream trace instrumentation includes signing arguments.
    environment.insert("RUST_LOG".into(), "info".into());
    environment.insert(
        "CARGO_PACKAGER_SIGN_PRIVATE_KEY".into(),
        environment["MINISIGN_KEY"].clone(),
    );
    environment.insert(
        "CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD".into(),
        environment
            .get("MINISIGN_PASSWORD")
            .cloned()
            .unwrap_or_default(),
    );
    let temporary = tempfile::Builder::new().prefix("xylo-signing-").tempdir()?;
    if system == "macos" {
        let iconset = temporary.path().join("xylo.iconset");
        fs::create_dir(&iconset)?;
        for size in [16, 32, 128, 256, 512] {
            for scale in [1, 2] {
                let pixels = (size * scale).to_string();
                let name = format!(
                    "icon_{size}x{size}{}.png",
                    if scale == 2 { "@2x" } else { "" }
                );
                checked(
                    Command::new("sips")
                        .args([
                            "-z",
                            &pixels,
                            &pixels,
                            "crates/desktop/assets/xylo-icon.png",
                            "--out",
                        ])
                        .arg(iconset.join(name)),
                    "macOS icon conversion",
                )?;
            }
        }
        let icon = temporary.path().join("xylo.icns");
        checked(
            Command::new("iconutil")
                .args(["-c", "icns"])
                .arg(&iconset)
                .arg("-o")
                .arg(&icon),
            "macOS icon assembly",
        )?;
        config["icons"] = json!([icon]);
    }
    let _keychain = if apple {
        Some(setup_apple(temporary.path(), &mut environment)?)
    } else {
        if system == "macos" {
            println!(
                "::warning::Using ad-hoc bundle signing; Developer ID signing and notarization are not configured"
            );
            environment.retain(|name, _| !name.starts_with("APPLE_"));
        }
        None
    };
    // Resource paths must resolve relative to the source manifest, not a temp dir.
    let generated = GeneratedConfig(PathBuf::from("packager.ci.json"));
    let mut native_config = config.clone();
    if system == "linux" {
        native_config["version"] = json!(version.replacen('-', "~", 1));
    }
    fs::write(&generated.0, serde_json::to_vec(&native_config)?)?;
    checked(
        Command::new("cargo")
            .args(["packager", "--config"])
            .arg(&generated.0)
            .arg("--formats")
            .arg(
                environment
                    .get("PACKAGE_FORMATS")
                    .context("PACKAGE_FORMATS is required")?,
            )
            .env_clear()
            .envs(&environment),
        "Package/sign/notarize",
    )?;
    assets::collect_assets(&config, &environment, apple)
}
