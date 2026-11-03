use crate::{package::*, publish::*, util::*};
use serde_json::{Value, json};
use std::{fs, path::Path};

#[test]
fn absent_apple_credentials_still_seal_macos_without_mutating_source() {
    let config = json!({"macos": {"signingIdentity": "old"}, "licenseFile": "LICENSE"});
    let environment = Environment::from([("MINISIGN_KEY".into(), "key".into())]);
    let (result, apple, minisign) = configure(&config, &environment, "macos");
    assert!(!apple);
    assert!(minisign);
    assert_eq!(result["macos"]["signingIdentity"], "-");
    assert_eq!(config["macos"]["signingIdentity"], "old");
    assert!(result.get("licenseFile").is_none());
    assert_eq!(
        configure(&config, &Environment::new(), "windows").0["licenseFile"],
        "LICENSE"
    );
    assert!(
        configure(&config, &Environment::new(), "linux").0["macos"]
            .get("signingIdentity")
            .is_none()
    );
}

#[test]
fn apple_signing_requires_every_credential_and_native_macos() {
    let mut environment: Environment = [
        "APPLE_CERTIFICATE",
        "APPLE_SIGNING_IDENTITY",
        "APPLE_ID",
        "APPLE_PASSWORD",
        "APPLE_TEAM_ID",
    ]
    .into_iter()
    .map(|s| (s.into(), "configured".into()))
    .collect();
    let (config, apple, minisign) = configure(&json!({}), &environment, "macos");
    assert!(apple);
    assert!(!minisign);
    assert_eq!(config["macos"]["signingIdentity"], "configured");
    assert!(!configure(&json!({}), &environment, "linux").1);
    for name in environment.clone().keys() {
        let saved = environment.insert(name.clone(), String::new());
        assert!(!configure(&json!({}), &environment, "macos").1);
        environment.insert(name.clone(), saved.unwrap());
    }
}

#[test]
fn tag_mismatch_and_absent_project_key_stop_packaging() {
    let config = json!({"version": "1.0.0-rc.1"});
    assert!(
        validate_release(&config, &Environment::new(), false)
            .unwrap_err()
            .to_string()
            .contains("MINISIGN_KEY")
    );
    let environment = Environment::from([("GITHUB_REF".into(), "refs/tags/v1.0.0".into())]);
    assert!(
        validate_release(&config, &environment, true)
            .unwrap_err()
            .to_string()
            .contains("tag must match")
    );
    assert!(validate_release(&config, &Environment::new(), true).is_ok());
}

#[test]
fn linux_dependencies_preserve_abi_metadata_and_dynamic_requirements() {
    let config = json!({"deb": {"depends": ["ca-certificates", "socat"]}});
    let dependencies = dependency_list(
        &config,
        "shlibs:Depends=libc6 (>= 2.39), libgcc-s1 (>= 4.2)\n",
    )
    .unwrap();
    for name in [
        "libc6 (>= 2.39)",
        "libgcc-s1 (>= 4.2)",
        "libvulkan1",
        "libegl1",
        "libwayland-client0",
        "bubblewrap",
        "socat",
        "ripgrep",
        "ca-certificates",
    ] {
        assert!(dependencies.contains(&name.to_owned()), "{name}");
    }
    assert_eq!(dependencies.iter().filter(|s| *s == "socat").count(), 1);
}

#[test]
fn missing_linux_dependency_metadata_fails_closed() {
    for metadata in ["", "shlibs:Depends=", "shlibs:Depends=libc6, "] {
        assert!(dependency_list(&json!({}), metadata).is_err());
    }
}

fn fixture(root: &Path) {
    for platform in ["macos", "windows", "linux"] {
        let folder = root.join(platform);
        fs::create_dir_all(&folder).unwrap();
        let artifact = folder.join("application.bin");
        fs::write(&artifact, b"fixture download").unwrap();
        fs::write(sidecar(&artifact), "verified during packaging").unwrap();
        let receipt = folder.join(format!("Xylo-{platform}.json"));
        fs::write(&receipt, serde_json::to_vec(&json!({"version": "0.1.1-rc.1", "commit": "fixture",
            "platform": platform, "artifact_signatures": "verified",
            "files": [{"name": "application.bin", "sha256": sha256(&artifact).unwrap(), "size": artifact.metadata().unwrap().len()}]
        })).unwrap()).unwrap();
        fs::write(sidecar(&receipt), "structural test signature placeholder").unwrap();
        let checksum = receipt.with_extension("sha256");
        fs::write(
            &checksum,
            format!("{}  application.bin\n", sha256(&artifact).unwrap()),
        )
        .unwrap();
        fs::write(sidecar(&checksum), "structural test signature placeholder").unwrap();
        fs::write(
            folder.join(crate::signatures::PUBLIC_FILENAME),
            crate::signatures::PINNED_PUBLIC,
        )
        .unwrap();
    }
}

