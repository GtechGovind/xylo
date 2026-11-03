use crate::util::*;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{env, fs, path::Path, process::Command};

pub fn verify_macos_bundle(app: &Path, version: &str) -> Result<()> {
    let plist = app.join("Contents/Info.plist");
    for (key, expected) in [
        ("CFBundleShortVersionString", version),
        ("CFBundleIdentifier", "com.qurkos.xylo"),
    ] {
        let actual = output(
            Command::new("plutil")
                .args(["-extract", key, "raw", "-o", "-"])
                .arg(&plist),
            "macOS bundle metadata inspection",
        )?;
        ensure!(
            actual.trim() == expected,
            "macOS app has the wrong version or bundle identifier"
        );
    }
    for name in ["xylo", "xylo-worker", "xylo-helper", "xylo-cli"] {
        executable(&app.join("Contents/MacOS").join(name))?;
    }
    let helper = app.join("Contents/Resources/sandbox-helper");
    ensure!(
        helper.join("run.mjs").is_file() && helper.join("node_modules").is_dir(),
        "macOS app is missing sandbox helper dependencies"
    );
    checked(
        Command::new("codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(app),
        "macOS bundle signature verification",
    )
}

fn verify_macos_packages(
    app: &Path,
    dmg: &Path,
    archive: &Path,
    version: &str,
    notarized: bool,
) -> Result<()> {
    verify_macos_bundle(app, version)?;
    if notarized {
        checked(
            Command::new("xcrun").args(["stapler", "validate"]).arg(app),
            "Apple notarization validation",
        )?;
    }
    checked(
        Command::new("hdiutil").arg("verify").arg(dmg),
        "DMG verification",
    )?;
    let temporary = tempfile::Builder::new()
        .prefix("xylo-macos-package-check-")
        .tempdir()?;
    let extracted = temporary.path().join("archive");
    fs::create_dir(&extracted)?;
    checked(
        Command::new("tar")
            .arg("-xzf")
            .arg(archive.canonicalize()?)
            .arg("-C")
            .arg(&extracted),
        "App archive extraction",
    )?;
    verify_macos_bundle(&extracted.join("Xylo.app"), version)?;
    let mount = temporary.path().join("disk");
    fs::create_dir(&mount)?;
    checked(
        Command::new("hdiutil")
            .arg("attach")
            .arg(dmg.canonicalize()?)
            .args(["-readonly", "-nobrowse", "-noautoopen", "-mountpoint"])
            .arg(&mount),
        "DMG mount",
    )?;
    let verification = (|| -> Result<()> {
        let installed = mount.join("Xylo.app");
        verify_macos_bundle(&installed, version)?;
        let reported = output(
            Command::new(installed.join("Contents/MacOS/xylo-cli")).arg("--version"),
            "Packaged macOS CLI startup",
        )?;
        ensure!(
            reported.contains(&format!("xylo-cli {version} ")),
            "macOS package reports the wrong application version"
        );
        Ok(())
    })();
    let detached = checked(
        Command::new("hdiutil").arg("detach").arg(&mount),
        "DMG detach",
    );
    verification?;
    detached?;
    println!("Verified macOS bundle signatures, DMG, app archive and packaged CLI startup");
    Ok(())
}

const LINUX_DESKTOP: &str = "usr/share/applications/xylo.desktop";
// cargo-packager currently copies the original 1024px PNG without resizing it.
const LINUX_ICON: &str = "usr/share/icons/hicolor/1024x1024/apps/xylo.png";
const ORIGINAL_ICON: &[u8] = include_bytes!("../../desktop/assets/xylo-icon.png");

/// Inspect extracted package files only: do not launch Xylo or install desktop entries.
fn verify_linux_desktop(installed: &Path) -> Result<()> {
    let desktop = installed.join(LINUX_DESKTOP);
    let contents = fs::read_to_string(&desktop).with_context(|| {
        format!(
            "Missing or unreadable Linux launcher: {}",
            desktop.display()
        )
    })?;
    let mut in_entry = false;
    let mut seen_entry = false;
    let mut fields = std::collections::BTreeMap::new();
    for line in contents.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            if in_entry {
                ensure!(
                    !seen_entry,
                    "Linux launcher has duplicate Desktop Entry groups"
                );
                seen_entry = true;
            }
        } else if in_entry {
            let (key, value) = line
                .split_once('=')
                .context("Linux Desktop Entry contains a malformed field")?;
            ensure!(
                fields.insert(key.trim(), value.trim()).is_none(),
                "Linux Desktop Entry has a duplicate {} field",
                key.trim()
            );
        }
    }
    ensure!(
        seen_entry,
        "Linux launcher is missing its Desktop Entry group"
    );
    for (key, expected) in [("Type", "Application"), ("Name", "Xylo"), ("Icon", "xylo")] {
        ensure!(
            fields.get(key).copied() == Some(expected),
            "Linux Desktop Entry must set {key}={expected}"
        );
    }
    let exec = fields
        .get("Exec")
        .context("Linux Desktop Entry is missing Exec")?;
    let arguments =
        shlex::split(exec).context("Linux Desktop Entry has an invalid Exec command")?;
    ensure!(
        arguments.first().is_some_and(|command| {
            command == "xylo" || (command.starts_with('/') && command.ends_with("/xylo"))
        }),
        "Linux Desktop Entry Exec must launch xylo"
    );
    if let Some(class) = fields.get("StartupWMClass") {
        ensure!(
            *class == "xylo",
            "Linux Desktop Entry StartupWMClass must be xylo"
        );
    }

    let icon = installed.join(LINUX_ICON);
    let png = fs::read(&icon).with_context(|| {
        format!(
            "Missing or unreadable Linux hicolor icon: {}",
            icon.display()
        )
    })?;
    ensure!(
        png.len() >= 24
            && &png[..16] == b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"
            && png[16..20] == 1024_u32.to_be_bytes()
            && png[20..24] == 1024_u32.to_be_bytes(),
        "Linux hicolor icon must be a square 1024x1024 PNG"
    );
    ensure!(
        png == ORIGINAL_ICON,
        "Linux hicolor icon does not match the original Xylo icon"
    );
    Ok(())
}

