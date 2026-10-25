#!/usr/bin/env python3
"""Early native Linux tool integration, using synthetic application executables.

This catches ldd/readelf/dpkg output assumptions before compiling the app. It
does not qualify a real AppImage, Debian package, application, or Linux install.
"""
import argparse
import json
from pathlib import Path
import platform
import shutil
import sys
import tempfile

sys.dont_write_bytecode = True
from release_common import BINARIES, ROOT, digest
from release_linux_notices import (TARGETS, collect_linux_notices, command, pinned_binary,
                                   validate_linux_appimage, validate_linux_inventory)


def smoke(target: str) -> dict:
    architecture = "aarch64" if target.startswith("aarch64") else "x86_64"
    if platform.system() != "Linux" or platform.machine().lower() not in ({"arm64", "aarch64"} if architecture == "aarch64" else {"amd64", "x86_64"}):
        raise ValueError("The actual Linux native-tool fixture must run on its requested architecture")
    with tempfile.TemporaryDirectory(prefix="xylo-linux-notices-") as directory:
        root = Path(directory)
        binaries, native, metadata, appdir = [root / name for name in ("bin", "native", "metadata", "appdir")]
        binaries.mkdir()
        (native / "whisper-runtime/runtime").mkdir(parents=True)
        source = root / "fixture.c"
        source.write_text("int main(void) { return XYLO_FIXTURE_ID; }\n", encoding="utf-8", newline="\n")
        inputs = [(name, binaries / name) for name in BINARIES]
        inputs.append(("whisper-cli", native / "whisper-runtime/runtime/whisper-cli"))
        for index, (_, path) in enumerate(inputs):
            command(["cc", str(source), "-DXYLO_FIXTURE_ID=" + str(index), "-Wl,--build-id=sha1", "-Wl,--no-as-needed",
                     "-lgomp", "-lxkbcommon-x11", "-lxkbcommon", "-o", str(path)])
        inventory = collect_linux_notices(binaries, native, metadata, target)
        for name, path in inputs:
            relative = "usr/bin/" + name if name in BINARIES else "usr/bin/native-assets/whisper-runtime/runtime/whisper-cli"
            destination = appdir / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, destination)
        for item in inventory["libraries"]:
            destination = appdir / "usr/lib" / item["identity"]["soname"]
            destination.parent.mkdir(parents=True, exist_ok=True)
            if not destination.exists():
                shutil.copyfile(item["resolved_path"], destination)
        # Exactly the byte-changing transform performed by native deployment.
        mutated = appdir / "usr/lib" / inventory["libraries"][0]["identity"]["soname"]
        original_hash = digest(mutated)
        command(["patchelf", "--set-rpath", "$ORIGIN", str(mutated)])
        if digest(mutated) == original_hash:
            raise ValueError("Native fixture did not actually mutate an ELF RPATH")
        cache = ROOT / "target/release-runtime-cache"
        shutil.copyfile(pinned_binary(metadata, target, cache, "apprun"), appdir / "AppRun")
        runtime = pinned_binary(metadata, target, cache, "appimage-runtime")
        image = root / "synthetic.AppImage"
        image.write_bytes(runtime.read_bytes() + b"Not a real SquashFS: early native tools fixture only\n")
        records = [{"name": name, "sha256": digest(path), "size_bytes": path.stat().st_size, "architecture": architecture} for name, path in inputs]
        report = validate_linux_appimage(metadata, appdir, target, records, image)
        text = metadata / inventory["packages"][0]["texts"][0]["path"]
        text.write_bytes(b"Intentional early-smoke tamper")
        try:
            validate_linux_inventory(metadata, target)
        except ValueError:
            pass
        else:
            raise ValueError("Native fixture incorrectly accepted a tampered distro notice")
        return {"schema": 1, "target": target, "passed": True,
                "scope": "actual Linux ldd/readelf/dpkg + synthetic four executables/AppDir; no package qualification",
                "libraries": len(report["libraries"]), "packages": len(inventory["packages"]),
                "checks": ["actual_target_linkage", "installed_package_provenance", "exact_distro_texts",
                           "changed_rpath_identity_match", "pinned_infrastructure_bytes", "notice_tamper_rejected"]}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=TARGETS)
    arguments = parser.parse_args()
    print(json.dumps(smoke(arguments.target), indent=2, sort_keys=True))
