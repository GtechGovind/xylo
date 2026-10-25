#!/usr/bin/env python3
"""Exercise real Windows packaging tools with synthetic native executables.

This deliberately runs only on disposable GitHub-hosted Windows VMs. It uses
the actual pinned packager, LLVM audit and NSIS installation function before
the application dependency graph is built. Its result never qualifies Xylo.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import uuid

sys.dont_write_bytecode = True
from release_common import BINARIES, ROOT, digest, packager, source_identity, write_json
from release_smoke import binary_architecture, install_nsis, terminate
from release_windows import audit_windows_imports, validate_windows_imports

TARGETS = {"x86_64-pc-windows-msvc": {"amd64", "x86_64"},
           "aarch64-pc-windows-msvc": {"arm64", "aarch64"}}
MAX_TOOL_OUTPUT = 8 * 1024 * 1024
FIXTURE_SOURCE = r'''#![cfg_attr(xylo_gui_fixture, windows_subsystem = "windows")]
fn main() {
    let executable = std::env::current_exe().expect("fixture executable");
    let name = executable.file_stem().expect("fixture basename").to_string_lossy();
    match std::env::args().nth(1).as_deref() {
        Some("--version") | Some("--help") => println!("Xylo packaging fixture {name} 0.0.0"),
        _ => std::process::exit(2),
    }
}
'''


def remove_fixture_directory(root: Path, *, timeout: float = 10) -> None:
    """Retire only our temporary tree, awaiting bounded Windows file release.

    NSIS has exited before this runs. Windows can still reject immediate
    deletion of its uninstaller with sharing/access errors. Keep other errors
    fatal, and retain the last OS cause if the owned tree stays inaccessible.
    """
    if (not root.name.startswith("xylo-windows-packaging-") or root.is_symlink()
            or root.parent.resolve() != Path(tempfile.gettempdir()).resolve()):
        raise ValueError("Refusing cleanup outside the owned packaging fixture directory")
    deadline = time.monotonic() + timeout
    while True:
        try:
            shutil.rmtree(root)
            return
        except FileNotFoundError:
            if not root.exists():
                return
            raise
        except PermissionError as error:
            if getattr(error, "winerror", None) not in (5, 32, 33):
                raise
            if time.monotonic() >= deadline:
                raise TimeoutError("Owned Windows packaging fixture files stayed inaccessible after cleanup deadline") from error
            time.sleep(0.1)


@contextmanager
def fixture_directory():
    directory = Path(tempfile.mkdtemp(prefix="xylo-windows-packaging-"))
    failure = None
    try:
        yield directory
    except BaseException as error:
        failure = error
        raise
    finally:
        try:
            remove_fixture_directory(directory)
        except BaseException as error:
            if failure is None:
                raise
            failure.add_note("Owned fixture cleanup also failed: " + repr(error))


def run_tool(args: list[str | Path], log: Path, *, timeout: float,
             limit: int = MAX_TOOL_OUTPUT) -> str:
    """Bound diagnostics and wall time; terminate only the owned process tree."""
    process = subprocess.Popen([str(value) for value in args], cwd=ROOT,
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               start_new_session=os.name != "nt")
    output = bytearray()
    errors = []

    def collect():
        try:
            with process.stdout:
                while chunk := process.stdout.read1(min(64 * 1024, limit + 1 - len(output))):
                    output.extend(chunk)
                    if len(output) > limit:
                        errors.append(ValueError("Packaging fixture tool output exceeds its budget"))
                        terminate(process)
                        return
        except OSError as error:
            errors.append(error)

    reader = threading.Thread(target=collect, daemon=True)
    reader.start()
    failure = None
    try:
        process.wait(timeout=timeout)
    except BaseException as error:
        failure = error
        terminate(process)
    finally:
        reader.join(timeout=5)
        log.parent.mkdir(parents=True, exist_ok=True)
        log.write_bytes(output[:limit])
    if reader.is_alive():
        raise ValueError("Packaging fixture tool output did not close")
    if failure is not None:
        raise failure
    if errors:
        raise errors[0]
    if process.returncode != 0:
        diagnostic = output[-8192:].decode("utf-8", errors="replace").encode("ascii", errors="backslashreplace").decode("ascii")
        raise ValueError(f"Packaging fixture tool exited {process.returncode}; see {log}\n{diagnostic}")
    return output.decode("utf-8", errors="strict")


def fixture_configuration(target: str, root: Path, token: str) -> dict:
    """Mirror production's NSIS layout without its application identity or hooks."""
    if target not in TARGETS or len(token) != 32 or any(char not in "0123456789abcdef" for char in token):
        raise ValueError("Expected a supported target and a unique fixture identity")
    return {
        "name": "xylo-packaging-fixture-" + token,
        "productName": "Xylo Packaging Fixture " + token,
        "identifier": "com.qurkos.xylo.packaging-fixture." + token,
        "publisher": "Xylo Packaging Fixture",
        "description": "Synthetic packaging tool fixture; not the Xylo application",
        "version": "0.0.0", "targetTriple": target,
        "formats": ["nsis"], "binariesDir": str(root / "binaries"),
        "outDir": str(root / "packages"),
        "binaries": [{"path": name, **({"main": True} if index == 0 else {})}
                     for index, name in enumerate(BINARIES)],
        "licenseFile": str(root / "notices/fixture LICENSE.txt"),
        "resources": [{"src": str(root / "native-assets"), "target": "native-assets"},
                      {"src": str(root / "notices"), "target": "licenses/dependencies"}],
        "nsis": {"installMode": "currentUser"},
        "windows": {"digestAlgorithm": "sha256", "allowDowngrades": False},
    }