// These are structural tests, not cryptographic tests. Only this test helper
// substitutes the verifier, at the exact production authentication boundary.
fn validate_receipts(
    root: &Path,
    version: &str,
    commit: &str,
    partial: bool,
) -> anyhow::Result<Vec<Value>> {
    validate_with_verifier(root, version, commit, partial, |_| Ok(()))
        .map(|authenticated| authenticated.receipts)
}

fn validate(root: &Path) -> anyhow::Result<Vec<Value>> {
    validate_receipts(root, "0.1.1-rc.1", "fixture", false)
}

fn change_receipt(root: &Path, change: impl FnOnce(&mut Value)) {
    let path = root.join("macos/Xylo-macos.json");
    let mut receipt = read_json(&path).unwrap();
    change(&mut receipt);
    fs::write(path, serde_json::to_vec(&receipt).unwrap()).unwrap();
}

#[test]
fn all_native_platforms_and_matching_contents_are_required() {
    let temporary = tempfile::tempdir().unwrap();
    fixture(temporary.path());
    assert_eq!(validate(temporary.path()).unwrap().len(), 3);
    fs::remove_file(temporary.path().join("windows/Xylo-windows.json")).unwrap();
    assert!(
        validate(temporary.path())
            .unwrap_err()
            .to_string()
            .contains("three platform")
    );
}

#[test]
fn changed_artifacts_and_mixed_commits_or_versions_are_rejected() {
    let temporary = tempfile::tempdir().unwrap();
    fixture(temporary.path());
    for (version, commit) in [("0.1.1-rc.1", "other"), ("0.2.0", "fixture")] {
        assert!(
            validate_receipts(temporary.path(), version, commit, false)
                .unwrap_err()
                .to_string()
                .contains("different versions or commits")
        );
    }
    fs::write(
        temporary.path().join("macos/application.bin"),
        b"tampered download",
    )
    .unwrap();
    assert!(
        validate(temporary.path())
            .unwrap_err()
            .to_string()
            .contains("does not match")
    );
}

#[test]
fn macos_only_publication_must_be_explicit() {
    let temporary = tempfile::tempdir().unwrap();
    fixture(temporary.path());
    let mac = temporary.path().join("macos");
    assert_eq!(
        validate_receipts(&mac, "0.1.1-rc.1", "fixture", true)
            .unwrap()
            .len(),
        1
    );
    assert!(validate(&mac).is_err());
    assert!(validate_receipts(temporary.path(), "0.1.1-rc.1", "fixture", true).is_err());
}

#[test]
fn every_download_and_receipt_requires_a_signature() {
    for name in [
        "linux/application.bin.sig",
        "linux/Xylo-linux.json.sig",
        "linux/Xylo-linux.sha256.sig",
    ] {
        let temporary = tempfile::tempdir().unwrap();
        fixture(temporary.path());
        fs::remove_file(temporary.path().join(name)).unwrap();
        assert!(
            validate(temporary.path())
                .unwrap_err()
                .to_string()
                .contains("signature is missing")
        );
    }
}

#[test]
fn duplicate_platform_unverified_receipt_and_traversal_are_rejected() {
    for (field, value) in [
        ("platform", json!("linux")),
        ("artifact_signatures", json!("unsigned")),
    ] {
        let temporary = tempfile::tempdir().unwrap();
        fixture(temporary.path());
        change_receipt(temporary.path(), |r| r[field] = value);
        assert!(validate(temporary.path()).is_err());
    }
    for name in [
        "../application.bin",
        "..\\application.bin",
        "/absolute",
        "C:relative",
        ".",
        "..",
        "",
    ] {
        let temporary = tempfile::tempdir().unwrap();
        fixture(temporary.path());
        change_receipt(temporary.path(), |r| r["files"][0]["name"] = json!(name));
        assert!(
            validate(temporary.path())
                .unwrap_err()
                .to_string()
                .contains("filename")
        );
    }
}

