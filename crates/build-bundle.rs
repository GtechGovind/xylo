//! Complete, self-contained local macOS bundles, without interpreter tooling.
use crate::{
    browser::{self, Profile},
    run,
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use std::{
    env, fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const BINARIES: [&str; 5] = [
    "xylo",
    "xylo-worker",
    "xylo-helper",
    "xylo-cli",
    "xylo-obscura",
];

/// Build and verify the entire app before touching an existing `dist/Xylo.app`.
/// `--skip-build` still installs npm dependencies and assembles/signs the bundle.
pub fn build(root: &Path, profile: Profile, skip_build: bool) -> Result<()> {
    ensure!(
        cfg!(target_os = "macos"),
        "bundle-macos requires macOS (sips, iconutil and codesign)"
    );
    let root = root.canonicalize().context("resolving workspace root")?;
    let metadata: serde_json::Value = serde_json::from_slice(
        &fs::read(root.join("packager.json")).context("reading packager.json")?,
    )
    .context("parsing packager.json")?;
    let version = metadata
        .get("version")
        .and_then(serde_json::Value::as_str)
        .context("packager.json must contain a string version")?;
    let plist = info_plist(version)?;
    let target = target_directory(&root, env::var_os("CARGO_TARGET_DIR").as_deref());
    let source = target.join(profile.name());
    let out = root.join("dist");
    fs::create_dir_all(&out)?;
    // Serialize bundle writers, including npm ci and final replacement. The
    // lock lives outside the app so replacing the bundle cannot invalidate it.
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(out.join(".bundle-macos.lock"))?;
    lock.lock_exclusive()?;

    if !skip_build {
        let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut command = Command::new(cargo);
        command
            .args([
                "build",
                "--locked",
                "-p",
                "xylo",
                "-p",
                "xylo-worker",
                "-p",
                "xylo-helper",
                "-p",
                "xylo-cli",
                "--bins",
            ])
            .arg("--target-dir")
            .arg(&target)
            .current_dir(&root);
        if profile == Profile::Release {
            command.arg("--release");
        }
        run(&mut command)?;
        let host = browser::host(&root)?;
        ensure!(
            host.ends_with("-apple-darwin"),
            "bundle-macos requires a native macOS Rust toolchain"
        );
        browser::build(&root, &source, profile, &host, &host)?;
    }
    for name in BINARIES {
        let path = source.join(name);
        ensure!(
            path.is_file(),
            "Missing {}; run without --skip-build to build the complete app.",
            path.display()
        );
    }
    ensure!(
        source.join("obscura-licenses/upstream/LICENSE").is_file(),
        "Missing Obscura attribution resources; run without --skip-build."
    );

    let temporary = tempfile::Builder::new()
        .prefix("xylo-bundle-")
        .tempdir_in(&out)?;
    let stage = temporary.path().join("Xylo.app");
    let contents = stage.join("Contents");
    let macos = contents.join("MacOS");
    let resources = contents.join("Resources");
    fs::create_dir_all(&macos)?;
    fs::create_dir_all(&resources)?;
    for name in BINARIES {
        fs::copy(source.join(name), macos.join(name))
            .with_context(|| format!("copying bundle executable {name}"))?;
    }
    fs::write(contents.join("Info.plist"), plist)?;
    make_icons(&root, temporary.path(), &resources)?;

    let helper = root.join("crates/services/sandbox-helper");
    run(Command::new("npm")
        .arg("ci")
        .arg("--prefix")
        .arg(&helper)
        .args(["--ignore-scripts", "--no-audit", "--no-fund"])
        .current_dir(&root))?;
    copy_resources(&helper, &resources.join("sandbox-helper"))?;
    for name in ["LICENSE", "NOTICE"] {
        fs::copy(root.join(name), resources.join(name))?;
    }
    copy_resources(
        &source.join("obscura-licenses"),
        &resources.join("licenses/obscura"),
    )?;

    // Linker signatures do not seal an application bundle. Sign all nested
    // native code (including npm .node/dylib files) before sealing resources.
    let mut native_files = Vec::new();
    collect_mach_o(&stage, &mut native_files)?;
    native_files.sort_by(|left, right| {
        right
            .components()
            .count()
            .cmp(&left.components().count())
            .then_with(|| left.cmp(right))
    });
    for path in native_files {
        run(Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(path))?;
    }
    run(Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(&stage))?;
    run(Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(&stage))?;
    let destination = out.join("Xylo.app");
    install_verified_bundle(&stage, &destination)?;
    println!(
        "Built {} ({}); all five executables, including source-built Obscura, are included.",
        destination.display(),
        profile.name()
    );
    Ok(())
}

fn target_directory(root: &Path, configured: Option<&std::ffi::OsStr>) -> PathBuf {
    let directory = configured
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    if directory.is_absolute() {
        directory
    } else {
        root.join(directory)
    }
}

fn xml_escape(value: &str) -> Result<String> {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            '\t'
            | '\n'
            | '\r'
            | '\u{20}'..='\u{d7ff}'
            | '\u{e000}'..='\u{fffd}'
            | '\u{10000}'..='\u{10ffff}' => escaped.push(character),
            _ => bail!(
                "bundle metadata contains a character invalid in XML 1.0: U+{:04X}",
                character as u32
            ),
        }
    }
    Ok(escaped)
}