def require_native_vm(target: str) -> str:
    if target not in TARGETS or platform.system() != "Windows" or platform.machine().lower() not in TARGETS[target]:
        raise ValueError("Windows packaging fixture requires the requested native Windows architecture")
    if os.environ.get("GITHUB_ACTIONS") != "true" or os.environ.get("RUNNER_ENVIRONMENT") != "github-hosted":
        raise ValueError("Real NSIS installation is restricted to disposable GitHub-hosted VMs")
    return "aarch64" if target.startswith("aarch64") else "x86_64"


def file_records(paths: list[tuple[str, Path]], architecture: str) -> list[dict]:
    records = []
    for name, path in paths:
        if path.is_symlink() or not path.is_file() or binary_architecture(path) != architecture:
            raise ValueError("Fixture executable is missing or has the wrong native machine")
        records.append({"name": name, "sha256": digest(path), "size_bytes": path.stat().st_size,
                        "architecture": architecture})
    return records


def verify_resources(installed: Path, expected: dict[str, str]) -> None:
    for relative, sha256 in expected.items():
        path = installed / relative
        if path.is_symlink() or not path.is_file() or not path.resolve().is_relative_to(installed.resolve()):
            raise ValueError("Installed fixture resource is missing or escaped its root")
        if digest(path) != sha256:
            raise ValueError("Installed fixture resource bytes changed")


