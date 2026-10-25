#!/usr/bin/env python3
"""Build native installers locally. No upload, publication, or account setup.

Production mode requires existing maintainer credentials and invokes native
signing/notarization tools. Preview mode additionally requires clean committed
source and complete license texts, while keeping OS trust and updates disabled.
Development mode permits incomplete local inventories. --plan does not compile.
"""
from __future__ import annotations

import argparse
import gzip
import json
import os
from pathlib import Path
import platform
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile
import uuid

sys.dont_write_bytecode = True
from release_common import (BINARIES, ROOT, TARGETS, build_env, digest, packager,
                            run, source_identity, workspace_version, write_json)
from release_manifest import validate_policy
from release_metadata import checksums, generate, validate_inventory
from release_windows import audit_windows_imports


def require(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise ValueError(f"Required release configuration is missing: {name}")
    return value


def archive_bundle(bundle: Path, destination: Path, epoch: int) -> None:
    """Stable paths/order/ownership/mtime; preserve actual signed file bytes."""
    if destination.exists():
        raise ValueError("Refusing to overwrite an update archive")
    with destination.open("xb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", filename="", mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
            for path in [bundle, *sorted(bundle.rglob("*"))]:
                info = archive.gettarinfo(str(path), arcname=(Path(bundle.name) / path.relative_to(bundle)).as_posix())
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                info.mtime = epoch
                if info.isfile():
                    with path.open("rb") as stream:
                        archive.addfile(info, stream)
                else:
                    archive.addfile(info)


def rehash_native_assets(root: Path) -> None:
    """Codesign changes native payload bytes; bind those FINAL bytes in manifests."""
    if not root.exists():
        return
    for path in sorted(root.glob("*/manifest.json")):
        manifest = json.loads(path.read_text())
        for entry in manifest["files"]:
            relative = Path(entry["path"])
            if relative.is_absolute() or ".." in relative.parts:
                raise ValueError("Unsafe bundled native asset path")
            file = path.parent / relative
            if not file.is_file() or file.is_symlink():
                raise ValueError("Bundled native asset must be a regular file")
            entry.update(size_bytes=file.stat().st_size, sha256=digest(file))
        write_json(path, manifest)


def sign_macos(bundle: Path, policy: dict) -> None:
    identity = require("WORKBENCH_MACOS_SIGNING_IDENTITY")
    team = policy.get("macos_team_id", "")
    if len(team) != 10 or not team.isalnum() or not team.isupper():
        raise ValueError("Production policy must pin the Apple Team ID")
    profile = require("WORKBENCH_NOTARY_KEYCHAIN_PROFILE")
    # Sign actual Mach-O files first, then enclosing bundles. No --deep signing.
    magic = {b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf", b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca"}
    files = sorted((p for p in bundle.rglob("*") if p.is_file() and not p.is_symlink()),
                   key=lambda p: (-len(p.parts), str(p)))
    for path in files:
        with path.open("rb") as stream:
            is_macho = stream.read(4) in magic
        if is_macho:
            command = ["codesign", "--force", "--timestamp", "--options", "runtime", "--sign", identity]
            if path.name == "agent-workbench":
                command += ["--entitlements", ROOT / "packaging/entitlements.plist"]
            run([*command, path])
    rehash_native_assets(bundle / "Contents/Resources/native-assets")
    for framework in sorted(bundle.rglob("*.framework"), key=lambda p: -len(p.parts)):
        run(["codesign", "--force", "--timestamp", "--options", "runtime", "--sign", identity, framework])
    run(["codesign", "--force", "--timestamp", "--options", "runtime", "--sign", identity,
         "--entitlements", ROOT / "packaging/entitlements.plist", bundle])
    run(["codesign", "--verify", "--deep", "--strict", bundle])
    # codesign emits details on stderr, so use the designated requirement directly.
    run(["codesign", "--verify", "--strict", "-R", f'anchor apple generic and certificate leaf[subject.OU] = "{team}"', bundle])
    notary_zip = bundle.parent / "notarization-upload.zip"
    run(["ditto", "-c", "-k", "--keepParent", bundle, notary_zip])
    try:
        run(["xcrun", "notarytool", "submit", notary_zip, "--keychain-profile", profile, "--wait"])
        run(["xcrun", "stapler", "staple", bundle])
        run(["xcrun", "stapler", "validate", bundle])
        run(["spctl", "--assess", "--type", "execute", bundle])
    finally:
        notary_zip.unlink(missing_ok=True)


def configuration(target: str, output: Path, native: Path, notices: Path, *, production: bool) -> dict:
    config = json.loads((ROOT / "packager.json").read_text())
    # The pinned CLI's name autodetection changes directory to the config FILE
    # when name is absent. Supply the stable Cargo identifier explicitly.
    config.update(name="agent-workbench", version=workspace_version(),
                  binariesDir=str(ROOT / "target" / target / "release"),
                  outDir=str(output), targetTriple=target, licenseFile=str(ROOT / "LICENSE"))
    config["icons"] = [str(ROOT / "packaging/icons/app.png"), str(ROOT / "packaging/icons/app.ico"),
                       str(ROOT / "packaging/icons/app.icns")]
    for resource in config["resources"]:
        resource["src"] = str(ROOT / resource["src"])
    config["resources"].append({"src": str(notices), "target": "licenses/dependencies"})
    if "apple" in target:
        config["formats"] = ["app"]
        config["macos"] = {"minimumSystemVersion": "15.0", "infoPlistPath": str(ROOT / "packaging/info.plist")}
        config["resources"].append({"src": str(native), "target": "native-assets"})
    elif "windows" in target:
        config["formats"] = ["nsis"]
        config["resources"].append({"src": str(native), "target": "native-assets"})
        config["nsis"] = {"installMode": "currentUser"}
        config["windows"] = {"digestAlgorithm": "sha256", "allowDowngrades": False}
        if production:
            # Signing is owned below: packager otherwise signs the main PE
            # again after the import audit, invalidating its final input hash.
            require("WORKBENCH_WINDOWS_CERTIFICATE_THUMBPRINT")
            require("WORKBENCH_WINDOWS_TIMESTAMP_URL")
    else:
        config["formats"] = ["appimage", "deb"]
        # The native helper resolves adjacent assets; do not leave them in usr/lib.
        config["appimage"] = {
            "files": {str(native): "usr/bin/native-assets"},
            # 0.11.8 renders bins directly into `cp <entry> usr/bin` without
            # shell quoting. A separate top-level dependency seed lets
            # linuxdeploy scan Whisper while preserving the nested asset bytes
            # and manifest. Quote once here for that exact pinned template.
            "bins": [shlex.quote((native / "whisper-runtime/runtime/whisper-cli").as_posix())],
        }
        config["deb"] = {"files": {str(native): "usr/bin/native-assets"},
                          "depends": ["libasound2t64", "libfontconfig1", "libxkbcommon-x11-0", "libvulkan1", "libssl3t64",
                                      "libpipewire-0.3-0", "libgbm1", "libwayland-client0", "libx11-xcb1"]}
    return config


def preflight_configuration(executable: str, config: dict) -> None:
    """Parse the unchanged app config, selecting only an inert sibling config.

    In 0.11.8, --packages filters automatic discovery, not --config inputs.
    Deserialize [app, sentinel] together, then select only the empty-formats
    sentinel. An invalid app makes the entire array fail parsing and exit
    nonzero; a valid array reaches the sentinel's pre-I/O empty-formats return.
    """
    selector = "xylo-parser-sentinel-" + uuid.uuid4().hex
    if config.get("name") == selector:
        raise ValueError("Configuration must not match the parser-only sentinel")

    output = Path(config["outDir"])

    def output_state():
        try:
            info = output.lstat()
        except FileNotFoundError:
            return None
        return (info.st_dev, info.st_ino, info.st_mode, info.st_size, info.st_mtime_ns, info.st_ctime_ns)

    before = output_state()
    environment = os.environ.copy()
    environment.pop("CARGO_PACKAGER_SIGN_PRIVATE_KEY", None)
    environment.pop("CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD", None)
    with tempfile.TemporaryDirectory(prefix="xylo-packager-config-") as directory:
        root = Path(directory)
        path = root / "packager.json"
        # The original config is serialized unchanged, including its formats and
        # nested platform fields. No hooks, binaries, or resources on the sentinel.
        sentinel = {"name": selector, "formats": []}
        write_json(path, [config, sentinel])
        result = subprocess.run([str(Path(executable).resolve()) if Path(executable).is_file() else executable,
                                 "--packages", selector],
                                cwd=root, env=environment, text=True, capture_output=True, timeout=30)
        diagnostic = (result.stdout + "\n" + result.stderr).strip()
        if sorted(root.iterdir()) != [path] or output_state() != before:
            raise ValueError("Pinned packager preflight changed output files; no installer is accepted")
        if result.returncode != 0:
            raise ValueError("Pinned packager configuration preflight failed; no installer is accepted. "
                             + diagnostic[:2048])


def linux_dependencies(binaries: Path, native: Path, configured: list[str]) -> list[str]:
    """Bind Debian requirements to this build's ELF linkage, plus dlopen backends."""
    with tempfile.TemporaryDirectory(prefix="xylo-shlibdeps-") as directory:
        work = Path(directory)
        (work / "debian").mkdir()
        (work / "debian/control").write_text("Source: xylo\n\nPackage: xylo\nArchitecture: any\nDescription: Xylo native preview\n")
        inputs = [binaries / name for name in BINARIES]
        inputs.append(native / "whisper-runtime/runtime/whisper-cli")
        result = subprocess.run(["dpkg-shlibdeps", "-O", *["-e" + str(path.resolve()) for path in inputs]],
                                cwd=work, text=True, capture_output=True, check=True, timeout=120)
        records = [line.removeprefix("shlibs:Depends=") for line in result.stdout.splitlines() if line.startswith("shlibs:Depends=")]
        if len(records) != 1 or not records[0].strip():
            raise ValueError("Could not determine Debian dependencies from the actual native executables")
        return sorted(set(configured) | {entry.strip() for entry in records[0].split(",") if entry.strip()})


def sign_windows_files(inputs: list[Path]) -> None:
    thumbprint = require("WORKBENCH_WINDOWS_CERTIFICATE_THUMBPRINT")
    timestamp = require("WORKBENCH_WINDOWS_TIMESTAMP_URL")
    if not timestamp.startswith("https://"):
        raise ValueError("The Windows timestamp service must use HTTPS")
    for path in inputs:
        run(["signtool", "sign", "/fd", "SHA256", "/sha1", thumbprint,
             "/tr", timestamp, "/td", "SHA256", path])
        run(["signtool", "verify", "/pa", "/all", path])


def sign_windows(binaries: Path, native: Path) -> None:
    inputs = [binaries / f"{name}.exe" for name in BINARIES]
    inputs += sorted(path for path in native.rglob("*") if path.suffix.lower() in (".exe", ".dll"))
    sign_windows_files(inputs)
    rehash_native_assets(native)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGETS, required=True)
    parser.add_argument("--mode", choices=("development", "preview", "production"), default="development")
    parser.add_argument("--policy", type=Path, default=ROOT / "packaging/update-policy.json")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--native-assets", type=Path)
    inspection = parser.add_mutually_exclusive_group()
    inspection.add_argument("--plan", action="store_true", help="Print the plan without compiling or signing")
    inspection.add_argument("--preflight", action="store_true", help="Parse config with the pinned CLI; do not compile or package")
    args = parser.parse_args()
    production = args.mode == "production"
    strict = args.mode != "development"
    identity = source_identity()
    if strict and (identity["dirty"] or not identity["commit"]):
        raise ValueError("Preview/production packaging requires committed, clean source")
    if production:
        if identity["dirty"] or not identity["commit"]:
            raise ValueError("Production requires a committed, clean source tree and a fresh locked build")
        policy = validate_policy(args.policy)
        if not (args.plan or args.preflight):
            require("CARGO_PACKAGER_SIGN_PRIVATE_KEY")
    else:
        policy = json.loads(args.policy.read_text())
        if policy.get("enabled"):
            raise ValueError("Development/preview packages must embed a disabled update policy")
    output = (args.output or ROOT / "dist" / f"{workspace_version()}-{args.target}-{args.mode}").resolve()
    if output.exists() and not (args.plan or args.preflight):
        raise ValueError("Use a fresh output directory; release outputs are immutable")
    native = (args.native_assets or ROOT / "target" / args.target / "release/native-assets").resolve()
    notices = output / "release-metadata"
    config = configuration(args.target, output, native, notices, production=production)
    plan = {"target": args.target, "mode": args.mode, "publishable": False,
            "source": identity, "packager": config, "policy_enabled": policy["enabled"],
            "update_policy_sha256": digest(args.policy)}
    if args.plan:
        print(json.dumps(plan, indent=2, sort_keys=True))
        return
    if args.preflight:
        preflight_configuration(packager(), config)
        print(f"Pinned packager parsed the resolved {args.target} configuration; no compilation or packaging was requested.")
        return
    expected_os = "Darwin" if "apple" in args.target else "Windows" if "windows" in args.target else "Linux"
    if platform.system() != expected_os:
        raise ValueError("Build/package on the target's own OS; cross-platform packaging is not qualified")
    executable = packager()
    if not (native / "whisper-runtime/manifest.json").is_file():
        raise ValueError("Build the pinned native speech runtime into --native-assets before packaging")
    output.mkdir(parents=True)
    write_json(output / "build-plan.json", plan)
    packages = [part for binary in BINARIES for part in ("-p", binary)]
    run(["cargo", "build", "--locked", "--release", "--target", args.target, *packages],
        env=build_env(identity["source_date_epoch"], args.policy, args.target))
    generate(args.target, notices, strict=strict)
    if "linux" in args.target:
        config["deb"]["depends"] = linux_dependencies(Path(config["binariesDir"]), native, config["deb"]["depends"])
    for file in (ROOT / "LICENSE", ROOT / "NOTICE"):
        shutil.copyfile(file, notices / file.name)
    config_path = output / "packager.resolved.json"
    write_json(config_path, config)
    # Package first; update signatures must cover final OS-signed/stapled bytes.
    package_env = os.environ.copy()
    package_env.pop("CARGO_PACKAGER_SIGN_PRIVATE_KEY", None)
    package_env.pop("CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD", None)
    if production and "windows" in args.target:
        sign_windows(Path(config["binariesDir"]), native)
    if "windows" in args.target:
        # Inspect the final inputs, including any Authenticode changes, before
        # NSIS copies them into the installer. The proof ships with the notices.
        audit_windows_imports(Path(config["binariesDir"]), native,
                              notices / "windows-pe-imports.json", args.target)
    run([executable, "--config", config_path], env=package_env)
    if "apple" in args.target:
        bundles = list(output.glob("*.app"))
        if len(bundles) != 1:
            raise ValueError("Expected exactly one cargo-packager app bundle")
        bundle = bundles[0]
        if not (bundle / "Contents/Resources/native-assets/whisper-runtime/manifest.json").is_file():
            raise ValueError("Invalid native resource bundle layout")
        validate_inventory(bundle / "Contents/Resources/licenses/dependencies", strict=strict)
        if production:
            sign_macos(bundle, policy)
        else:
            run(["codesign", "--force", "--deep", "--sign", "-", bundle])
            rehash_native_assets(bundle / "Contents/Resources/native-assets")
            run(["codesign", "--force", "--sign", "-", bundle])
        archive_bundle(bundle, output / f"Xylo-{workspace_version()}-{TARGETS[args.target][0]}.app.tar.gz",
                       identity["source_date_epoch"])
        # DMG is built AFTER stapling; rebuilding with packager could mutate .app.
        dmg = output / f"Xylo-{workspace_version()}-{TARGETS[args.target][0]}.dmg"
        run(["hdiutil", "create", "-volname", "Xylo", "-srcfolder", bundle,
             "-format", "UDZO", "-ov", dmg])
        if production:
            run(["codesign", "--timestamp", "--sign", require("WORKBENCH_MACOS_SIGNING_IDENTITY"), dmg])
            run(["xcrun", "notarytool", "submit", dmg, "--keychain-profile",
                 require("WORKBENCH_NOTARY_KEYCHAIN_PROFILE"), "--wait"])
            run(["xcrun", "stapler", "staple", dmg])
            run(["xcrun", "stapler", "validate", dmg])
    if production and "windows" in args.target:
        installers = list(output.glob("*.exe"))
        if len(installers) != 1 or installers[0].is_symlink() or not installers[0].is_file():
            raise ValueError("Expected exactly one finished NSIS installer for signing")
        sign_windows_files(installers)
    validate_inventory(notices, strict=strict)
    write_json(output / "release-status.json", {"mode": args.mode, "license_inventory_complete": strict,
               "os_signed": production and expected_os in ("Darwin", "Windows"),
               "updater_feed_enabled": policy["enabled"], "update_manifest_created": False,
               "published": False, "clean_machine_qualified": False})
    checksums(output)
    print(f"Created {args.mode} installers in {output}. No files were published.")


if __name__ == "__main__":
    main()