/// Reads follow symlinks, rejecting missing/broken links as well as stale copies.
fn verify_linux_appimage_entries(installed: &Path) -> Result<()> {
    for (root_entry, counterpart) in [
        ("xylo.desktop", LINUX_DESKTOP),
        ("xylo.png", LINUX_ICON),
        (".DirIcon", LINUX_ICON),
    ] {
        let entry = installed.join(root_entry);
        let actual = fs::read(&entry).with_context(|| {
            format!("Missing or unreadable AppImage entry: {}", entry.display())
        })?;
        let expected = fs::read(installed.join(counterpart))
            .with_context(|| format!("Missing AppImage installed counterpart: {counterpart}"))?;
        ensure!(
            actual == expected,
            "AppImage {root_entry} does not match {counterpart}"
        );
    }
    Ok(())
}

fn verify_linux_install(installed: &Path, version: &str) -> Result<()> {
    verify_linux_desktop(installed)?;
    for binary in ["xylo", "xylo-worker", "xylo-helper", "xylo-cli"] {
        executable(&installed.join("usr/bin").join(binary))?;
    }
    let helper = installed.join("usr/lib/xylo/sandbox-helper");
    ensure!(
        helper.join("run.mjs").is_file() && helper.join("node_modules").is_dir(),
        "Linux package is missing sandbox helper dependencies"
    );
    let reported = output(
        Command::new(installed.join("usr/bin/xylo-cli")).arg("--version"),
        "Packaged Linux CLI startup",
    )?;
    ensure!(
        reported.contains(&format!("xylo-cli {version} ")),
        "Linux package reports the wrong application version"
    );
    Ok(())
}

