"""Shared release tooling. Build-time Python only; the shipped app is native Rust."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]
PACKAGER_VERSION = "0.11.8"
BINARIES = ("agent-workbench", "workbench-worker", "workbench-helper")
TARGETS = {
    "aarch64-apple-darwin": ("macos-aarch64", "app"),
    "x86_64-apple-darwin": ("macos-x86_64", "app"),
    "x86_64-pc-windows-msvc": ("windows-x86_64", "nsis"),
    "aarch64-pc-windows-msvc": ("windows-aarch64", "nsis"),
    "x86_64-unknown-linux-gnu": ("linux-x86_64", "appimage"),
    "aarch64-unknown-linux-gnu": ("linux-aarch64", "appimage"),
}


def run(args: list[str | Path], *, env: dict | None = None, capture: bool = False) -> str:
    # Never print argv or environments: signing tools may receive identity data.
    result = subprocess.run([str(arg) for arg in args], cwd=ROOT, env=env,
                            text=True, check=True, capture_output=capture)
    return result.stdout.strip() if capture else ""


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write_json(path: Path, data: object, *, exclusive: bool = False) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("x" if exclusive else "w", encoding="utf-8", newline="\n") as stream:
        json.dump(data, stream, indent=2, sort_keys=True, ensure_ascii=False)
        stream.write("\n")


def workspace_version() -> str:
    return tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]


def source_identity() -> dict:
    try:
        commit = run(["git", "rev-parse", "HEAD"], capture=True)
        epoch = int(run(["git", "show", "-s", "--format=%ct", "HEAD"], capture=True))
    except subprocess.CalledProcessError:
        commit, epoch = None, 0
    dirty = bool(run(["git", "status", "--porcelain", "--untracked-files=normal"], capture=True))
    return {"commit": commit, "dirty": dirty, "source_date_epoch": epoch}


def packager() -> str:
    executable = os.environ.get("WORKBENCH_PACKAGER", "cargo-packager")
    actual = run([executable, "--version"], capture=True)
    if actual.split()[-1] != PACKAGER_VERSION:
        raise ValueError(f"Install cargo-packager {PACKAGER_VERSION}; found another version")
    return executable


def build_env(epoch: int, policy: Path, target: str | None = None) -> dict:
    env = os.environ.copy()
    env.pop("CARGO_PACKAGER_SIGN_PRIVATE_KEY", None)
    env.pop("CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD", None)
    env["SOURCE_DATE_EPOCH"] = str(epoch)
    env["WORKBENCH_UPDATE_POLICY"] = str(policy.resolve())
    env["CARGO_INCREMENTAL"] = "0"
    # Match Cargo's environment precedence. RUSTFLAGS uses whitespace splitting,
    # not shell quoting; encoded arguments retain spaces/backslashes verbatim.
    if "CARGO_ENCODED_RUSTFLAGS" in env:
        flags = env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f") if env["CARGO_ENCODED_RUSTFLAGS"] else []
    elif "RUSTFLAGS" in env:
        flags = env["RUSTFLAGS"].split()
    else:
        target_flags = env.get("CARGO_TARGET_" + target.upper().replace("-", "_") + "_RUSTFLAGS", "") if target else ""
        flags = target_flags.split()
    if target and target.endswith("-pc-windows-msvc"):
        # Do not silently override an explicitly requested dynamic CRT. The
        # post-build PE import gate checks the actual native linker result too.
        for index, flag in enumerate(flags):
            codegen = flags[index + 1] if flag == "-C" and index + 1 < len(flags) else flag.removeprefix("-C")
            if codegen.startswith("target-feature=") and "-crt-static" in codegen.split("=", 1)[1].split(","):
                raise ValueError("Windows release builds require a static CRT; remove the -crt-static override")
        text = " ".join(flags).lower()
        dynamic = r"(?:^|[\s=])[-/]mdd?(?:\s|$)|[-/]defaultlib:(?:msvcrt|msvcprt|vcruntime|msvcp|concrt)|[-/]nodefaultlib:libcmtd?(?:\.lib)?(?:\s|$)"
        if re.search(dynamic, text):
            raise ValueError("Windows release flags explicitly select or require a dynamic CRT")
    remap = f"--remap-path-prefix={ROOT}=/workbench"
    flags.append(remap)
    if target and target.endswith("-pc-windows-msvc"):
        flags.append("-Ctarget-feature=+crt-static")
    env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
    if os.name != "nt":
        env["TZ"] = "UTC"
    return env