fn info_plist(version: &str) -> Result<String> {
    ensure!(
        !version.is_empty(),
        "packager.json version must not be empty"
    );
    let short_version = xml_escape(version)?;
    let build_version = xml_escape(version.split('-').next().unwrap_or(version))?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>xylo</string>
    <key>CFBundleDisplayName</key><string>Xylo</string>
    <key>CFBundleName</key><string>Xylo</string>
    <key>CFBundleIdentifier</key><string>com.qurkos.xylo</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>{short_version}</string>
    <key>CFBundleVersion</key><string>{build_version}</string>
    <key>CFBundleIconFile</key><string>xylo.icns</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
</dict>
</plist>
"#
    ))
}

fn make_icons(root: &Path, temporary: &Path, resources: &Path) -> Result<()> {
    let iconset = temporary.join("xylo.iconset");
    fs::create_dir(&iconset)?;
    let source = root.join("crates/desktop/assets/xylo-icon.png");
    for size in [16, 32, 128, 256, 512] {
        for scale in [1, 2] {
            let suffix = if scale == 2 { "@2x" } else { "" };
            let filename = format!("icon_{size}x{size}{suffix}.png");
            let pixels = (size * scale).to_string();
            run(Command::new("sips")
                .arg("-z")
                .arg(&pixels)
                .arg(&pixels)
                .arg(&source)
                .arg("--out")
                .arg(iconset.join(filename))
                .stdout(Stdio::null()))?;
        }
    }
    run(Command::new("iconutil")
        .args(["-c", "icns"])
        .arg(iconset)
        .arg("-o")
        .arg(resources.join("xylo.icns")))
}

/// Like Python copytree's default behavior, dereference resource symlinks.
/// npm creates node_modules/.bin links; rejecting them breaks a normal install,
/// while copying absolute links would make the app depend on the source tree.
/// Detect directory cycles explicitly instead of recursing without a bound.
fn copy_resources(source: &Path, destination: &Path) -> Result<()> {
    copy_resource(source, destination, &mut Vec::new())
}

fn copy_resource(source: &Path, destination: &Path, ancestors: &mut Vec<PathBuf>) -> Result<()> {
    let metadata = fs::metadata(source)
        .with_context(|| format!("reading bundle resource {}", source.display()))?;
    if metadata.is_file() {
        fs::copy(source, destination)
            .with_context(|| format!("copying bundle resource {}", source.display()))?;
    } else if metadata.is_dir() {
        let canonical = source.canonicalize()?;
        ensure!(
            !ancestors.contains(&canonical),
            "symlink cycle in bundle resources: {}",
            source.display()
        );
        ancestors.push(canonical);
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            if entry.file_name() == ".git" {
                continue;
            }
            copy_resource(
                &entry.path(),
                &destination.join(entry.file_name()),
                ancestors,
            )?;
        }
        ancestors.pop();
    } else {
        bail!("unsupported bundle resource: {}", source.display());
    }
    Ok(())
}

