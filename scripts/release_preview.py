#!/usr/bin/env python3
"""Validate all six native results and prepare assets, optionally create a DRAFT.

Default operation is local. --create-draft is explicit, never publishes a release,
never replaces a tag/release, and requires a public destination repository.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import zipfile

sys.dont_write_bytecode = True
from release_common import BINARIES, ROOT, TARGETS, digest, workspace_version, write_json
from release_metadata import checksums, validate_inventory
from release_windows import validate_windows_imports
from release_linux_notices import read_json as read_linux_json, validate_linux_report

COMMON_CHECKS = {"all_four_native_executable_machines", "packaged_license_inventory_complete", "native_asset_hashes",
                 "all_three_packaged_version_commands", "packaged_whisper_cli_help",
                 "packaged_helper_correlated_jsonl_and_actual_docx_export",
                 "packaged_worker_ready_and_clean_stdin_shutdown", "packaged_desktop_isolated_profile_launch_alive_8s"}


def regular(root: Path, name: str) -> Path:
    path = root / name
    if Path(name).is_absolute() or ".." in Path(name).parts or not path.is_file() or path.is_symlink():
        raise ValueError("Unsafe or missing release input")
    if any(parent.is_symlink() for parent in path.parents if parent.is_relative_to(root)):
        raise ValueError("Release input traverses a symbolic link")
    return path


def validate_results(inputs: Path, commit: str, lock_sha256: str) -> list[tuple[Path, dict]]:
    if not re.fullmatch(r"[0-9a-f]{40}", commit) or not re.fullmatch(r"[0-9a-f]{64}", lock_sha256):
        raise ValueError("Expected exact source commit and Cargo.lock SHA256")
    found = {}
    for path in inputs.rglob("qualification.json"):
        if len(found) >= len(TARGETS):
            raise ValueError("Duplicate or excessive native qualifications")
        root = path.parent
        proof = json.loads(regular(root, "qualification.json").read_text())
        target = proof.get("target")
        if target not in TARGETS or target in found:
            raise ValueError("Unknown/duplicate native qualification target")
        platform_id = TARGETS[target][0]
        checks = COMMON_CHECKS | ({"app_bundle_codesign_verified"} if "apple" in target else
                                  {"nsis_silent_install_executed", "packaged_windows_crt_imports_validated"} if "windows" in target else
                                  {"appimage_native_extraction_executed", "packaged_appimage_apprun_launch_alive_8s",
                                   "packaged_debian_control_validated", "packaged_linux_distro_native_notices_validated",
                                   "packaged_linux_appimage_infrastructure_notices_validated", "packaged_linux_debian_native_notices_validated"})
        if (proof.get("schema") != 1 or proof.get("passed") is not True or proof.get("platform") != platform_id
                or proof.get("qualification") != "native_package_smoke" or not checks.issubset(proof.get("checks", []))
                or proof.get("license_inventory_complete") is not True
                or proof.get("cargo_lock_sha256") != lock_sha256
                or proof.get("source", {}).get("commit") != commit or proof["source"].get("dirty") is not False):
            raise ValueError("Native qualification is incomplete or belongs to another source")
        status = json.loads(regular(root, "release-status.json").read_text())
        plan = json.loads(regular(root, "build-plan.json").read_text())
        if plan.get("source") != proof["source"] or plan.get("target") != target or plan.get("mode") != "preview":
            raise ValueError("Package plan does not match the qualified source/target")
        if (status.get("mode") != "preview" or status.get("license_inventory_complete") is not True
                or status.get("updater_feed_enabled") is not False or status.get("published") is not False
                or status.get("os_signed") is not False):
            raise ValueError("Expected a complete unsigned preview with disabled updater")
        metadata = root / "release-metadata"
        provenance = json.loads(regular(metadata, "build-provenance.json").read_text())
        if (provenance.get("commit") != commit or provenance.get("dirty") is not False
                or provenance.get("target") != target or provenance.get("cargo_lock_sha256") != lock_sha256):
            raise ValueError("Dependency provenance differs from its qualified source")
        validate_inventory(metadata, strict=True)
        inventory = json.loads(regular(metadata, "licenses.json").read_text())
        if not inventory.get("packages"):
            raise ValueError("Release inventory must cover actual delivered dependencies")
        sbom = json.loads(regular(metadata, "sbom.cdx.json").read_text())
        if sbom.get("bomFormat") != "CycloneDX" or not sbom.get("components"):
            raise ValueError("Missing dependency SBOM")
        binaries = proof.get("binaries", [])
        architecture = "aarch64" if target.startswith("aarch64") else "x86_64"
        if len(binaries) != 4 or {item.get("name") for item in binaries} != {*BINARIES, "whisper-cli"}:
            raise ValueError("Native executable inspection is incomplete")
        for item in binaries:
            if (item.get("architecture") != architecture or not re.fullmatch(r"[0-9a-f]{64}", item.get("sha256", ""))
                    or not isinstance(item.get("size_bytes"), int) or item["size_bytes"] < 1):
                raise ValueError("Invalid native executable identity")
        if "windows" in target:
            validate_windows_imports(regular(metadata, "windows-pe-imports.json"), target, binaries)
        if "linux" in target:
            report_path = regular(metadata, "linux-native-validation.json")
            if digest(report_path) != proof.get("linux_native_notices_report_sha256"):
                raise ValueError("Linux native notice proof hash differs from its qualification")
            validate_linux_report(read_linux_json(report_path), metadata, target, binaries)
        expected = {".dmg", ".app.tar.gz"} if "apple" in target else {".exe"} if "windows" in target else {".AppImage", ".deb"}
        seen = set()
        for artifact in proof.get("artifacts", []):
            name = artifact["name"]
            extension = next((suffix for suffix in expected if name.endswith(suffix)), None)
            file = regular(root, name)
            if (extension is None or extension in seen or artifact["sha256"] != digest(file)
                    or artifact["size_bytes"] != file.stat().st_size or not 0 < file.stat().st_size <= 2 * 1024**3):
                raise ValueError("Native artifact format/size/hash mismatch")
            seen.add(extension)
        if seen != expected:
            raise ValueError("Missing required native installer artifacts")
        if "linux" in target:
            notice_report = read_linux_json(metadata / "linux-native-validation.json")
            image = notice_report["appimage"]
            if image not in proof.get("artifacts", []):
                raise ValueError("Linux notice proof belongs to a different AppImage artifact")
            debian = {key: notice_report["debian"][key] for key in ("name", "sha256", "size_bytes")}
            if debian not in proof.get("artifacts", []):
                raise ValueError("Linux notice proof belongs to a different Debian artifact")
        found[target] = (root, proof)
    if set(found) != set(TARGETS):
        raise ValueError("All six native targets must pass before release preparation")
    return [found[target] for target in sorted(TARGETS)]


def prepare(inputs: Path, output: Path, commit: str, version: str) -> dict:
    if version != workspace_version():
        raise ValueError("Release tag version must equal the committed workspace version")
    results = validate_results(inputs, commit, digest(ROOT / "Cargo.lock"))
    # All validation above precedes any output mutation.
    if output.exists():
        raise ValueError("Use a fresh immutable release staging directory")
    output.mkdir(parents=True)
    platforms = []
    for root, proof in results:
        platform_id = proof["platform"]
        prefix = f"Xylo-{version}-{platform_id}"
        artifacts = []
        for artifact in proof["artifacts"]:
            extension = ".app.tar.gz" if artifact["name"].endswith(".app.tar.gz") else Path(artifact["name"]).suffix
            name = prefix + extension
            shutil.copyfile(root / artifact["name"], output / name)
            artifacts.append({**artifact, "name": name})
        metadata_name = prefix + "-metadata.zip"
        with zipfile.ZipFile(output / metadata_name, "x", compression=zipfile.ZIP_DEFLATED) as archive:
            files = [root / "qualification.json", root / "release-status.json", root / "build-plan.json"]
            files += sorted((root / "release-metadata").rglob("*"))
            files += sorted((root / "smoke-logs").glob("*.log"))
            count, total = 0, 0
            for path in files:
                if path.is_dir():
                    continue
                file = regular(root, path.relative_to(root).as_posix())
                count += 1
                total += file.stat().st_size
                if count > 100_000 or total > 512 * 1024**2:
                    raise ValueError("Metadata archive exceeds release bounds")
                archive.write(file, file.relative_to(root).as_posix())
        platforms.append({"target": proof["target"], "platform": platform_id, "artifacts": artifacts,
                          "metadata": {"name": metadata_name, "sha256": digest(output / metadata_name)}})
    manifest = {"schema": 1, "product": "Xylo", "version": version, "tag": "v" + version,
                "source_commit": commit, "cargo_lock_sha256": digest(ROOT / "Cargo.lock"),
                "prerelease": True, "draft": True, "signed": False, "updater_enabled": False,
                "qualification": "six_native_package_smokes", "platforms": platforms}
    write_json(output / "release.json", manifest, exclusive=True)
    notes = (f"# Xylo {version} preview\n\nNative preview executables for macOS, Windows and Linux on ARM64 and x64.\n\n"
             "**Unsigned preview.** macOS is ad hoc signed, not notarized; Windows has no Authenticode certificate. "
             "Operating system trust prompts may appear. The built-in updater is disabled.\n\n"
             "All six native CI jobs must pass package installation/extraction, native executable architecture, "
             "worker/helper protocol, document export and isolated desktop launch smoke checks before this draft is created. "
             "These checks do not establish full visual, accessibility, microphone, GPU, live-account or provider acceptance. "
             "Desktop profile creation and liveness do not establish full Host initialization success. "
             "Linux tests launch the extracted AppImage through AppRun; FUSE mounting and Debian installation are not exercised.\n\n"
             "Model weights are not included. Choose/install compatible models explicitly in the app. "
             "The portable x64 speech runtime currently favors a baseline CPU build over AVX optimization; speech performance is unqualified. "
             "The current bundled ONNX runtime setup is unavailable on macOS Intel; full-text memory search remains available.\n\n"
             "Each platform has a metadata ZIP with complete dependency license texts, SBOM, source/SDK provenance and native smoke results. "
             f"Source commit: `{commit}`. Verify downloads with `SHA256SUMS`.\n")
    (output / "RELEASE_NOTES.md").write_text(notes, encoding="utf-8")
    checksums(output)
    return manifest


def create_draft(output: Path, manifest: dict, repository: str):
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ValueError("Use an exact GitHub owner/repository")
    result = subprocess.run(["gh", "repo", "view", repository, "--json", "visibility,nameWithOwner"],
                            text=True, capture_output=True, check=True, timeout=60)
    identity = json.loads(result.stdout)
    if identity.get("visibility") != "PUBLIC" or identity.get("nameWithOwner", "").lower() != repository.lower():
        raise ValueError("This preview workflow only publishes artifacts for public repositories")
    exists = subprocess.run(["gh", "release", "view", manifest["tag"], "--repo", repository],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=60)
    if exists.returncode == 0:
        raise ValueError("Refusing to replace an existing release or draft")
    # An existing tag must already point at this exact commit; never move tags.
    tag = subprocess.run(["gh", "api", f"repos/{repository}/git/matching-refs/tags/{manifest['tag']}"],
                         text=True, capture_output=True, check=True, timeout=60)
    matches = [entry for entry in json.loads(tag.stdout) if entry.get("ref") == "refs/tags/" + manifest["tag"]]
    if matches:
        item = matches[0]["object"]
        if item.get("type") != "commit" or item.get("sha") != manifest["source_commit"]:
            raise ValueError("Existing tag is not a lightweight tag at the qualified commit")
    subprocess.run(["gh", "release", "create", manifest["tag"], "--repo", repository,
                    "--draft", "--prerelease", "--target", manifest["source_commit"],
                    "--title", f"Xylo {manifest['version']} preview", "--notes-file", str(output / "RELEASE_NOTES.md"),
                    *[str(path) for path in sorted(output.iterdir()) if path.is_file()]], check=True, timeout=600)
    print("Created an unpublished prerelease draft. Publishing remains a separate maintainer action.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inputs", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY"))
    parser.add_argument("--create-draft", action="store_true")
    args = parser.parse_args()
    if args.tag != "v" + workspace_version():
        raise ValueError("Tag must match the current workspace version")
    manifest = prepare(args.inputs.resolve(), args.output.resolve(), args.commit, args.tag[1:])
    if args.create_draft:
        if not args.repository:
            parser.error("--create-draft requires --repository or GITHUB_REPOSITORY")
        create_draft(args.output.resolve(), manifest, args.repository)
    else:
        print("Prepared all six native targets locally. No release was created or published.")


if __name__ == "__main__":
    main()
