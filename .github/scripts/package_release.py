#!/usr/bin/env python3
"""Package the checked-in manifest; secrets remain in process environment."""
import base64
import json
import os
from pathlib import Path
import platform
import secrets
import subprocess
import tempfile


def configure(config, environment, system):
    """Return independent signing decisions and a manifest with public identity."""
    config = json.loads(json.dumps(config))
    apple_names = ("APPLE_CERTIFICATE", "APPLE_SIGNING_IDENTITY", "APPLE_ID", "APPLE_PASSWORD", "APPLE_TEAM_ID")
    apple = system == "Darwin" and all(environment.get(name) for name in apple_names)
    if apple:
        config.setdefault("macos", {})["signingIdentity"] = environment["APPLE_SIGNING_IDENTITY"]
    else:
        config.get("macos", {}).pop("signingIdentity", None)
    return config, apple, bool(environment.get("MINISIGN_KEY"))


def security(arguments, *, check=True):
    """Keep certificate passwords and keychain passwords out of exceptions."""
    result = subprocess.run(["security", *arguments], capture_output=True)
    if check and result.returncode:
        raise SystemExit("Apple signing setup failed; check certificate and signing secrets")
    return result


def run():
    manifest = Path("packager.json")
    config, apple, minisign = configure(json.loads(manifest.read_text()), os.environ, platform.system())
    tag = os.environ.get("GITHUB_REF", "")
    if tag.startswith("refs/tags/v") and config["version"] != tag.removeprefix("refs/tags/v"):
        raise SystemExit("Release tag must match packager.json version")
    binary_directory = Path(config["binariesDir"])
    suffix = ".exe" if platform.system() == "Windows" else ""
    for binary in config["binaries"]:
        if not (binary_directory / (binary["path"] + suffix)).is_file():
            raise SystemExit("A binary from packager.json is missing")
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
    with tempfile.TemporaryDirectory(prefix="xylo-signing-") as temporary:
        if apple:
            certificate = Path(temporary) / "certificate.p12"
            certificate.write_bytes(base64.b64decode(environment["APPLE_CERTIFICATE"], validate=True))
            certificate.chmod(0o600)
            keychain = str(Path(temporary) / "release.keychain-db")
            password = secrets.token_urlsafe(32)
            security(["create-keychain", "-p", password, keychain])
            security(["unlock-keychain", "-p", password, keychain])
            security(["import", str(certificate), "-k", keychain, "-P", environment.get("APPLE_CERTIFICATE_PASSWORD", ""), "-T", "/usr/bin/codesign"])
            security(["set-key-partition-list", "-S", "apple-tool:,apple:", "-s", "-k", password, keychain])
            security(["list-keychains", "-d", "user", "-s", keychain])
            # The imported keychain supplies signing; do not ask packager to import again.
            environment.pop("APPLE_CERTIFICATE", None)
            environment.pop("APPLE_CERTIFICATE_PASSWORD", None)
        elif platform.system() == "Darwin":
            print("::warning::APPLE_* secrets are incomplete; Apple signing and notarization skipped", flush=True)
            for name in tuple(environment):
                if name.startswith("APPLE_"):
                    environment.pop(name)
        # Keep the generated config beside the source manifest so all resource paths resolve.
        generated = Path("packager.ci.json")
        try:
            generated.write_text(json.dumps(config))
            subprocess.run(["cargo", "packager", "--config", str(generated), "--formats", environment["PACKAGE_FORMATS"]], env=environment, check=True)
        finally:
            generated.unlink(missing_ok=True)
            if keychain:
                security(["delete-keychain", keychain], check=False)


if __name__ == "__main__":
    run()