fn is_mach_o(magic: &[u8]) -> bool {
    matches!(
        magic,
        // 32-/64-bit Mach-O, both byte orders.
        [0xfe, 0xed, 0xfa, 0xce] | [0xce, 0xfa, 0xed, 0xfe] |
        [0xfe, 0xed, 0xfa, 0xcf] | [0xcf, 0xfa, 0xed, 0xfe] |
        // Universal (fat) Mach-O, including fat_arch_64 and swapped headers.
        [0xca, 0xfe, 0xba, 0xbe] | [0xbe, 0xba, 0xfe, 0xca] |
        [0xca, 0xfe, 0xba, 0xbf] | [0xbf, 0xba, 0xfe, 0xca]
    )
}

fn collect_mach_o(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_mach_o(&entry.path(), files)?;
        } else if kind.is_file() {
            let mut magic = Vec::with_capacity(4);
            fs::File::open(entry.path())?
                .take(4)
                .read_to_end(&mut magic)?;
            if is_mach_o(&magic) {
                files.push(entry.path());
            }
        }
    }
    Ok(())
}

/// Rename on the same filesystem, retaining the old app until the new one is
/// installed. Roll back a failed rename instead of deleting the working app.
fn install_verified_bundle(stage: &Path, destination: &Path) -> Result<()> {
    let exists = match fs::symlink_metadata(destination) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).context("checking previous bundle"),
    };
    if !exists {
        return fs::rename(stage, destination).context("installing verified bundle");
    }
    let backup = tempfile::Builder::new()
        .prefix("xylo-bundle-previous-")
        .tempdir_in(destination.parent().context("bundle destination parent")?)?;
    let previous = backup.path().join("Xylo.app");
    fs::rename(destination, &previous).context("retaining previous bundle for rollback")?;
    if let Err(install_error) = fs::rename(stage, destination) {
        if let Err(rollback_error) = fs::rename(&previous, destination) {
            // Do not let TempDir cleanup destroy the only remaining old app.
            let retained = backup.keep();
            bail!(
                "installing verified bundle failed: {install_error}; rollback failed: {rollback_error}; previous app retained at {}",
                retained.join("Xylo.app").display()
            );
        }
        return Err(install_error).context("installing verified bundle (previous bundle restored)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_metadata_is_escaped_and_invalid_characters_are_rejected() {
        assert_eq!(
            xml_escape("<&>\"' café").unwrap(),
            "&lt;&amp;&gt;&quot;&apos; café"
        );
        assert_eq!(xml_escape("\t\r\n").unwrap(), "\t\r\n");
        for value in ["\0", "\u{1f}", "\u{fffe}", "\u{ffff}"] {
            assert!(xml_escape(value).is_err());
        }
        assert!(info_plist("").is_err());
        assert!(info_plist("1.0\0").is_err());
        let plist = info_plist("1.2.3-rc.4<&\"'").unwrap();
        assert!(plist.contains(
            "<key>CFBundleShortVersionString</key><string>1.2.3-rc.4&lt;&amp;&quot;&apos;</string>"
        ));
        assert!(plist.contains("<key>CFBundleVersion</key><string>1.2.3</string>"));
        for field in [
            "CFBundleExecutable",
            "CFBundleDisplayName",
            "CFBundleName",
            "CFBundleIdentifier",
            "CFBundlePackageType",
            "CFBundleIconFile",
            "NSHighResolutionCapable",
            "NSPrincipalClass",
        ] {
            assert!(plist.contains(&format!("<key>{field}</key>")));
        }
    }

    #[test]
    fn recognizes_all_mach_o_headers_but_not_short_or_other_files() {
        for header in [
            [0xfe, 0xed, 0xfa, 0xce],
            [0xce, 0xfa, 0xed, 0xfe],
            [0xfe, 0xed, 0xfa, 0xcf],
            [0xcf, 0xfa, 0xed, 0xfe],
            [0xca, 0xfe, 0xba, 0xbe],
            [0xbe, 0xba, 0xfe, 0xca],
            [0xca, 0xfe, 0xba, 0xbf],
            [0xbf, 0xba, 0xfe, 0xca],
        ] {
            assert!(is_mach_o(&header));
        }
        for other in [
            &b""[..],
            &b"\xfe\xed\xfa"[..],
            &b"\x7fELF"[..],
            &b"MZ\0\0"[..],
            &b"text"[..],
            &b"\x89PNG"[..],
        ] {
            assert!(!is_mach_o(other));
        }
    }

    #[test]
    fn native_file_discovery_is_recursive_and_handles_empty_files() {
        let temporary = tempfile::tempdir().unwrap();
        let nested = temporary.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let binary = nested.join("addon.node");
        fs::write(&binary, b"\xcf\xfa\xed\xfeextra").unwrap();
        fs::write(temporary.path().join("empty"), b"").unwrap();
        fs::write(temporary.path().join("text"), b"not native code").unwrap();
        let mut files = Vec::new();
        collect_mach_o(temporary.path(), &mut files).unwrap();
        assert_eq!(files, [binary]);
    }

    #[test]
    fn target_directory_resolves_default_relative_and_absolute_settings() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        assert_eq!(target_directory(root, None), root.join("target"));
        assert_eq!(
            target_directory(root, Some(std::ffi::OsStr::new("custom"))),
            root.join("custom")
        );
        let absolute = root.join("elsewhere");
        assert_eq!(target_directory(root, Some(absolute.as_os_str())), absolute);
    }

    #[test]
    fn resources_skip_git_but_keep_dependencies_and_license_files() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir_all(source.join("node_modules/package/.git")).unwrap();
        fs::write(source.join("node_modules/package/LICENSE"), "license").unwrap();
        fs::write(
            source.join("node_modules/package/.git/config"),
            "not a resource",
        )
        .unwrap();
        let destination = temporary.path().join("copy");
        copy_resources(&source, &destination).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("node_modules/package/LICENSE")).unwrap(),
            "license"
        );
        assert!(!destination.join("node_modules/package/.git").exists());
    }

    #[cfg(unix)]
    #[test]
    fn npm_symlinks_become_independent_executable_files_and_cycles_fail() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir_all(source.join("node_modules/.bin")).unwrap();
        fs::create_dir_all(source.join("node_modules/package")).unwrap();
        let tool = source.join("node_modules/package/tool");
        fs::write(&tool, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        symlink("../package/tool", source.join("node_modules/.bin/tool")).unwrap();
        symlink("package", source.join("node_modules/alias")).unwrap();
        let destination = temporary.path().join("copy");
        copy_resources(&source, &destination).unwrap();
        let copied_tool = destination.join("node_modules/.bin/tool");
        assert!(
            !fs::symlink_metadata(&copied_tool)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_ne!(
            fs::metadata(&copied_tool).unwrap().permissions().mode() & 0o111,
            0
        );
        assert_eq!(
            fs::read_to_string(destination.join("node_modules/alias/tool")).unwrap(),
            "#!/bin/sh\n"
        );
        fs::remove_dir_all(&source).unwrap();
        assert_eq!(fs::read_to_string(copied_tool).unwrap(), "#!/bin/sh\n");
        fs::create_dir(&source).unwrap();
        symlink(".", source.join("loop")).unwrap();
        assert!(copy_resources(&source, &temporary.path().join("cycle-copy")).is_err());
    }

    #[test]
    fn replacement_installs_complete_stage_and_rolls_back_on_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("Xylo.app");
        let stage = temporary.path().join("stage.app");
        fs::create_dir(&stage).unwrap();
        fs::write(stage.join("version"), "first").unwrap();
        install_verified_bundle(&stage, &destination).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("version")).unwrap(),
            "first"
        );
        // A missing stage simulates a rename failure after saving the old app.
        assert!(install_verified_bundle(&stage, &destination).is_err());
        assert_eq!(
            fs::read_to_string(destination.join("version")).unwrap(),
            "first"
        );
        fs::create_dir(&stage).unwrap();
        fs::write(stage.join("version"), "second").unwrap();
        install_verified_bundle(&stage, &destination).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("version")).unwrap(),
            "second"
        );
        assert!(!stage.exists());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_is_rejected_before_filesystem_or_process_work() {
        let error = build(Path::new("nonexistent-workspace"), Profile::Debug, true).unwrap_err();
        assert!(error.to_string().contains("requires macOS"));
    }
}
