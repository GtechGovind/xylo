"""Inspect built Windows PE imports with LLVM; no execution or DLL downloads.

llvm-readobj --coff-imports emits both Import and DelayImport tables (including
nested imported symbols). This module parses that tool output, not PE tables.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import threading

from release_common import BINARIES, digest, write_json

MAX_OUTPUT = 8 * 1024 * 1024
PHASE = "final PE inputs before installer packaging"
FORBIDDEN = re.compile(r"^(?:vcruntime|msvcp|msvcr|msvcprt|concrt|vcomp|libomp|libiomp|libgomp)[a-z0-9_.+-]*\.dll$", re.I)
# These two names are Windows OS components, not the redistributable CRT/C++
# DLLs selected by /MD. UCRT and api-ms-win-* imports are also allowed.
SYSTEM_RUNTIME_IMPORTS = {"msvcrt.dll", "msvcp_win.dll"}


def forbidden_import(name: str) -> bool:
    return name.casefold() not in SYSTEM_RUNTIME_IMPORTS and bool(FORBIDDEN.fullmatch(name))


def run_llvm(args: list[str], *, timeout: float, limit: int = MAX_OUTPUT) -> subprocess.CompletedProcess:
    """Bound both output buffers while draining native pipes on Windows too."""
    process = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    buffers = [bytearray(), bytearray()]
    errors = []

    def collect(pipe, index, budget):
        try:
            with pipe:
                while chunk := pipe.read1(min(64 * 1024, budget + 1 - len(buffers[index]))):
                    buffers[index].extend(chunk)
                    if len(buffers[index]) > budget:
                        errors.append(ValueError("LLVM output exceeds its inspection budget"))
                        process.kill()
                        return
        except OSError as error:
            errors.append(error)

    readers = [threading.Thread(target=collect, args=(pipe, index, budget), daemon=True)
               for index, (pipe, budget) in enumerate(((process.stdout, limit), (process.stderr, 64 * 1024)))]
    for reader in readers:
        reader.start()
    try:
        process.wait(timeout=timeout)
    except BaseException:
        if process.poll() is None:
            process.kill()
        process.wait(timeout=5)
        raise
    finally:
        for reader in readers:
            reader.join(timeout=2)
    if any(reader.is_alive() for reader in readers):
        raise ValueError("LLVM output pipes did not close after the inspection process exited")
    if errors:
        raise errors[0]
    return subprocess.CompletedProcess(args, process.returncode,
                                       buffers[0].decode("utf-8"), buffers[1].decode("utf-8"))


def llvm_readobj() -> Path:
    configured = os.environ.get("WORKBENCH_LLVM_READOBJ")
    if configured:
        candidate = Path(configured)
        if not candidate.is_file():
            raise ValueError("WORKBENCH_LLVM_READOBJ must select an installed llvm-readobj executable")
        return candidate.resolve()
    found = shutil.which("llvm-readobj")
    if found:
        return Path(found).resolve()
    for variable in ("ProgramFiles", "ProgramFiles(x86)"):
        if directory := os.environ.get(variable):
            candidate = Path(directory) / "LLVM/bin/llvm-readobj.exe"
            if candidate.is_file():
                return candidate.resolve()
    raise ValueError("Windows release import audit requires installed LLVM llvm-readobj; set WORKBENCH_LLVM_READOBJ")


def parse_imports(text: str) -> dict:
    if len(text.encode("utf-8")) > MAX_OUTPUT:
        raise ValueError("LLVM import report exceeds its output budget")
    headers, imports, delayed, stack = {}, [], [], []
    name, root_kind = None, None
    for raw in text.splitlines():
        line = raw.strip()
        if not line:
            continue
        if line.endswith(" {"):
            kind = line[:-2]
            if not stack:
                if kind not in ("Import", "DelayImport"):
                    raise ValueError("Unexpected LLVM import report block")
                name, root_kind = None, kind
            elif stack != ["DelayImport"] or kind != "Import":
                raise ValueError("Unexpected nested LLVM import block")
            stack.append(kind)
        elif line == "}":
            if not stack:
                raise ValueError("Unbalanced LLVM import report")
            stack.pop()
            if not stack:
                if name is None:
                    raise ValueError("LLVM import block has no DLL name")
                (delayed if root_kind == "DelayImport" else imports).append(name)
                if len(imports) + len(delayed) > 4096:
                    raise ValueError("PE import count exceeds the audit budget")
        elif stack:
            if len(stack) == 1 and line.startswith("Name:"):
                if name is not None:
                    raise ValueError("LLVM import block repeats its DLL name")
                name = line.removeprefix("Name:").strip()
                if not re.fullmatch(r"[A-Za-z0-9_.+-]{1,255}\.dll", name, re.I):
                    raise ValueError("Unsupported DLL name in LLVM import report")
            elif not re.fullmatch(r"(?:ImportLookupTableRVA|ImportAddressTableRVA|Attributes|ModuleHandle|ImportAddressTable|ImportNameTable|BoundDelayImportTable|UnloadDelayImportTable|Symbol|Address): .*", line):
                raise ValueError("Unrecognized LLVM import report field")
        else:
            key, separator, value = line.partition(": ")
            if not separator or key not in ("File", "Format", "Arch", "AddressSize") or key in headers:
                raise ValueError("Unexpected or repeated LLVM file header")
            headers[key] = value
    if stack or set(headers) != {"File", "Format", "Arch", "AddressSize"}:
        raise ValueError("Truncated LLVM import report")
    if not headers["Format"].startswith("COFF-") or not imports and not delayed:
        raise ValueError("Expected a COFF image with reported DLL imports")
    return {"format": headers["Format"], "architecture": headers["Arch"],
            "imports": sorted(set(imports), key=str.casefold),
            "delay_imports": sorted(set(delayed), key=str.casefold)}


def audit_windows_imports(binaries: Path, native: Path, report_path: Path, target: str) -> dict:
    if target not in ("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"):
        raise ValueError("PE import audit requires an explicit supported Windows MSVC target")
    tool = llvm_readobj()
    version_result = run_llvm([str(tool), "--version"], timeout=15, limit=64 * 1024)
    if version_result.returncode != 0 or version_result.stderr.strip() or not version_result.stdout.strip():
        raise ValueError("Installed LLVM did not report its version successfully")
    version = version_result.stdout
    records, errors = [], []
    expected_format = "COFF-ARM64" if target.startswith("aarch64") else "COFF-x86-64"
    paths = [(name, binaries / (name + ".exe")) for name in BINARIES]
    paths.append(("whisper-cli", native / "whisper-runtime/runtime/whisper-cli.exe"))
    for name, path in paths:
        item = {"name": name, "path": str(path)}
        records.append(item)
        try:
            if not path.is_file() or path.is_symlink():
                raise ValueError("Release PE input must be a regular file")
            with path.open("rb") as stream:
                if stream.read(2) != b"MZ":
                    raise ValueError("Release input has no PE DOS header")
            before = digest(path)
            item.update(sha256=before, size_bytes=path.stat().st_size)
            result = run_llvm([str(tool), "--coff-imports", str(path)], timeout=60)
            if result.returncode != 0 or result.stderr.strip():
                raise ValueError("LLVM could not completely inspect PE imports: " + result.stderr.strip()[:2048])
            parsed = parse_imports(result.stdout)
            if parsed["format"] != expected_format:
                raise ValueError("PE import report machine differs from the release target")
            if digest(path) != before:
                raise ValueError("Release PE changed during import inspection")
            item.update(parsed,
                        report_sha256=hashlib.sha256(result.stdout.encode()).hexdigest())
            blocked = sorted({dll for dll in parsed["imports"] + parsed["delay_imports"] if forbidden_import(dll)}, key=str.casefold)
            item["forbidden_runtime_imports"] = blocked
            if blocked:
                raise ValueError("Unstaged redistributable runtime imports: " + ", ".join(blocked))
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            item["error"] = str(error)[:2048]
            errors.append(name + ": " + item["error"])
    report = {"schema": 1, "target": target, "phase": PHASE,
              "tool": {"path": str(tool), "sha256": digest(tool), "version": version[:2048]},
              "policy": "Reject VC CRT/C++ redistributable and OpenMP runtime DLL imports; Windows system/UCRT imports are permitted.",
              "binaries": records, "passed": not errors,
              "limitations": ["Import inspection does not qualify Windows execution or dynamically loaded optional components."]}
    write_json(report_path, report, exclusive=True)
    if errors:
        raise ValueError("Windows PE runtime audit failed: " + "; ".join(errors))
    return report


def validate_windows_imports(report_path: Path, target: str, binaries: list[dict]) -> dict:
    """Bind the retained inspection to the four actually installed input hashes.

    This consumer does not invoke LLVM or execute a binary. Native smoke owns
    execution; release aggregation uses the identical identity check offline.
    """
    targets = {"aarch64-pc-windows-msvc": ("COFF-ARM64", "aarch64"),
               "x86_64-pc-windows-msvc": ("COFF-x86-64", "x86_64")}
    if target not in targets:
        raise ValueError("Windows import proof requires a supported MSVC target")
    if report_path.is_symlink() or not report_path.is_file():
        raise ValueError("Missing or unsafe Windows import proof")
    with report_path.open("rb") as stream:
        payload = stream.read(MAX_OUTPUT + 1)
    if len(payload) > MAX_OUTPUT:
        raise ValueError("Windows import proof exceeds its inspection budget")

    def unique_object(pairs):
        value = {}
        for key, item in pairs:
            if key in value:
                raise ValueError("Windows import proof repeats a JSON field")
            value[key] = item
        return value

    report = json.loads(payload, object_pairs_hook=unique_object)
    if (not isinstance(report, dict) or type(report.get("schema")) is not int or report["schema"] != 1
            or report.get("passed") is not True or report.get("target") != target or report.get("phase") != PHASE):
        raise ValueError("Windows import proof is failed, incomplete or from another target")
    tool = report.get("tool")
    if (not isinstance(tool, dict) or not re.fullmatch(r"[0-9a-f]{64}", str(tool.get("sha256", "")))
            or not isinstance(tool.get("version"), str) or not 0 < len(tool["version"]) <= 2048):
        raise ValueError("Windows import proof lacks its inspection tool identity")
    expected_names = {*BINARIES, "whisper-cli"}
    records = report.get("binaries")
    if (not isinstance(records, list) or len(records) != 4 or not isinstance(binaries, list) or len(binaries) != 4
            or any(not isinstance(item, dict) for item in records + binaries)
            or {item.get("name") for item in records} != expected_names
            or {item.get("name") for item in binaries} != expected_names):
        raise ValueError("Windows import proof must identify exactly four installed executables")
    installed = {item["name"]: item for item in binaries}
    expected_format, architecture = targets[target]
    for item in records:
        actual = installed[item["name"]]
        if ("error" in item or item.get("format") != expected_format
                or actual.get("architecture") != architecture
                or not re.fullmatch(r"[0-9a-f]{64}", str(item.get("sha256", "")))
                or item["sha256"] != actual.get("sha256")
                or type(item.get("size_bytes")) is not int or item["size_bytes"] < 1
                or item["size_bytes"] != actual.get("size_bytes")
                or not re.fullmatch(r"[0-9a-f]{64}", str(item.get("report_sha256", "")))):
            raise ValueError("Windows import proof differs from the installed executable identity")
        imports, delayed = item.get("imports"), item.get("delay_imports")
        if (not isinstance(imports, list) or not isinstance(delayed, list)
                or not 0 < len(imports) + len(delayed) <= 4096
                or any(not isinstance(name, str) or not re.fullmatch(r"[A-Za-z0-9_.+-]{1,255}\.dll", name, re.I)
                       or forbidden_import(name) for name in imports + delayed)
                or item.get("forbidden_runtime_imports") != []):
            raise ValueError("Windows import proof contains forbidden or malformed DLL imports")
    return report
