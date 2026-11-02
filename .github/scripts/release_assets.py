"""Assemble and verify the release downloads for one native platform."""
import hashlib
import json
from pathlib import Path
import platform
import plistlib
import shutil
import subprocess
import tarfile
import tempfile


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def platform_name(system, machine):
    return {"Darwin": "macos", "Windows": "windows", "Linux": "linux"}[system], {
        "arm64": "arm64", "aarch64": "arm64", "amd64": "x64", "x86_64": "x64"}[machine.lower()]


def verify_macos_bundle(app, version):
    """Validate the sealed application, independently of download signatures."""
    info = plistlib.loads((app / "Contents/Info.plist").read_bytes())
    if info.get("CFBundleShortVersionString") != version or info.get("CFBundleIdentifier") != "com.qurkos.xylo":
        raise ValueError("macOS app has the wrong version or bundle identifier")
    for name in ("xylo", "xylo-worker", "xylo-helper", "xylo-cli"):
        binary = app / "Contents/MacOS" / name
        if not binary.is_file() or not binary.stat().st_mode & 0o111:
            raise ValueError(f"macOS app is missing executable {name}")
    helper = app / "Contents/Resources/sandbox-helper"
    if not (helper / "run.mjs").is_file() or not (helper / "node_modules").is_dir():
        raise ValueError("macOS app is missing sandbox helper dependencies")
    subprocess.run(["codesign", "--verify", "--deep", "--strict", str(app)], check=True)


def verify_macos_packages(app, dmg, archive, version, notarized):
    verify_macos_bundle(app, version)
    if notarized:
        subprocess.run(["xcrun", "stapler", "validate", str(app)], check=True)
    subprocess.run(["hdiutil", "verify", str(dmg)], check=True, stdout=subprocess.DEVNULL)
    with tempfile.TemporaryDirectory(prefix="xylo-macos-package-check-") as temporary:
        root = Path(temporary)
        extracted = root / "archive"
        extracted.mkdir()
        # Use macOS's archive reader, including on its bundled Python 3.9.
        # This is the archive created from our validated app just above.
        subprocess.run(["tar", "-xzf", str(archive.resolve()), "-C", str(extracted)], check=True)
        verify_macos_bundle(extracted / "Xylo.app", version)
        mount = root / "disk"
        mount.mkdir()
        subprocess.run(["hdiutil", "attach", str(dmg.resolve()), "-readonly", "-nobrowse",
            "-noautoopen", "-mountpoint", str(mount)], check=True, stdout=subprocess.DEVNULL)
        try:
            installed = mount / "Xylo.app"
            verify_macos_bundle(installed, version)
            reported = subprocess.check_output([str(installed / "Contents/MacOS/xylo-cli"), "--version"], text=True)
            if f"xylo-cli {version} " not in reported:
                raise ValueError("macOS package reports the wrong application version")
        finally:
            subprocess.run(["hdiutil", "detach", str(mount)], check=True, stdout=subprocess.DEVNULL)
    print("Verified macOS bundle signatures, DMG, app archive and packaged CLI startup", flush=True)


def verify_linux_packages(downloads, version):
    """Check installed files and the loader, not just the package container."""
    deb = next(path for path in downloads if path.suffix == ".deb")
    native_version = subprocess.check_output(["dpkg-deb", "--field", str(deb), "Version"], text=True).strip()
    if native_version != version.replace("-", "~", 1):
        raise ValueError("Linux package has incorrect release-candidate version ordering")
    dependencies = subprocess.check_output(["dpkg-deb", "--field", str(deb), "Depends"], text=True)
    for name in ("libc6", "bubblewrap", "socat", "ripgrep", "libvulkan1"):
        if name not in dependencies:
            raise ValueError(f"Linux package is missing its {name} dependency")
    appimage = next(path for path in downloads if path.suffix == ".AppImage")
    with tempfile.TemporaryDirectory(prefix="xylo-package-check-") as temporary:
        root = Path(temporary)
        subprocess.run(["dpkg-deb", "--extract", str(deb), str(root / "deb")], check=True)
        appimage.chmod(appimage.stat().st_mode | 0o111)
        subprocess.run([str(appimage.resolve()), "--appimage-extract"], cwd=root,
            check=True, stdout=subprocess.DEVNULL)
        for installed in (root / "deb", root / "squashfs-root"):
            for binary in ("xylo", "xylo-worker", "xylo-helper", "xylo-cli"):
                executable = installed / "usr/bin" / binary
                if not executable.is_file() or not executable.stat().st_mode & 0o111:
                    raise ValueError(f"Linux package is missing executable {binary}")
            helper = installed / "usr/lib/xylo/sandbox-helper"
            if not (helper / "run.mjs").is_file() or not (helper / "node_modules").is_dir():
                raise ValueError("Linux package is missing sandbox helper dependencies")
            reported = subprocess.check_output([str(installed / "usr/bin/xylo-cli"), "--version"], text=True)
            if f"xylo-cli {version} " not in reported:
                raise ValueError("Linux package reports the wrong application version")
    print("Verified Linux installed files, runtime dependencies and CLI startup", flush=True)


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
    if system == "Linux":
        verify_linux_packages(downloads, version)
    if system == "Darwin":
        apps = list(packages.rglob("*.app"))
        if len(apps) != 1:
            raise ValueError("Expected one macOS app bundle")
        archive = destination / (prefix + ".app.tar.gz")
        with tarfile.open(archive, "w:gz") as output:
            output.add(apps[0], arcname="Xylo.app")
        downloads.append(archive)
        verify_macos_packages(apps[0], downloads[0], archive, version, apple)
    public = Path(".github/release-signing.pub")
    verifier = Path("target/release/xylo-signature-verify" + (".exe" if system == "Windows" else ""))
    checksum = destination / (prefix + ".sha256")
    checksum.write_text("".join(f"{sha256(path)}  {path.name}\n" for path in downloads))
    receipt = destination / (prefix + ".json")
    receipt.write_text(json.dumps({"version": version, "commit": environment.get("GITHUB_SHA", "local"),
        "platform": name, "arch": arch, "artifact_signatures": "verified",
        "platform_signing": "not_applicable" if system == "Linux" else ("verified" if apple else ("ad_hoc" if system == "Darwin" else "not_configured")),
        "files": [{"name": path.name, "sha256": sha256(path), "size": path.stat().st_size} for path in downloads]}, indent=2) + "\n")
    for path in [*downloads, checksum, receipt]:
        subprocess.run(["cargo", "packager", "signer", "sign", str(path)], env=environment, check=True)
        subprocess.run([str(verifier), str(public), str(path) + ".sig", str(path)], check=True)
    shutil.copy2(public, destination / "Xylo-release-signing.pub")
    print(f"Verified release downloads: {destination}", flush=True)
