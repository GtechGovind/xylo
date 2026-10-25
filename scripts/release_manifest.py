#!/usr/bin/env python3
"""Sign immutable metadata and final artifacts. Does not publish anything."""
from __future__ import annotations

import argparse
import base64
import json
import os
from pathlib import Path
import re
import sys
import time
from urllib.parse import quote, urlsplit

sys.dont_write_bytecode = True
from release_common import digest, packager, run, workspace_version, write_json


def allowed_url(url: str, policy: dict) -> None:
    parsed = urlsplit(url)
    if parsed.scheme != "https" or not parsed.netloc or parsed.username or parsed.password or parsed.fragment:
        raise ValueError("Release URLs must use credential-free HTTPS")
    origin = f"https://{parsed.hostname}" + (f":{parsed.port}" if parsed.port and parsed.port != 443 else "")
    if origin not in policy["allowed_origins"]:
        raise ValueError("Release URL origin is not in the embedded policy")


def validate_policy(path: Path) -> dict:
    policy = json.loads(path.read_text())
    if not policy.get("enabled") or not policy.get("public_key"):
        raise ValueError("A real maintainer public key and enabled release policy are required")
    public_key = base64.b64decode(policy["public_key"], validate=True).decode()
    if len(public_key.splitlines()) < 2 or not public_key.splitlines()[-1].startswith("RW"):
        raise ValueError("Public key must be base64 of the complete packager/minisign public-key file")
    for key in ("manifest_url", "signature_url"):
        allowed_url(policy[key], policy)
    if not 0 < policy["max_artifact_bytes"] <= 2 * 1024**3:
        raise ValueError("Invalid update artifact bound")
    return policy


def sign(path: Path, executable: str) -> str:
    signature = Path(str(path) + ".sig")
    if signature.exists():
        raise ValueError(f"Refusing to overwrite signature: {signature.name}")
    # Explicit environment; never pass key/password on argv or enable trace logs.
    env = os.environ.copy()
    env["RUST_LOG"] = "error"
    run([executable, "signer", "sign", path], env=env)
    return signature.read_text().strip()


def create(policy_path: Path, artifacts: list[tuple[str, str, Path]], output: Path,
           base_url: str, sequence: int, notes: str, lifetime_hours: int) -> Path:
    policy = validate_policy(policy_path)
    if not os.environ.get("CARGO_PACKAGER_SIGN_PRIVATE_KEY"):
        raise ValueError("CARGO_PACKAGER_SIGN_PRIVATE_KEY must reference the maintainer's secret key")
    if not 0 < sequence < 2**64 or not 1 <= lifetime_hours <= 31 * 24:
        raise ValueError("Sequence must be positive and metadata lifetime at most 31 days")
    if len(notes.encode()) > 65536:
        raise ValueError("Release notes exceed 64 KiB")
    version = workspace_version()
    if policy["channel"] == "stable" and "-" in version:
        raise ValueError("Stable releases cannot contain prerelease versions")
    executable = packager()
    now = int(time.time())
    document = {"schema": 1, "application_id": policy["application_id"],
                "channel": policy["channel"], "sequence": sequence, "version": version,
                "published_at": now, "expires_at": now + lifetime_hours * 3600,
                "notes": notes, "platforms": {}}
    output.mkdir(parents=True, exist_ok=True)
    manifest = output / f"release-{version}-{sequence}.json"
    if manifest.exists() or Path(str(manifest) + ".sig").exists():
        raise ValueError("Immutable release metadata already exists; use a new sequence/output directory")
    for platform, kind, path in artifacts:
        if not re.fullmatch(r"(?:macos|linux|windows)-(?:aarch64|x86_64)", platform):
            raise ValueError("Unknown artifact platform")
        expected = "app" if platform.startswith("macos-") else "appimage" if platform.startswith("linux-") else "nsis"
        if kind != expected or platform in document["platforms"]:
            raise ValueError("Duplicate platform or incompatible artifact format")
        suffix = ".app.tar.gz" if kind == "app" else ".AppImage" if kind == "appimage" else ".exe"
        if not path.name.endswith(suffix) or not path.is_file():
            raise ValueError(f"Expected a final {suffix} artifact")
        if not 0 < path.stat().st_size <= policy["max_artifact_bytes"]:
            raise ValueError("Artifact exceeds the embedded update size limit")
        url = base_url.rstrip("/") + "/" + quote(path.name)
        allowed_url(url, policy)
        document["platforms"][platform] = {"url": url, "format": kind, "signature": sign(path, executable),
                                             "size_bytes": path.stat().st_size, "sha256": digest(path)}
    if not artifacts:
        raise ValueError("At least one platform artifact is required")
    write_json(manifest, document, exclusive=True)
    sign(manifest, executable)
    # Verify with the exact application-side verifier before handing off bytes.
    for platform, _, path in artifacts:
        run(["cargo", "run", "--locked", "-p", "workbench-distribution", "--bin",
             "workbench-release-verify", "--", policy_path, platform, manifest,
             Path(str(manifest) + ".sig"), path])
    return manifest


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--policy", type=Path, required=True)
    parser.add_argument("--artifact", nargs=3, action="append", metavar=("PLATFORM", "FORMAT", "PATH"), required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--sequence", type=int, required=True)
    parser.add_argument("--notes", type=Path, required=True)
    parser.add_argument("--lifetime-hours", type=int, default=14 * 24)
    args = parser.parse_args()
    created = create(args.policy.resolve(), [(p, f, Path(a).resolve()) for p, f, a in args.artifact],
                     args.output.resolve(), args.base_url, args.sequence,
                     args.notes.read_text(), args.lifetime_hours)
    print(f"Signed, verified immutable release: {created.name}. No files were published.")
