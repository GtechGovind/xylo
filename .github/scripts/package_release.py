#!/usr/bin/env python3
"""Package the checked-in manifest; secrets remain in process environment."""
import base64
import json
import os
from pathlib import Path
import platform
import secrets
import shlex
import subprocess
import tempfile

from release_assets import collect_assets


def configure(config, environment, system):
    """Return independent signing decisions and a manifest with public identity."""
    config = json.loads(json.dumps(config))
    if system == "Darwin":
        # LICENSE and NOTICE are already bundled resources. A DMG licenseFile
        # adds an interactive agreement that blocks unattended verification.
        config.pop("licenseFile", None)
    apple_names = ("APPLE_CERTIFICATE", "APPLE_SIGNING_IDENTITY", "APPLE_ID", "APPLE_PASSWORD", "APPLE_TEAM_ID")
    apple = system == "Darwin" and all(environment.get(name) for name in apple_names)
    if apple:
        config.setdefault("macos", {})["signingIdentity"] = environment["APPLE_SIGNING_IDENTITY"]
    elif system == "Darwin":
        # Linker signatures cover individual Mach-O files, not the finished
        # app's Info.plist and resources. Seal the complete bundle even when
        # a Developer ID certificate is unavailable.
        config.setdefault("macos", {})["signingIdentity"] = "-"
    else:
        config.get("macos", {}).pop("signingIdentity", None)
    return config, apple, bool(environment.get("MINISIGN_KEY"))


def security(arguments, *, check=True):
    """Keep certificate passwords and keychain passwords out of exceptions."""
    result = subprocess.run(["security", *arguments], capture_output=True)
    if check and result.returncode:
        raise SystemExit("Apple signing setup failed; check certificate and signing secrets")
    return result


def linux_dependencies(config):
    """Use Debian's ABI metadata instead of guessing shared-library versions."""
    binaries = [str((Path(config["binariesDir"]) / binary["path"]).resolve())
        for binary in config["binaries"]]
    with tempfile.TemporaryDirectory(prefix="xylo-dependencies-") as temporary:
        debian = Path(temporary) / "debian"
        debian.mkdir()
        (debian / "control").write_text(
            "Source: xylo\n\nPackage: xylo\nArchitecture: any\nDescription: Xylo desktop workspace\n")
        result = subprocess.run(["dpkg-shlibdeps", "-O", *[f"-e{path}" for path in binaries]],
            cwd=temporary, text=True, capture_output=True, check=True)
    dependencies = next((line.removeprefix("shlibs:Depends=").split(", ")
        for line in result.stdout.splitlines() if line.startswith("shlibs:Depends=")), [])
    if not dependencies or not all(dependencies):
        raise ValueError("Could not determine Linux runtime dependencies")
    # Graphics backends load these libraries dynamically; shlibdeps cannot see
    # them. Executable tools also require bubblewrap and socat on Linux.
    dependencies.extend(["libvulkan1", "libegl1", "libwayland-client0", "bubblewrap", "socat", "ripgrep"])
    dependencies.extend(config.get("deb", {}).get("depends", []))
    config.setdefault("deb", {})["depends"] = sorted(set(dependencies))
    print("Linux runtime dependencies: " + ", ".join(config["deb"]["depends"]), flush=True)