fn verify_linux_packages(downloads: &Path, version: &str) -> Result<()> {
    let deb = exactly_one(downloads, ".deb")?;
    let native_version = output(
        Command::new("dpkg-deb")
            .arg("--field")
            .arg(&deb)
            .arg("Version"),
        "Debian version inspection",
    )?;
    ensure!(
        native_version.trim() == version.replacen('-', "~", 1),
        "Linux package has incorrect release-candidate version ordering"
    );
    let dependencies = output(
        Command::new("dpkg-deb")
            .arg("--field")
            .arg(&deb)
            .arg("Depends"),
        "Debian dependency inspection",
    )?;
    for name in ["libc6", "bubblewrap", "socat", "ripgrep", "libvulkan1"] {
        ensure!(
            dependencies.contains(name),
            "Linux package is missing its {name} dependency"
        );
    }
    let appimage = exactly_one(downloads, ".AppImage")?;
    let temporary = tempfile::Builder::new()
        .prefix("xylo-package-check-")
        .tempdir()?;
    let extracted = temporary.path().join("deb");
    checked(
        Command::new("dpkg-deb")
            .arg("--extract")
            .arg(&deb)
            .arg(&extracted),
        "Debian extraction",
    )?;
    make_executable(&appimage)?;
    checked(
        Command::new(appimage.canonicalize()?)
            .arg("--appimage-extract")
            .current_dir(temporary.path()),
        "AppImage extraction",
    )?;
    verify_linux_install(&extracted, version)?;
    let appdir = temporary.path().join("squashfs-root");
    verify_linux_install(&appdir, version)?;
    verify_linux_appimage_entries(&appdir)?;
    println!(
        "Verified Linux launchers, icons, installed files, runtime dependencies and CLI startup"
    );
    Ok(())
}

pub fn collect_assets(config: &Value, environment: &Environment, apple: bool) -> Result<()> {
    let (name, arch) = platform_name(env::consts::OS, env::consts::ARCH)?;
    let version = text(config, "version")?;
    let packages = Path::new(text(config, "outDir")?);
    let destination = Path::new("dist/release").join(format!("{name}-{arch}"));
    reset_directory(&destination)?;
    let prefix = format!("Xylo-{version}-{name}-{arch}");
    let extensions: &[&str] = match name {
        "macos" => &[".dmg"],
        "windows" => &[".exe"],
        _ => &[".deb", ".AppImage"],
    };
    let mut downloads = Vec::new();
    for extension in extensions {
        let source = exactly_one(packages, extension)?;
        let target = destination.join(format!("{prefix}{extension}"));
        fs::copy(source, &target)?;
        downloads.push(target);
    }
    if name == "linux" {
        verify_linux_packages(&destination, version)?;
    }
    if name == "macos" {
        let app = exactly_one(packages, ".app")?;
        ensure!(
            app.file_name().is_some_and(|s| s == "Xylo.app"),
            "Expected Xylo.app bundle"
        );
        let archive = destination.join(format!("{prefix}.app.tar.gz"));
        // Native tar preserves executable bits, symlinks and bundle metadata.
        checked(
            Command::new("tar")
                .arg("-czf")
                .arg(
                    archive
                        .canonicalize()
                        .unwrap_or(env::current_dir()?.join(&archive)),
                )
                .arg("-C")
                .arg(app.parent().expect("bundle parent"))
                .arg("Xylo.app"),
            "App archive creation",
        )?;
        verify_macos_packages(&app, &downloads[0], &archive, version, apple)?;
        downloads.push(archive);
    }
    let checksum = destination.join(format!("{prefix}.sha256"));
    let mut checksums = String::new();
    let mut files = Vec::new();
    for path in &downloads {
        let hash = sha256(path)?;
        let filename = path
            .file_name()
            .expect("download filename")
            .to_string_lossy();
        checksums.push_str(&format!("{hash}  {filename}\n"));
        files.push(json!({"name": filename, "sha256": hash, "size": path.metadata()?.len()}));
    }
    fs::write(&checksum, checksums)?;
    let receipt = destination.join(format!("{prefix}.json"));
    let signing = if name == "linux" {
        "not_applicable"
    } else if apple {
        "verified"
    } else if name == "macos" {
        "ad_hoc"
    } else {
        "not_configured"
    };
    fs::write(
        &receipt,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({
                "version": version, "commit": environment.get("GITHUB_SHA").map(String::as_str).unwrap_or("local"),
                "platform": name, "arch": arch, "artifact_signatures": "verified", "platform_signing": signing, "files": files
            }))?
        ),
    )?;
    downloads.extend([checksum, receipt]);
    for path in &downloads {
        checked(
            Command::new("cargo")
                .args(["packager", "signer", "sign"])
                .arg(path)
                .env_clear()
                .envs(environment),
            "Project artifact signing",
        )?;
        crate::signatures::verify(path)?;
    }
    fs::write(
        destination.join(crate::signatures::PUBLIC_FILENAME),
        crate::signatures::PINNED_PUBLIC,
    )?;
    println!("Verified release downloads: {}", destination.display());
    Ok(())
}