def smoke(target: str, output: Path) -> dict:
    architecture = require_native_vm(target)
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    logs = output / "logs"
    configured = packager()
    tool = str(Path(shutil.which(configured) or configured).resolve(strict=True))
    version = run_tool(["rustc", "--version", "--verbose"], logs / "rustc-version.log", timeout=30)
    if not version.startswith("rustc 1.98.0 ") or f"host: {target}" not in version.splitlines():
        raise ValueError("Fixture requires the repository-pinned native Rust 1.98.0 toolchain")
    result = {"schema": 1, "target": target, "passed": False,
              "scope": "Actual LLVM and NSIS tools with synthetic native PEs; no Xylo application qualification",
              "packager": {"path": tool, "sha256": digest(Path(tool)), "version": "0.11.8"},
              "rustc": version, "source": source_identity(), "checks": []}
    write_json(output / "result.json", result)
    with fixture_directory() as directory:
        root = directory / "fixture inputs café"
        binaries, native, notices = root / "binaries", root / "native-assets", root / "notices"
        binaries.mkdir(parents=True)
        runtime = native / "whisper-runtime/runtime"
        runtime.mkdir(parents=True)
        notices.mkdir()
        source = root / "fixture.rs"
        source.write_text(FIXTURE_SOURCE, encoding="utf-8", newline="\n")
        inputs = [(name, binaries / (name + ".exe")) for name in BINARIES]
        inputs.append(("whisper-cli", runtime / "whisper-cli.exe"))
        compiler = ["rustc", "--edition=2024", "--crate-name", "xylo_packaging_fixture", "--target", target,
                    "-Ctarget-feature=+crt-static", "-Copt-level=0", str(source)]
        # Test both a GUI-subsystem main (including its inherited version pipe)
        # and console-subsystem helper/worker without compiling any app crate.
        run_tool([*compiler, "--cfg", "xylo_gui_fixture", "-o", inputs[0][1]],
                 logs / "rustc-gui.log", timeout=120)
        run_tool([*compiler, "-o", inputs[1][1]], logs / "rustc-console.log", timeout=120)
        for _, path in inputs[2:]:
            shutil.copyfile(inputs[1][1], path)
        records = file_records(inputs, architecture)
        result["binaries"] = records
        result["checks"].append("four_synthetic_native_pe_machines")
        write_json(native / "whisper-runtime/manifest.json", {
            "fixture_only": True, "files": [{"path": "runtime/whisper-cli.exe", "sha256": digest(inputs[3][1]),
                                             "size_bytes": inputs[3][1].stat().st_size}]})
        (notices / "fixture LICENSE.txt").write_text("Synthetic fixture only; not a product license inventory.\n",
                                                     encoding="utf-8", newline="\n")
        nested = notices / "nested Unicode café"
        nested.mkdir()
        (nested / "NOTICE 測試.txt").write_text("Exact Unicode resource bytes: Ł測試\n", encoding="utf-8", newline="\n")
        audit = notices / "windows-pe-imports.json"
        audit_windows_imports(binaries, native, audit, target)
        result["checks"].append("actual_llvm_static_crt_import_audit")
        shutil.copyfile(audit, output / audit.name)
        expected_resources = {}
        for base, target_prefix in ((native, "native-assets"), (notices, "licenses/dependencies")):
            for path in sorted(base.rglob("*")):
                if path.is_file():
                    expected_resources[target_prefix + "/" + path.relative_to(base).as_posix()] = digest(path)
        config = fixture_configuration(target, root, uuid.uuid4().hex)
        config_path = root / "packager.json"
        write_json(config_path, config)
        write_json(output / "fixture.resolved.json", config)
        run_tool([tool, "--config", config_path], logs / "packager.log", timeout=300)
        installers = list((root / "packages").glob("*.exe"))
        if len(installers) != 1 or installers[0].is_symlink():
            raise ValueError("Actual NSIS packager did not produce exactly one fixture installer")
        result["installer"] = {"name": installers[0].name, "sha256": digest(installers[0]),
                               "size_bytes": installers[0].stat().st_size}
        result["checks"].append("actual_pinned_nsis_installer_compiled")
        installed = root / "installed fixture café"
        try:
            # Use the exact product CreateProcess command and unquoted /D tail.
            install_nsis(installers[0], installed)
            result["checks"].append("production_silent_nsis_install_executed")
            installed_inputs = [(name, installed / ("native-assets/whisper-runtime/runtime/whisper-cli.exe"
                                                   if name == "whisper-cli" else name + ".exe"))
                                for name, _ in inputs]
            if file_records(installed_inputs, architecture) != records:
                raise ValueError("NSIS changed one of the four native fixture executables")
            verify_resources(installed, expected_resources)
            validate_windows_imports(installed / "licenses/dependencies/windows-pe-imports.json", target, records)
            result["checks"].extend(["installed_four_pe_hashes_unchanged", "nested_unicode_resources_unchanged",
                                      "installed_import_report_bound_to_actual_files"])
            for name, path in installed_inputs:
                flag = "--help" if name == "whisper-cli" else "--version"
                actual = run_tool([path, flag], logs / (name + "-execution.log"), timeout=15)
                if actual.strip() != f"Xylo packaging fixture {name} 0.0.0":
                    raise ValueError("Installed native fixture did not report its expected basename")
            result["checks"].append("all_four_installed_native_fixture_commands")
        finally:
            uninstaller = installed / "uninstall.exe"
            if uninstaller.is_file():
                # NSIS's _? tail is likewise intentionally last and unquoted.
                command = subprocess.list2cmdline([str(uninstaller), "/S"]) + " _?=" + str(installed)
                subprocess.run(command, executable=str(uninstaller), shell=False, check=True, timeout=60)
        result["checks"].append("fixture_uninstaller_executed")
        result["resources"] = expected_resources
    result["checks"].append("owned_fixture_directory_removed")
    result["passed"] = True
    write_json(output / "result.json", result)
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGETS, required=True)
    parser.add_argument("--output", type=Path, required=True, help="Fresh retained fixture evidence directory")
    arguments = parser.parse_args()
    print(json.dumps(smoke(arguments.target, arguments.output), indent=2, sort_keys=True, ensure_ascii=True))