#[test]
fn platform_architecture_labels_are_consistent() {
    assert_eq!(
        platform_name("Windows", "AMD64").unwrap(),
        ("windows", "x64")
    );
    assert_eq!(platform_name("Linux", "x86_64").unwrap(), ("linux", "x64"));
    assert_eq!(
        platform_name("Darwin", "arm64").unwrap(),
        ("macos", "arm64")
    );
    assert_eq!(
        platform_name("macos", "aarch64").unwrap(),
        ("macos", "arm64")
    );
    assert!(platform_name("linux", "riscv64").is_err());
}

#[test]
fn failed_ci_blocks_and_successful_ci_allows_full_publication() {
    for conclusion in ["failure", "cancelled", "skipped", "timed_out"] {
        assert!(ci_passed(&json!([{"status": "completed", "conclusion": conclusion, "url": "https://example.invalid/ci"}]))
            .unwrap_err().to_string().contains("CI must pass"));
    }
    assert_eq!(ci_passed(&json!([{"status": "completed", "conclusion": "success", "url": "https://example.invalid/ci"}])).unwrap(), Some(()));
    assert_eq!(ci_passed(&json!([])).unwrap(), None);
    assert_eq!(
        ci_passed(&json!([{"status": "in_progress"}])).unwrap(),
        None
    );
}

#[test]
fn stable_releases_and_complete_notes_cannot_be_replaced() {
    assert!(
        validate_existing(
            &json!({"isDraft": false, "isPrerelease": false, "assets": []}),
            false
        )
        .is_err()
    );
    let current = json!({"isDraft": false, "isPrerelease": true, "assets": [{"name": "Xylo-1-linux-x64.deb"}]});
    assert!(validate_existing(&current, true).is_err());
    assert!(
        validate_existing(&current, false)
            .unwrap()
            .contains("Xylo-1-linux-x64.deb")
    );
}

#[test]
fn authenticated_allowlist_covers_only_expected_files_and_verifies_every_payload() {
    use std::cell::RefCell;
    let temporary = tempfile::tempdir().unwrap();
    fixture(temporary.path());
    let verified = RefCell::new(std::collections::BTreeSet::new());
    let authenticated =
        validate_with_verifier(temporary.path(), "0.1.1-rc.1", "fixture", false, |p| {
            verified.borrow_mut().insert(p.to_owned());
            Ok(())
        })
        .unwrap();
    assert_eq!(authenticated.files.len(), 21); // 3 * (3 signed files + 3 signatures + key)
    assert_eq!(verified.borrow().len(), 9);
    for file in &authenticated.files {
        if file.extension().is_some_and(|e| e == "sig") {
            continue;
        }
        if file.file_name().unwrap() != crate::signatures::PUBLIC_FILENAME {
            assert!(verified.borrow().contains(file));
        }
    }
    // A mere 'verified' assertion and signature presence never pass production.
    assert!(
        crate::publish::validate_receipts(temporary.path(), "0.1.1-rc.1", "fixture", false)
            .is_err()
    );
    for name in [
        "extra.exe",
        "extra.exe.sig",
        "nested/extra",
        "Xylo-macos.sha256.sig.extra",
    ] {
        let extra = temporary.path().join("macos").join(name);
        fs::create_dir_all(extra.parent().unwrap()).unwrap();
        fs::write(&extra, b"unexpected").unwrap();
        assert!(
            validate(temporary.path())
                .unwrap_err()
                .to_string()
                .contains("Unexpected")
        );
        fs::remove_file(extra).unwrap();
    }
}

