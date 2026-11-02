#!/usr/bin/env python3
"""Publish verified RC downloads, optionally making macOS available first."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

from release_assets import sha256


def wait_for_ci(repo, commit):
    """Do not publish the complete RC until the matching main CI run passes."""
    deadline = time.monotonic() + 3600
    while time.monotonic() < deadline:
        runs = json.loads(subprocess.check_output([
            "gh", "run", "list", "--repo", repo, "--workflow", "ci.yml",
            "--commit", commit, "--branch", "main", "--limit", "1",
            "--json", "status,conclusion,url",
        ], text=True))
        if runs and runs[0]["status"] == "completed":
            if runs[0]["conclusion"] != "success":
                raise ValueError(f"CI must pass before publishing: {runs[0]['url']}")
            return
        print("Waiting for the matching main CI run to finish", flush=True)
        time.sleep(30)
    raise TimeoutError("CI has not completed; verified downloads remain in Actions artifacts")


def validate_receipts(root, version, commit, required_platforms=None):
    receipts = list(root.rglob("Xylo-*.json"))
    seen = set()
    for path in receipts:
        receipt = json.loads(path.read_text())
        if receipt["version"] != version or receipt["commit"] != commit:
            raise ValueError("Release downloads come from different versions or commits")
        if receipt["artifact_signatures"] != "verified":
            raise ValueError("An artifact signature was not verified")
        if receipt["platform"] in seen:
            raise ValueError("Duplicate platform receipt")
        seen.add(receipt["platform"])
        if not Path(str(path) + ".sig").is_file():
            raise ValueError("Release receipt signature is missing")
        for artifact in receipt["files"]:
            filename = artifact["name"]
            if Path(filename).name != filename:
                raise ValueError("Artifact must be a filename")
            artifact_path = path.parent / filename
            if artifact_path.stat().st_size != artifact["size"] or sha256(artifact_path) != artifact["sha256"]:
                raise ValueError("Release download does not match its verified receipt")
            if not Path(str(artifact_path) + ".sig").is_file():
                raise ValueError("Release download signature is missing")
    required_platforms = required_platforms or {"macos", "windows", "linux"}
    if seen != required_platforms:
        raise ValueError("All three platform builds must be present before publishing")
    return [json.loads(path.read_text()) for path in receipts]


def run():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--allow-partial', action='store_true', help='Publish the verified macOS build while native CI builds continue')
    args = parser.parse_args()
    tag = os.environ["GITHUB_REF_NAME"]
    if not tag.startswith("v") or "-rc." not in tag:
        raise SystemExit("This publisher is for release-candidate tags only")
    version = tag[1:]
    repo = os.environ["GITHUB_REPOSITORY"]
    if not args.allow_partial:
        wait_for_ci(repo, os.environ["GITHUB_SHA"])
    root = Path("dist/downloads")
    receipts = validate_receipts(root, version, os.environ["GITHUB_SHA"], {'macos'} if args.allow_partial else None)
    destination = Path("dist/publish")
    if destination.exists():
        shutil.rmtree(destination)
    destination.mkdir(parents=True, exist_ok=True)
    for path in root.rglob("*"):
        if not path.is_file():
            continue
        target = destination / path.name
        if target.exists() and sha256(path) != sha256(target):
            raise ValueError("Conflicting release asset names")
        shutil.copy2(path, target)
    platform_status = "\n".join(f"- {r['platform']} ({r['arch']}): project signature verified; platform signing: {r['platform_signing'].replace('_', ' ')}." for r in sorted(receipts, key=lambda r: r['platform']))
    availability = 'The macOS build is available now. Windows and Linux builds are still running and will be added after verification.' if args.allow_partial else 'macOS, Windows, and Linux downloads have completed native builds and signature verification.'
    notes = f"""Release candidate for testing, built from `{os.environ['GITHUB_SHA']}`.

{availability}

This release includes the native chat and composer polish, clearer file edits and helper-agent results, draft and queue recovery fixes, retained command output, and consistent chat search.

Every download has a SHA-256 checksum and a project-key signature (`.sig`). The verification public key is included as `Xylo-release-signing.pub`. These are cargo-packager/Minisign signatures; **they do not provide Apple notarization or Windows publisher trust**. This preview may display operating-system security warnings.

macOS app bundles are also sealed and checked with `codesign --verify --deep --strict`, including the copies inside the DMG and app archive. Without Developer ID credentials this is an ad-hoc signature, not Apple publisher verification. If macOS blocks this preview because the developer cannot be verified, follow [Apple's Open Anyway instructions](https://support.apple.com/102445) in System Settings → Privacy & Security. No global Gatekeeper changes are required.

{platform_status}

Available architectures: macOS Apple Silicon (`arm64`), Windows `x64`, and Linux `x64`. Linux packages target Ubuntu 24.04 and require glibc 2.39 or later; the AppImage does not bundle glibc.

The macOS `.app.tar.gz` preserves the app bundle's executable permissions. The `.dmg`, Windows installer, and Linux `.deb`/`.AppImage` are native packages. Executable tools require Node.js 22.12 or later. Linux also requires `bubblewrap`, `socat`, and `ripgrep`; the `.deb` declares these dependencies, while AppImage users install them separately. Linux host policy must allow Bubblewrap to create user and network namespaces; some Ubuntu installations require AppArmor configuration.

Artifact signatures use cargo-packager's Base64 encoding of Minisign signatures. Verification instructions and the public key are in the repository's distribution documentation.
"""
    existing = subprocess.run(["gh", "release", "view", tag, "--repo", repo, "--json", "isDraft,isPrerelease,assets"], text=True, capture_output=True)
    existing_names = set()
    with tempfile.NamedTemporaryFile(mode="w", suffix=".md", encoding="utf-8") as handle:
        handle.write(notes)
        handle.flush()
        if existing.returncode == 0:
            current = json.loads(existing.stdout)
            if not current['isDraft'] and not current['isPrerelease']:
                raise ValueError("Refusing to change a stable release")
            existing_names = {asset['name'] for asset in current['assets']}
            if args.allow_partial and any('-windows-' in name or '-linux-' in name for name in existing_names):
                raise ValueError('Cannot replace complete release notes with partial-build notes')
            subprocess.run(["gh", "release", "edit", tag, "--repo", repo, "--notes-file", handle.name], check=True)
        else:
            subprocess.run(["gh", "release", "create", tag, "--repo", repo, "--verify-tag", "--draft", "--prerelease", "--title", f"Xylo {version}", "--notes-file", handle.name], check=True)
    # Keep already published downloads immutable when later native builds arrive.
    pending = [str(path) for path in sorted(destination.iterdir()) if path.name not in existing_names]
    if pending:
        subprocess.run(["gh", "release", "upload", tag, "--repo", repo, *pending], check=True)
    subprocess.run(["gh", "release", "edit", tag, "--repo", repo, "--draft=false", "--prerelease", "--latest=false"], check=True)


if __name__ == "__main__":
    run()
