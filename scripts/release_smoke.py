#!/usr/bin/env python3
"""Smoke the actual native package on its own disposable CI host; no accounts/devices."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path, PurePath, PureWindowsPath
import platform
import queue
import re
import signal
import stat
import struct
import subprocess
import sys
import tempfile
import threading
import time
import uuid
import zipfile

sys.dont_write_bytecode = True
from release_common import BINARIES, ROOT, TARGETS, digest, source_identity, write_json
from release_metadata import checksums, validate_inventory

MAX_FRAME = 8 * 1024 * 1024


def windows_namespace(path: PureWindowsPath) -> PureWindowsPath:
    """Use one comparison namespace for resolved Windows drive and UNC paths.

    Rust canonicalize returns extended paths. Python preserves that prefix only
    when its input already had one, so resolve alone does not make the two
    spelling forms comparable. Device namespaces and unresolved parents are
    rejected rather than converted into ordinary filesystem paths.
    """
    value = str(path)
    if value[:8].casefold() == "\\\\?\\unc\\":
        value = "\\\\" + value[8:]
    elif value.startswith("\\\\?\\"):
        value = value[4:]
    normalized = PureWindowsPath(value)
    drive = normalized.drive
    ordinary_drive = re.fullmatch(r"[a-zA-Z]:", drive) is not None
    unc = drive.startswith("\\\\") and len(drive[2:].split("\\")) == 2
    if unc:
        server, share = drive[2:].split("\\")
        unc = bool(server and share and server not in (".", "?"))
    if (not normalized.is_absolute() or not (ordinary_drive or unc)
            or ".." in normalized.parts):
        raise ValueError("Artifact path uses an unsupported Windows namespace")
    return normalized


def artifact_relative_path(artifact: PurePath, root: PurePath) -> PurePath:
    """Component-wise comparison only; callers must resolve physical paths first."""
    if isinstance(artifact, PureWindowsPath) and isinstance(root, PureWindowsPath):
        artifact, root = windows_namespace(artifact), windows_namespace(root)
    if not artifact.is_absolute() or not root.is_absolute() or ".." in artifact.parts or ".." in root.parts:
        raise ValueError("Artifact paths must be resolved and absolute")
    try:
        relative = artifact.relative_to(root)
    except ValueError as error:
        raise ValueError("Packaged helper returned an unexpected artifact path") from error
    if not relative.parts:
        raise ValueError("Packaged helper returned a directory instead of an artifact")
    return relative


def confined_artifact(artifact: Path, root: Path) -> Path:
    """Require an existing regular file physically inside the isolated root."""
    resolved_root = root.resolve(strict=True)
    resolved_artifact = artifact.resolve(strict=True)
    if not resolved_root.is_dir() or not stat.S_ISREG(resolved_artifact.stat().st_mode):
        raise ValueError("Packaged helper artifact must be a regular file in its artifact directory")
    relative = artifact_relative_path(resolved_artifact, resolved_root)
    # Windows supports case-sensitive directories. PureWindowsPath comparison
    # intentionally ignores case; verify the actual parent identity too, so a
    # differently cased sibling cannot borrow the root's lexical namespace.
    expected_parent = resolved_root.joinpath(*relative.parts[:-1])
    if not resolved_artifact.parent.samefile(expected_parent):
        raise ValueError("Packaged helper artifact is outside its physical artifact directory")
    return resolved_artifact


def binary_architecture(path: Path) -> str:
    """Read native executable headers, rather than trusting a target directory."""
    with path.open("rb") as stream:
        header = stream.read(64)
        if header[:2] == b"MZ":
            if len(header) < 64:
                raise ValueError("Truncated PE executable")
            offset = struct.unpack_from("<I", header, 60)[0]
            if offset > 1024 * 1024:
                raise ValueError("Invalid PE header offset")
            stream.seek(offset)
            pe = stream.read(6)
            if pe[:4] != b"PE\0\0" or len(pe) != 6:
                raise ValueError("Invalid PE signature")
            machine = struct.unpack_from("<H", pe, 4)[0]
            return {0x8664: "x86_64", 0xAA64: "aarch64"}.get(machine, "unsupported")
        if header[:4] == b"\x7fELF" and header[4:6] == b"\x02\x01":
            machine = struct.unpack_from("<H", header, 18)[0]
            return {62: "x86_64", 183: "aarch64"}.get(machine, "unsupported")
        if header[:4] == b"\xcf\xfa\xed\xfe":
            machine = struct.unpack_from("<I", header, 4)[0]
            return {0x01000007: "x86_64", 0x0100000C: "aarch64"}.get(machine, "unsupported")
    raise ValueError("Expected a native 64-bit PE, ELF or Mach-O executable")


def terminate(process: subprocess.Popen) -> None:
    if process.poll() is not None:
        return
    if os.name == "nt":
        subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15, check=False)
    else:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        if os.name != "nt":
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        process.kill()
        process.wait(timeout=10)


class JsonProcess:
    def __init__(self, args: list[str], log: Path):
        self.log = log.open("wb")
        self.process = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=self.log, start_new_session=os.name != "nt")
        self.frames: queue.Queue = queue.Queue(64)
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def _read(self):
        try:
            while True:
                line = self.process.stdout.readline(MAX_FRAME + 2)
                if not line:
                    raise ValueError("Native protocol ended before its response")
                if len(line) > MAX_FRAME + 1 or not line.endswith(b"\n"):
                    raise ValueError("Native protocol frame exceeds its budget")
                self.frames.put_nowait(json.loads(line))
        except Exception as error:
            try:
                self.frames.put_nowait(error)
            except queue.Full:
                pass

    def receive(self, timeout=60):
        try:
            value = self.frames.get(timeout=timeout)
        except queue.Empty as error:
            raise ValueError("Native protocol response timed out") from error
        if isinstance(value, Exception):
            raise value
        return value

    def execute(self, name: str, args: dict):
        request_id = str(uuid.uuid4())
        value = {"protocol_version": 1, "id": request_id,
                 "command": {"kind": "execute", "name": name, "args": args}}
        self.process.stdin.write(json.dumps(value).encode() + b"\n")
        self.process.stdin.flush()
        response = self.receive()
        if response.get("id") != request_id or response.get("protocol_version") != 1:
            raise ValueError("Native response correlation/version mismatch")
        if response.get("outcome", {}).get("status") != "completed":
            raise ValueError("Packaged helper rejected the smoke operation; inspect helper log")
        return response["outcome"]["value"]

    def close(self):
        try:
            self.process.stdin.close()
            self.process.wait(timeout=15)
            if self.process.returncode != 0:
                raise ValueError("Native helper/worker did not shut down cleanly")
        finally:
            terminate(self.process)
            self.reader.join(timeout=2)
            self.process.stdout.close()
            self.log.close()


def packaged_root(output: Path, target: str, scratch: Path) -> tuple[Path, str]:
    if "apple" in target:
        bundles = list(output.glob("*.app"))
        if len(bundles) != 1:
            raise ValueError("Expected exactly one packaged app")
        subprocess.run(["codesign", "--verify", "--deep", "--strict", str(bundles[0])], check=True, timeout=60)
        return bundles[0] / "Contents/MacOS", "app_bundle_codesign_verified"
    if "windows" in target:
        installers = list(output.glob("*.exe"))
        if len(installers) != 1:
            raise ValueError("Expected exactly one NSIS installer")
        install = scratch / "installed"
        # NSIS requires /D last, and its entire unquoted value in one argv entry.
        subprocess.run([str(installers[0]), "/S", f"/D={install}"], check=True, timeout=180)
        return install, "nsis_silent_install_executed"
    images = list(output.glob("*.AppImage"))
    if len(images) != 1:
        raise ValueError("Expected exactly one AppImage")
    images[0].chmod(images[0].stat().st_mode | 0o100)
    with (scratch / "extract.log").open("wb") as log:
        subprocess.run([str(images[0]), "--appimage-extract"], cwd=scratch,
                       stdout=log, stderr=subprocess.STDOUT, check=True, timeout=180)
    return scratch / "squashfs-root/usr/bin", "appimage_native_extraction_executed"


def smoke(output: Path, target: str) -> dict:
    identity = source_identity()
    plan = json.loads((output / "build-plan.json").read_text())
    status = json.loads((output / "release-status.json").read_text())
    if identity["dirty"] or not identity["commit"] or plan["source"] != identity:
        raise ValueError("Smoke source differs from the clean package build")
    if plan["target"] != target or status["mode"] != "preview" or status["updater_feed_enabled"]:
        raise ValueError("Only unsigned preview packages with disabled updates use this gate")
    validate_inventory(output / "release-metadata", strict=True)
    architecture = "aarch64" if target.startswith("aarch64") else "x86_64"
    native_host = platform.machine().lower()
    if native_host not in ({"arm64", "aarch64"} if architecture == "aarch64" else {"x86_64", "amd64"}):
        raise ValueError("Smoke must run on a native host of the requested architecture")
    expected_os = "Darwin" if "apple" in target else "Windows" if "windows" in target else "Linux"
    if platform.system() != expected_os:
        raise ValueError("Smoke host OS differs from its target")
    log_root = output / "smoke-logs"
    log_root.mkdir()
    records, passed = [], []
    with tempfile.TemporaryDirectory(prefix="xylo-package-smoke-") as directory:
        scratch = Path(directory).resolve()
        binaries, package_check = packaged_root(output, target, scratch)
        passed.append(package_check)
        suffix = ".exe" if "windows" in target else ""
        resources = binaries.parent / "Resources" if "apple" in target else binaries
        if "linux" in target:
            resources = scratch / "squashfs-root/usr/lib/Xylo"
            # cargo-packager uses the configured product name for generic resources.
            inventories = list((scratch / "squashfs-root").glob("usr/**/licenses/dependencies/licenses.json"))
            if len(inventories) != 1:
                raise ValueError("Packaged dependency license inventory is missing/ambiguous")
            resources = inventories[0].parent.parent.parent
        validate_inventory(resources / "licenses/dependencies", strict=True)
        asset_root = (binaries.parent / "Resources/native-assets" if "apple" in target else binaries / "native-assets")
        manifest = json.loads((asset_root / "whisper-runtime/manifest.json").read_text())
        for entry in manifest["files"]:
            relative = Path(entry["path"])
            if relative.is_absolute() or ".." in relative.parts:
                raise ValueError("Unsafe packaged native asset path")
            path = asset_root / "whisper-runtime" / relative
            if path.is_symlink() or digest(path) != entry["sha256"] or path.stat().st_size != entry["size_bytes"]:
                raise ValueError("Packaged native asset integrity mismatch")
        for name in (*BINARIES, "whisper-cli"):
            path = (asset_root / "whisper-runtime/runtime" if name == "whisper-cli" else binaries) / (name + suffix)
            if binary_architecture(path) != architecture:
                raise ValueError("Packaged executable machine differs from the native target")
            records.append({"name": name, "sha256": digest(path), "size_bytes": path.stat().st_size,
                            "architecture": architecture})
        passed.extend(["all_four_native_executable_machines", "packaged_license_inventory_complete", "native_asset_hashes"])
        for name in BINARIES:
            version = subprocess.run([str(binaries / (name + suffix)), "--version"],
                                     capture_output=True, text=True, check=True, timeout=30).stdout
            if "Xylo" not in version:
                raise ValueError("Packaged executable did not report its product version")
        passed.append("all_three_packaged_version_commands")
        whisper = asset_root / "whisper-runtime/runtime" / ("whisper-cli" + suffix)
        subprocess.run([str(whisper), "--help"], stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, check=True, timeout=30)
        passed.append("packaged_whisper_cli_help")
        workspace = scratch / "workspace"
        workspace.mkdir()
        helper = JsonProcess([str(binaries / ("workbench-helper" + suffix)), "--data-dir", str(scratch / "helper-data"),
                              "--workspace", str(workspace)], log_root / "helper.log")
        try:
            if helper.execute("helper_version", {}).get("protocol_version") != 1:
                raise ValueError("Unexpected packaged helper protocol")
            helper.execute("helper_status", {})
            result = helper.execute("document_export_docx", {"name": "native-smoke", "paragraphs": ["Xylo native package smoke"]})
            artifacts = result.get("artifacts", [])
            if len(artifacts) != 1:
                raise ValueError("Packaged document operation returned no artifact")
            artifact = confined_artifact(Path(artifacts[0]), scratch / "helper-data/artifacts")
            with zipfile.ZipFile(artifact) as archive:
                if b"Xylo native package smoke" not in archive.read("word/document.xml"):
                    raise ValueError("Packaged helper document content mismatch")
        finally:
            helper.close()
        passed.append("packaged_helper_correlated_jsonl_and_actual_docx_export")
        worker = JsonProcess([str(binaries / ("workbench-worker" + suffix)), "--data-dir", str(scratch / "worker-data")],
                             log_root / "worker.log")
        try:
            ready = worker.receive()
            if ready.get("version") != 1 or ready.get("payload", {}).get("event") != "ready":
                raise ValueError("Packaged worker did not become ready")
        finally:
            worker.close()
        passed.append("packaged_worker_ready_and_clean_stdin_shutdown")
        with (log_root / "desktop.log").open("wb") as log:
            data = scratch / "desktop-data"
            process = subprocess.Popen([str(binaries / ("agent-workbench" + suffix)), "--data-dir", str(data)],
                                       stdout=log, stderr=subprocess.STDOUT, start_new_session=os.name != "nt")
            try:
                deadline = time.monotonic() + 45
                while not (data / "workbench.db").is_file():
                    if process.poll() is not None or time.monotonic() > deadline:
                        raise ValueError("Packaged desktop failed isolated native startup; inspect desktop.log")
                    time.sleep(0.1)
                time.sleep(8)
                if process.poll() is not None:
                    raise ValueError("Packaged desktop exited during native launch smoke")
            finally:
                terminate(process)
        passed.append("packaged_desktop_isolated_profile_launch_alive_8s")
    extensions = (".dmg", ".app.tar.gz") if "apple" in target else (".exe",) if "windows" in target else (".AppImage", ".deb")
    artifacts = [{"name": path.name, "sha256": digest(path), "size_bytes": path.stat().st_size}
                 for path in sorted(output.iterdir()) if path.is_file() and path.name.endswith(extensions)]
    if len(artifacts) != len(extensions):
        raise ValueError("Missing or ambiguous native release artifacts")
    return {"schema": 1, "target": target, "platform": TARGETS[target][0], "source": identity,
            "cargo_lock_sha256": digest(ROOT / "Cargo.lock"), "checks": passed, "binaries": records,
            "artifacts": artifacts, "passed": True, "license_inventory_complete": True,
            "qualification": "native_package_smoke", "limitations": [
                "Unsigned preview: macOS uses ad hoc signing; Windows has no Authenticode certificate.",
                "Launch/process/protocol smoke is not visual, accessibility, device, account or live-provider acceptance.",
                "Desktop profile creation and process liveness do not establish full Host initialization success.",
                "Linux AppImage is extracted and launched; Debian installation is not exercised.",
                "No model weights, microphone recording, remote accounts or paid APIs are used."]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=TARGETS)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    output = args.output.resolve()
    proof = smoke(output, args.target)
    write_json(output / "qualification.json", proof, exclusive=True)
    checksums(output)
    print(f"Native package smoke passed for {args.target}; full product acceptance remains separate.")


if __name__ == "__main__":
    main()
