#!/usr/bin/env python3
"""Package the committed Xylo artwork using the explicitly built native tool.

This development script never builds dependencies or manipulates image pixels.
Build once with:
    cargo build --locked -p agent-workbench --example prepare_brand_icons
"""
from __future__ import annotations

import argparse
import hashlib
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
MASTER = ROOT / "packaging/branding/xylo-mark.png"
DEFAULT_TOOL = ROOT / "target/debug/examples/prepare_brand_icons"


def _verified_tool(tool: Path) -> Path:
    tool = tool.resolve()
    if not tool.is_file():
        raise RuntimeError(
            "Build the native icon converter first: "
            "cargo build --locked -p agent-workbench --example prepare_brand_icons. "
            "Use --tool to select another explicitly built converter."
        )
    embedded_hash = subprocess.run(
        [str(tool), "--master-sha256"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        timeout=10,
    ).stdout.decode("ascii").strip()
    if embedded_hash != hashlib.sha256(MASTER.read_bytes()).hexdigest():
        raise RuntimeError("The converter contains older artwork; rebuild it before packaging.")
    return tool


def png(size: int) -> bytes:
    """Return a faithful PNG derivative through the prebuilt native converter."""
    if not isinstance(size, int) or not 1 <= size <= 2048:
        raise ValueError("Icon size must be between 1 and 2048 pixels")
    return subprocess.run(
        [str(_verified_tool(DEFAULT_TOOL)), "--png", str(size)],
        cwd=ROOT,
        check=True,
        capture_output=True,
        timeout=60,
    ).stdout


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tool", type=Path, default=DEFAULT_TOOL)
    parser.add_argument("--output", type=Path, default=ROOT / "packaging/icons")
    args = parser.parse_args()
    subprocess.run(
        [str(_verified_tool(args.tool)), "--output", str(args.output.resolve())],
        cwd=ROOT,
        check=True,
        timeout=60,
    )


if __name__ == "__main__":
    main()
