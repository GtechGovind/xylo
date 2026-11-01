"""Assemble and verify the release downloads for one native platform."""
import hashlib
import json
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def platform_name(system, machine):
    return {"Darwin": "macos", "Windows": "windows", "Linux": "linux"}[system], {
        "arm64": "arm64", "aarch64": "arm64", "amd64": "x64", "x86_64": "x64"}[machine.lower()]


def collect_assets(config, environment, apple):
    system = platform.system()
    name, arch = platform_name(system, platform.machine())
    version = config["version"]
    packages = Path(config["outDir"])
    destination = Path("dist/release") / f"{name}-{arch}"
    if destination.exists():
        shutil.rmtree(destination)
    destination.mkdir(parents=True)
    prefix = f"Xylo-{version}-{name}-{arch}"
    extensions = {"Darwin": [".dmg"], "Windows": [".exe"], "Linux": [".deb", ".AppImage"]}[system]
    downloads = []
    for extension in extensions:
        matches = list(packages.rglob(f"*{extension}"))
        if len(matches) != 1:
            raise ValueError(f"Expected one {extension} release package, found {len(matches)}")
        output = destination / (prefix + extension)
        shutil.copy2(matches[0], output)
        downloads.append(output)
    if system == "Darwin":
        apps = list(packages.rglob("*.app"))
        if len(apps) != 1:
            raise ValueError("Expected one macOS app bundle")
        if apple:
            subprocess.run(["codesign", "--verify", "--deep", "--strict", str(apps[0])], check=True)
            subprocess.run(["xcrun", "stapler", "validate", str(apps[0])], check=True)
        archive = destination / (prefix + ".app.tar.gz")
        with tarfile.open(archive, "w:gz") as output:
            output.add(apps[0], arcname="Xylo.app")
        downloads.append(archive)
    public = Path(".github/release-signing.pub")
    verifier = Path("target/release/xylo-signature-verify" + (".exe" if system == "Windows" else ""))
    checksum = destination / (prefix + ".sha256")
    checksum.write_text("".join(f"{sha256(path)}  {path.name}\n" for path in downloads))
    receipt = destination / (prefix + ".json")
    receipt.write_text(json.dumps({"version": version, "commit": environment.get("GITHUB_SHA", "local"),
        "platform": name, "arch": arch, "artifact_signatures": "verified",
        "platform_signing": "not_applicable" if system == "Linux" else ("verified" if apple else "not_configured"),
        "files": [{"name": path.name, "sha256": sha256(path), "size": path.stat().st_size} for path in downloads]}, indent=2) + "\n")
    for path in [*downloads, checksum, receipt]:
        subprocess.run(["cargo", "packager", "signer", "sign", str(path)], env=environment, check=True)
        subprocess.run([str(verifier), str(public), str(path) + ".sig", str(path)], check=True)
    shutil.copy2(public, destination / "Xylo-release-signing.pub")
    print(f"Verified release downloads: {destination}", flush=True)