#[test]
fn authentication_failure_stops_before_receipt_parsing_and_covers_artifacts_and_checksums() {
    let temporary = tempfile::tempdir().unwrap();
    fixture(temporary.path());
    for suffix in [".json", ".bin", ".sha256"] {
        let error = validate_with_verifier(temporary.path(), "0.1.1-rc.1", "fixture", false, |p| {
            anyhow::ensure!(
                !p.to_string_lossy().ends_with(suffix),
                "mock authentication rejected"
            );
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("mock authentication rejected"));
    }
    fs::write(temporary.path().join("linux/Xylo-linux.json"), b"not JSON").unwrap();
    let error = validate_with_verifier(temporary.path(), "0.1.1-rc.1", "fixture", false, |_| {
        anyhow::bail!("authentication before parsing")
    })
    .unwrap_err();
    assert!(error.to_string().contains("authentication before parsing"));
}

#[test]
fn substituted_keys_and_inconsistent_checksums_are_rejected() {
    for name in ["Xylo-release-signing.pub", "Xylo-macos.sha256"] {
        let temporary = tempfile::tempdir().unwrap();
        fixture(temporary.path());
        fs::write(temporary.path().join("macos").join(name), b"substituted").unwrap();
        assert!(validate(temporary.path()).is_err());
    }
}

#[test]
fn download_symlinks_are_rejected_even_when_pointing_to_valid_files_or_directories() {
    let temporary = tempfile::tempdir().unwrap();
    fixture(temporary.path());
    let link = temporary.path().join("unexpected-link");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(temporary.path().join("macos/application.bin"), &link).unwrap();
        assert!(
            validate(temporary.path())
                .unwrap_err()
                .to_string()
                .contains("symlinks")
        );
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(temporary.path().join("macos"), &link).unwrap();
        assert!(
            validate(temporary.path())
                .unwrap_err()
                .to_string()
                .contains("symlinks")
        );
    }
    #[cfg(windows)]
    {
        // Junctions require no Developer Mode/admin privilege (unlike symlinks).
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(temporary.path().join("macos"))
            .output()
            .unwrap();
        assert!(status.status.success());
        assert!(
            validate(temporary.path())
                .unwrap_err()
                .to_string()
                .contains("symlinks")
        );
        fs::remove_dir(&link).unwrap();
    }
}

#[test]
fn sha256_is_standard_and_streamed() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("fixture");
    fs::write(&path, b"abc").unwrap();
    assert_eq!(
        sha256(&path).unwrap(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn release_checksums_fit_a_small_windows_stack() {
    const CHILD: &str = "XYLO_TEST_SMALL_HASH_STACK";
    if std::env::var_os(CHILD).is_some() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("fixture");
        // Span multiple chunks, so the same regression also exercises streaming.
        let bytes = vec![b'a'; 2 * 1024 * 1024 + 17];
        fs::write(&path, &bytes).unwrap();
        let expected = {
            use sha2::{Digest, Sha256};
            format!("{:x}", Sha256::digest(&bytes))
        };
        let actual = std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(move || sha256(&path).unwrap())
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(actual, expected);
        return;
    }
    // Stack overflow aborts the process rather than unwinding. Isolate that
    // failure so it reports as one failed test instead of aborting the suite.
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::release_checksums_fit_a_small_windows_stack",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "Checksum subprocess failed ({}): {}",
        child.status,
        String::from_utf8_lossy(&child.stderr)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn linker_signatures_do_not_seal_bundles_and_tampered_resources_fail() {
    use crate::assets::verify_macos_bundle;
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let app = root.join("Xylo.app");
    let executables = app.join("Contents/MacOS");
    let helper = app.join("Contents/Resources/sandbox-helper");
    fs::create_dir_all(&executables).unwrap();
    fs::create_dir_all(helper.join("node_modules")).unwrap();
    fs::write(helper.join("run.mjs"), "// original helper\n").unwrap();
    fs::write(app.join("Contents/Info.plist"), r#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict><key>CFBundleExecutable</key><string>xylo</string><key>CFBundleIdentifier</key><string>com.qurkos.xylo</string><key>CFBundlePackageType</key><string>APPL</string><key>CFBundleShortVersionString</key><string>0.0.0-test</string></dict></plist>"#).unwrap();
    let binary = root.join("fixture");
    let mut compiler = Command::new("cc")
        .args(["-x", "c", "-", "-o"])
        .arg(&binary)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    compiler
        .stdin
        .take()
        .unwrap()
        .write_all(b"int main(void) { return 0; }\n")
        .unwrap();
    assert!(compiler.wait().unwrap().success());
    for name in ["xylo", "xylo-worker", "xylo-helper", "xylo-cli"] {
        fs::copy(&binary, executables.join(name)).unwrap();
    }
    assert!(verify_macos_bundle(&app, "0.0.0-test").is_err());
    for entry in fs::read_dir(&executables).unwrap() {
        checked(
            Command::new("codesign")
                .args(["--force", "--sign", "-"])
                .arg(entry.unwrap().path()),
            "Fixture executable signing",
        )
        .unwrap();
    }
    checked(
        Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(&app),
        "Fixture bundle signing",
    )
    .unwrap();
    verify_macos_bundle(&app, "0.0.0-test").unwrap();
    fs::write(helper.join("run.mjs"), "// changed after signing\n").unwrap();
    assert!(verify_macos_bundle(&app, "0.0.0-test").is_err());
}