def run():
    manifest = Path("packager.json")
    config, apple, minisign = configure(json.loads(manifest.read_text()), os.environ, platform.system())
    tag = os.environ.get("GITHUB_REF", "")
    if tag.startswith("refs/tags/v") and config["version"] != tag.removeprefix("refs/tags/v"):
        raise SystemExit("Release tag must match packager.json version")
    if not minisign:
        raise SystemExit("MINISIGN_KEY is required for verifiable release downloads")
    config["outDir"] = f"dist/packages/{config['version']}/{platform.system().lower()}-{platform.machine().lower()}"
    binary_directory = Path(config["binariesDir"])
    suffix = ".exe" if platform.system() == "Windows" else ""
    for binary in config["binaries"]:
        if not (binary_directory / (binary["path"] + suffix)).is_file():
            raise SystemExit("A binary from packager.json is missing")
    if platform.system() == "Linux":
        linux_dependencies(config)
    environment = dict(os.environ)
    # Upstream trace instrumentation includes signing arguments; keep secrets out of logs.
    environment["RUST_LOG"] = "info"
    if minisign:
        environment["CARGO_PACKAGER_SIGN_PRIVATE_KEY"] = environment["MINISIGN_KEY"]
        environment["CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD"] = environment.get("MINISIGN_PASSWORD", "")
    else:
        environment.pop("CARGO_PACKAGER_SIGN_PRIVATE_KEY", None)
        environment.pop("CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD", None)
        print("::warning::MINISIGN_KEY is missing; update artifacts will be unsigned", flush=True)
    keychain = None
    original_keychains = None
    with tempfile.TemporaryDirectory(prefix="xylo-signing-") as temporary:
        if platform.system() == "Darwin":
            # ICNS needs both standard and Retina representations; a bare
            # 1024px PNG is not a valid 1x representation in cargo-packager.
            iconset = Path(temporary) / "xylo.iconset"
            iconset.mkdir()
            for size in (16, 32, 128, 256, 512):
                for scale in (1, 2):
                    filename = f"icon_{size}x{size}" + ("@2x" if scale == 2 else "") + ".png"
                    subprocess.run(["sips", "-z", str(size * scale), str(size * scale),
                        "crates/desktop/assets/xylo-icon.png", "--out", str(iconset / filename)],
                        check=True, stdout=subprocess.DEVNULL)
            icon = Path(temporary) / "xylo.icns"
            subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(icon)], check=True)
            config["icons"] = [str(icon)]
        if apple:
            certificate = Path(temporary) / "certificate.p12"
            certificate.write_bytes(base64.b64decode(environment["APPLE_CERTIFICATE"], validate=True))
            certificate.chmod(0o600)
            keychain = str(Path(temporary) / "release.keychain-db")
            password = secrets.token_urlsafe(32)
            original_keychains = shlex.split(security(["list-keychains", "-d", "user"]).stdout.decode())
            security(["create-keychain", "-p", password, keychain])
            security(["unlock-keychain", "-p", password, keychain])
            security(["import", str(certificate), "-k", keychain, "-P", environment.get("APPLE_CERTIFICATE_PASSWORD", ""), "-T", "/usr/bin/codesign"])
            security(["set-key-partition-list", "-S", "apple-tool:,apple:", "-s", "-k", password, keychain])
            security(["list-keychains", "-d", "user", "-s", *original_keychains, keychain])
            # The imported keychain supplies signing; do not ask packager to import again.
            environment.pop("APPLE_CERTIFICATE", None)
            environment.pop("APPLE_CERTIFICATE_PASSWORD", None)
        elif platform.system() == "Darwin":
            print("::warning::Using ad-hoc bundle signing; Developer ID signing and notarization are not configured", flush=True)
            for name in tuple(environment):
                if name.startswith("APPLE_"):
                    environment.pop(name)
        # Keep the generated config beside the source manifest so all resource paths resolve.
        generated = Path("packager.ci.json")
        try:
            native_config = dict(config)
            if platform.system() == "Linux":
                # Debian's '~' sorts previews before the stable version. Keep
                # the app's SemVer in the signed download names and receipt.
                native_config["version"] = config["version"].replace("-", "~", 1)
            generated.write_text(json.dumps(native_config))
            subprocess.run(["cargo", "packager", "--config", str(generated), "--formats", environment["PACKAGE_FORMATS"]], env=environment, check=True)
            collect_assets(config, environment, apple)
        finally:
            generated.unlink(missing_ok=True)
            if original_keychains is not None:
                security(["list-keychains", "-d", "user", "-s", *original_keychains], check=False)
            if keychain:
                security(["delete-keychain", keychain], check=False)


if __name__ == "__main__":
    run()