/// Replaces the workflow's installed-browser smoke test, not the upstream build.
pub fn verify_payload() -> Result<()> {
    let packages = Path::new("dist/packages");
    match env::consts::OS {
        "macos" => {
            let app = exactly_one(packages, ".app")?;
            let browser = app.join("Contents/MacOS/xylo-obscura");
            executable(&browser)?;
            ensure!(
                app.join("Contents/Resources/licenses/obscura/upstream/LICENSE")
                    .is_file(),
                "Missing Obscura license"
            );
            checked(
                Command::new("codesign")
                    .args(["--verify", "--strict"])
                    .arg(browser),
                "Nested browser signature verification",
            )?;
            checked(
                Command::new("codesign")
                    .args(["--verify", "--deep", "--strict"])
                    .arg(app),
                "App signature verification",
            )?;
        }
        "linux" => {
            let deb = exactly_one(packages, ".deb")?;
            let temporary = tempfile::tempdir()?;
            checked(
                Command::new("dpkg-deb")
                    .arg("--extract")
                    .arg(deb)
                    .arg(temporary.path()),
                "Installed browser extraction",
            )?;
            let browser = temporary.path().join("usr/bin/xylo-obscura");
            executable(&browser)?;
            ensure!(
                temporary
                    .path()
                    .join("usr/lib/xylo/licenses/obscura/upstream/LICENSE")
                    .is_file(),
                "Missing Obscura license"
            );
            checked(
                Command::new(browser).args([
                    "fetch",
                    "data:text/html,<title>ok</title>",
                    "--eval",
                    "1+1",
                ]),
                "Installed browser smoke test",
            )?;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESKTOP: &str = "[Desktop Entry]\nType=Application\nName=Xylo\nIcon=xylo\nExec=xylo \n";

    fn linux_fixture() -> tempfile::TempDir {
        let temporary = tempfile::tempdir().unwrap();
        for (relative, contents) in [
            (LINUX_DESKTOP, DESKTOP.as_bytes()),
            (LINUX_ICON, ORIGINAL_ICON),
        ] {
            let path = temporary.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        temporary
    }

    fn appimage_fixture() -> tempfile::TempDir {
        let temporary = linux_fixture();
        for (entry, counterpart) in [
            ("xylo.desktop", LINUX_DESKTOP),
            ("xylo.png", LINUX_ICON),
            (".DirIcon", LINUX_ICON),
        ] {
            fs::copy(
                temporary.path().join(counterpart),
                temporary.path().join(entry),
            )
            .unwrap();
        }
        temporary
    }

    #[test]
    fn linux_desktop_accepts_packager_blank_exec_arg_and_original_icon() {
        let temporary = linux_fixture();
        // No executables exist in this fixture: verification must only read files.
        verify_linux_desktop(temporary.path()).unwrap();
        assert_eq!(&ORIGINAL_ICON[..16], b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
        assert_eq!(&ORIGINAL_ICON[16..24], &[0, 0, 4, 0, 0, 0, 4, 0]);
    }

    #[test]
    fn linux_desktop_accepts_xylo_arguments_absolute_command_and_optional_wm_class() {
        let temporary = linux_fixture();
        for command in ["xylo", "xylo --new-window %U", "\"/usr/bin/xylo\" %F"] {
            for class in ["", "StartupWMClass=xylo\n"] {
                let desktop = DESKTOP.replace("Exec=xylo \n", &format!("Exec={command}\n{class}"));
                fs::write(temporary.path().join(LINUX_DESKTOP), desktop).unwrap();
                verify_linux_desktop(temporary.path()).unwrap();
            }
        }
    }

    #[test]
    fn linux_desktop_requires_correctly_named_launcher_and_icon() {
        for relative in [LINUX_DESKTOP, LINUX_ICON] {
            let temporary = linux_fixture();
            let path = temporary.path().join(relative);
            fs::rename(&path, path.with_file_name("wrong-name")).unwrap();
            let error = verify_linux_desktop(temporary.path()).unwrap_err();
            assert!(
                error.to_string().contains(path.to_string_lossy().as_ref()),
                "{error:#}"
            );
        }
    }

    #[test]
    fn linux_desktop_rejects_missing_identity_fields() {
        for field in [
            "Type=Application\n",
            "Name=Xylo\n",
            "Icon=xylo\n",
            "Exec=xylo \n",
        ] {
            let temporary = linux_fixture();
            fs::write(
                temporary.path().join(LINUX_DESKTOP),
                DESKTOP.replace(field, ""),
            )
            .unwrap();
            let error = verify_linux_desktop(temporary.path()).unwrap_err();
            let key = field.split_once('=').unwrap().0;
            assert!(error.to_string().contains(key), "{field}: {error:#}");
        }
    }

    #[test]
    fn linux_desktop_rejects_changed_identity_and_wrong_executable() {
        for (field, value) in [
            ("Type=Application", "Type=Link"),
            ("Name=Xylo", "Name=Other"),
            ("Icon=xylo", "Icon=other"),
            ("Icon=xylo", "Icon=xylo.png"),
            ("Exec=xylo ", "Exec=xylo-cli"),
            ("Exec=xylo ", "Exec=/usr/bin/other xylo"),
            ("Exec=xylo ", "Exec=env xylo"),
            ("Exec=xylo ", "Exec="),
            ("Exec=xylo ", "Exec=\"xylo"),
        ] {
            let temporary = linux_fixture();
            fs::write(
                temporary.path().join(LINUX_DESKTOP),
                DESKTOP.replace(field, value),
            )
            .unwrap();
            let error = verify_linux_desktop(temporary.path()).unwrap_err();
            let key = field.split_once('=').unwrap().0;
            assert!(error.to_string().contains(key), "{value}: {error:#}");
        }
    }

    #[test]
    fn linux_desktop_rejects_wrong_or_empty_startup_wm_class() {
        let temporary = linux_fixture();
        for class in ["Xylo", "com.qurkos.xylo", ""] {
            fs::write(
                temporary.path().join(LINUX_DESKTOP),
                format!("{DESKTOP}StartupWMClass={class}\n"),
            )
            .unwrap();
            assert!(
                verify_linux_desktop(temporary.path())
                    .unwrap_err()
                    .to_string()
                    .contains("StartupWMClass")
            );
        }
    }

    #[test]
    fn linux_desktop_does_not_take_identity_from_other_groups() {
        let temporary = linux_fixture();
        for desktop in [
            DESKTOP.replace("[Desktop Entry]", "[Desktop Action Other]"),
            DESKTOP.replace("Name=Xylo\n", "") + "[Desktop Action Other]\nName=Xylo\n",
            DESKTOP.to_owned() + "[Desktop Entry]\nName=Xylo\n",
            DESKTOP.to_owned() + "Name=Other\n",
            DESKTOP.to_owned() + "malformed field\n",
        ] {
            fs::write(temporary.path().join(LINUX_DESKTOP), desktop).unwrap();
            assert!(verify_linux_desktop(temporary.path()).is_err());
        }
        // Action/localized names must not override the main application's identity.
        fs::write(
            temporary.path().join(LINUX_DESKTOP),
            format!("# generated launcher\n{DESKTOP}Name[de]=Andere\n[Desktop Action Other]\nName=Other\nExec=other\n"),
        )
        .unwrap();
        verify_linux_desktop(temporary.path()).unwrap();
    }

    #[test]
    fn linux_desktop_rejects_non_png_truncated_or_non_square_icon() {
        let temporary = linux_fixture();
        let mut wrong_dimensions = ORIGINAL_ICON.to_vec();
        wrong_dimensions[20..24].copy_from_slice(&512_u32.to_be_bytes());
        for png in [
            b"not a PNG".to_vec(),
            ORIGINAL_ICON[..20].to_vec(),
            wrong_dimensions,
        ] {
            fs::write(temporary.path().join(LINUX_ICON), png).unwrap();
            assert!(
                verify_linux_desktop(temporary.path())
                    .unwrap_err()
                    .to_string()
                    .contains("square 1024x1024 PNG")
            );
        }
    }

    #[test]
    fn linux_desktop_rejects_replaced_icon_even_with_matching_png_header() {
        let temporary = linux_fixture();
        let mut changed = ORIGINAL_ICON.to_vec();
        let last = changed.len() - 1;
        changed[last] ^= 1;
        fs::write(temporary.path().join(LINUX_ICON), changed).unwrap();
        assert!(
            verify_linux_desktop(temporary.path())
                .unwrap_err()
                .to_string()
                .contains("does not match the original Xylo icon")
        );
    }

    #[test]
    fn appimage_accepts_matching_root_entries() {
        let temporary = appimage_fixture();
        verify_linux_desktop(temporary.path()).unwrap();
        verify_linux_appimage_entries(temporary.path()).unwrap();
    }

    #[test]
    fn appimage_requires_correctly_named_root_entries() {
        for name in ["xylo.desktop", "xylo.png", ".DirIcon"] {
            let temporary = appimage_fixture();
            fs::rename(
                temporary.path().join(name),
                temporary.path().join("wrong-name"),
            )
            .unwrap();
            let error = verify_linux_appimage_entries(temporary.path()).unwrap_err();
            assert!(error.to_string().contains(name), "{error:#}");
        }
    }

    #[test]
    fn appimage_rejects_root_entries_that_differ_from_installed_files() {
        for name in ["xylo.desktop", "xylo.png", ".DirIcon"] {
            let temporary = appimage_fixture();
            fs::write(temporary.path().join(name), b"stale packaged entry").unwrap();
            let error = verify_linux_appimage_entries(temporary.path()).unwrap_err();
            assert!(error.to_string().contains(name), "{error:#}");
            assert!(error.to_string().contains("does not match"), "{error:#}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn appimage_follows_relative_symlinks_including_diricon_chain() {
        let temporary = linux_fixture();
        for (entry, target) in [
            ("xylo.desktop", LINUX_DESKTOP),
            ("xylo.png", LINUX_ICON),
            (".DirIcon", "xylo.png"),
        ] {
            std::os::unix::fs::symlink(target, temporary.path().join(entry)).unwrap();
        }
        verify_linux_appimage_entries(temporary.path()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn appimage_rejects_broken_root_symlinks() {
        for name in ["xylo.desktop", "xylo.png", ".DirIcon"] {
            let temporary = appimage_fixture();
            let entry = temporary.path().join(name);
            fs::remove_file(&entry).unwrap();
            std::os::unix::fs::symlink("missing-target", entry).unwrap();
            let error = verify_linux_appimage_entries(temporary.path()).unwrap_err();
            assert!(error.to_string().contains(name), "{error:#}");
        }
    }
}
