#!/usr/bin/env python3
"""Independently postvalidate Xylo evidence; publish only with explicit --publish.

Successor of record-completion-validation.py. Original producer scripts and gate
headers remain unchanged; this file supplies a separately hashed stricter
postvalidation policy, never a claim that it produced the passed gate logs.

No Cargo, benchmark, package generation or device operation is run here. The
read-only default checks gate proofs, source, existing bundle/notice bytes and
captures. It plans all outputs before writing; build-validation.json is last.
Earlier optimized app artifacts and measurements remain historical identities.
"""
from __future__ import annotations

import argparse
from collections import Counter
from datetime import datetime, timezone
import json
from pathlib import Path
import plistlib
import re
import shlex
import subprocess
import sys
import tomllib
sys.dont_write_bytecode = True
from completion_validation_common import *


AFAF_SOURCE = "afaf0c68703cc9d0a1a496eab07f0f1c87e2c5a81e205f89c7180cb48d5408b4"
AFAF_RUN = WORK / "completion-runs/afaf0c68703cc9d0-1792921760203577000"
AFAF_STAGE_NAMES = (
    "format", "native-format", "windows-format", "windows-workspace-format",
    "sdk-format", "preview-build", "workspace-tests", "native-tests", "native-render", "clippy",
)
# Explicitly reviewed original bytes; never regenerate these from a later run.
AFAF_FILES = {
    "captures/document-recovery.json": {
        "sha256": "29b39e80bc61ddd95848fbf563ba9ec65ac35e6c7fd782fd357217661835390c",
        "bytes": 1560
    },
    "captures/document-recovery.png": {
        "sha256": "ffae6c1f3e663d1612678e9ea800d854c368ca26539294506dbb0f9160be0d29",
        "bytes": 404925
    },
    "captures/environments.png": {
        "sha256": "61a97ac2fade2be38b75fefcc5993746e060872bd391228738d7f7054dab2cee",
        "bytes": 378487
    },
    "captures/home.png": {
        "sha256": "abf54dc29ea2d3fcc180638a0d9ea83eaaaff3f4493afb86c7a6a35b02bee149",
        "bytes": 378454
    },
    "captures/plugins.png": {
        "sha256": "e2be374164dade0729023076559d60fe05be70ac2062cf3fbf8b8fa2af471471",
        "bytes": 240327
    },
    "captures/recovery.json": {
        "sha256": "7634e255cc373e6c6ecbffee2245b05e2610961dbba9167d9bfaeae0bfa10e2b",
        "bytes": 1040
    },
    "captures/recovery.png": {
        "sha256": "3fde76bf151170135bf2a2985339da89032705c52991ca280ef1ad58c77f64d5",
        "bytes": 412949
    },
    "captures/settings.png": {
        "sha256": "961786deeb3240f0e1b42c01fead6198dac3ad6277ab283cc95b9df9e319810a",
        "bytes": 364234
    },
    "clippy.log": {
        "sha256": "802b14aeed34648bf0541fc7b5f038483a7f452c9cd807f7964429cb3da1b6be",
        "bytes": 4241
    },
    "clippy.result.json": {
        "sha256": "174c302ff93231e5d66ada5cf69b373d2bceff46b1b68e53fa420018058befb8",
        "bytes": 441
    },
    "format.log": {
        "sha256": "6c7bde0931ff5b2dec12ace46d9bd5c8335f44029e06226bb1adfdf91970d0d6",
        "bytes": 966
    },
    "format.result.json": {
        "sha256": "8c760bdf4ec71af6d3cdb5add6b8e45d3e1a3a4f350a2f626cf94388c6321717",
        "bytes": 437
    },
    "native-format.log": {
        "sha256": "d00f6f65dad704eeb0bc092e5b810d203696d5bbb7e233a2ad7237fbc4e62652",
        "bytes": 1025
    },
    "native-format.result.json": {
        "sha256": "e35b7b79d5ea1ed70ac442186cc5a41d1f73baecf8207b688ff4839628313f5f",
        "bytes": 452
    },
    "native-render.log": {
        "sha256": "2b94e7e5f1361602f985125f9620ca10c3f22c7955d76c17f1f06615ec316621",
        "bytes": 2580
    },
    "native-render.result.json": {
        "sha256": "c26a1435f634f1dab2702ca78ea711d8d2bcf372cd960fe559d5a9a1344f4578",
        "bytes": 453
    },
    "native-tests.log": {
        "sha256": "b55f044fb6946e728bd47b781493c6354ef2b716cb7ee9994f73c9f4f4097e6d",
        "bytes": 18107
    },
    "native-tests.result.json": {
        "sha256": "a39ad48e14cf82c9028c6a1e791e7663732b00f202a1ddfc2a327ecdc7df45b4",
        "bytes": 451
    },
    "preview-build.log": {
        "sha256": "fdc2861663b05274944fcc13ca4b5e1b134b9244e1779e5112c6a87915c74371",
        "bytes": 4042
    },
    "preview-build.result.json": {
        "sha256": "43fb8e1d5e6abcefbb790ad3dcb0163dce5178e43305f28f8249a6a30cb20dae",
        "bytes": 453
    },
    "run.json": {
        "sha256": "21fd956ae46072030238b5ec65e9eebf3c7521e18d241446ac5cedbbdf6aed01",
        "bytes": 1687
    },
    "sdk-format.log": {
        "sha256": "bea9c0a658fc7b879f3cf4a0890eb51a4e0aacbabbb53d4191b5129d4e84c992",
        "bytes": 1012
    },
    "sdk-format.result.json": {
        "sha256": "8780d4d292c2b89c91c79cbbb71096ce89b14426eaeaf80631a05300294df720",
        "bytes": 446
    },
    "source-manifest.json": {
        "sha256": "946a49d64a469401a7e3d92e5af5e0196142f62784fa99ad0a20b721fdb8b355",
        "bytes": 234885
    },
    "windows-format.log": {
        "sha256": "07473c98ae441d1ab4feef7a2f7aa30acbd6ff755520a60609befb188f27c06e",
        "bytes": 1034
    },
    "windows-format.result.json": {
        "sha256": "2e78154ef6191c8b9d7ab6ad794c19ed55e04de3f1bf52a2b74eeb9cd0de3139",
        "bytes": 454
    },
    "windows-workspace-format.log": {
        "sha256": "52618382b973fa907e7e15b7eefa31ef6a0d86bb282f26ce062f1c237812d56d",
        "bytes": 1035
    },
    "windows-workspace-format.result.json": {
        "sha256": "47795bc1a530f0f8972ffc339d453d6e7358dc806162e134a631d96e88088613",
        "bytes": 474
    },
    "workspace-tests.log": {
        "sha256": "fe572d64ba43ef0b7d7d9bbd0c45efb48c87076be52f40b8f08d9489be6343f3",
        "bytes": 53629
    },
    "workspace-tests.result.json": {
        "sha256": "cea59bf9fa991ee8d3d83e66ad8a7473aba3d7547a86bd8d4ab664ee14bbb62e",
        "bytes": 458
    }
}


def afaf_lint_attempt(plan: Plan) -> dict:
    """Queue exact history of a source-verified lint failure, never current totals."""
    run = AFAF_RUN
    require(not run.is_symlink() and run.resolve().parent == (WORK / "completion-runs").resolve(),
            "Historical lint run escaped its work directory")
    require(not (run / "completed.json").exists(), "Historical lint attempt became complete")
    actual = set()
    for path in run.rglob("*"):
        require(not path.is_symlink(), f"Historical lint input is a symlink: {path}")
        if path.is_file():
            relative = path.relative_to(run).as_posix()
            actual.add(relative)
            require(relative in AFAF_FILES and plan.track(path) == AFAF_FILES[relative]["sha256"]
                    and path.stat().st_size == AFAF_FILES[relative]["bytes"],
                    f"Historical lint attempt bytes changed: {relative}")
    require(actual == set(AFAF_FILES), "Historical lint attempt file inventory changed")
    index = plan.json(run / "run.json")
    manifest = plan.json(run / "source-manifest.json")
    require(index["reviewed_source_fingerprint_sha256"] == AFAF_SOURCE
            and manifest["source_fingerprint_sha256"] == AFAF_SOURCE
            and hashlib.sha256(json.dumps(manifest["files"], sort_keys=True,
                                         separators=(",", ":")).encode()).hexdigest() == AFAF_SOURCE,
            "Historical lint source fingerprint differs")
    require(index["stages"][:len(AFAF_STAGE_NAMES)] == list(AFAF_STAGE_NAMES)
            and len(index["stages"]) > len(AFAF_STAGE_NAMES),
            "Historical lint stage sequence changed")
    old_recorders = index["recorder_sources_sha256"]
    require(set(old_recorders) == {"completion_validation_common.py", "run-completion-gates.py",
                                  "record-completion-validation.py", "record-recovery-validation.py"}
            and all(re.fullmatch(r"[0-9a-f]{64}", value) for value in old_recorders.values()),
            "Historical lint producer inventory is invalid")
    evidence = f"docs/evidence/{DATE}-pre-xylo-lint-attempt"
    stages_record = []
    for name in AFAF_STAGE_NAMES:
        log_path = run / f"{name}.log"
        proof = plan.json(run / f"{name}.result.json")
        text = plan.read(log_path).decode()
        require("\0" not in text and "\x1b" not in text, f"Invalid historical lint log: {name}")
        first, separator, body = text.partition("\n")
        require(separator, f"Historical lint log has no producer header: {name}")
        header = json.loads(first)
        require(header["reviewed_source_fingerprint_sha256"] == AFAF_SOURCE
                and proof["reviewed_source_fingerprint_sha256"] == AFAF_SOURCE
                and header["recorder_sources_sha256"] == old_recorders
                and header["environment"] == index["environment"],
                f"Historical lint header differs from its original run: {name}")
        require(proof["stage"] == name and proof["log"] == str(log_path)
                and proof["log_sha256"] == AFAF_FILES[f"{name}.log"]["sha256"]
                and proof["source_verified_after"] is True and proof["runner_failure"] is None
                and header["started_at_unix"] >= manifest["frozen_at_unix"] - 2
                and isinstance(proof["seconds"], (float, int)) and 0 <= proof["seconds"] < 86400,
                f"Historical lint proof/source binding differs: {name}")
        require(not re.search(r"^test \S+ \.\.\. FAILED$|^test result: FAILED\.", body, re.MULTILINE),
                f"Historical lint attempt unexpectedly contains a functional test failure: {name}")
        if name == "clippy":
            require(proof["exit_code"] == 101 and "chunks_exact_to_as_chunks" in body
                    and "crates/desktop/src/ui.rs:23:33" in body,
                    "Historical lint failure has a different cause")
            status = "failed_strict_lint_for_historical_source_only"
        else:
            require(proof["exit_code"] == 0, f"Historical completed stage failed: {name}")
            status = "completed_for_historical_source_only"
        stages_record.append(dict(name=name, status=status, exit_code=proof["exit_code"],
            seconds=proof["seconds"], source_verified_after=proof["source_verified_after"],
            command=header["command"], log=f"{evidence}/{name}.log", log_sha256=proof["log_sha256"],
            proof=f"{evidence}/{name}.result.json",
            proof_sha256=AFAF_FILES[f"{name}.result.json"]["sha256"]))
    # Completed offscreen captures retain the old compiled-source map and their
    # original bytes. They never become captures of the later fixed source.
    for name, mapping in CAPTURE_SOURCES.items():
        metadata = plan.json(run / "captures" / f"{name}.json")
        image = run / "captures" / f"{name}.png"
        require(metadata["file"] == image.name and metadata["sha256"] == plan.track(image)
                and metadata["pixel_size"] == legacy.png(plan, image)
                and metadata["compiled_sources_sha256"] == {
                    label: manifest["files"][path] for label, path in mapping.items()},
                f"Historical capture lost its original source/image identity: {name}")
    for relative in AFAF_FILES:
        plan.copy(run / relative, ROOT / evidence / relative, immutable=True)
    failed = [stage for stage in stages_record if stage["exit_code"] != 0]
    require(len(failed) == 1 and failed[0]["name"] == "clippy", "Historical lint failure set differs")
    value = dict(schema_version=1, classification="historical_incomplete_attempt",
        failure_kind="strict_lint_failure", source_fingerprint_sha256=AFAF_SOURCE,
        original_run=str(run), source_manifest=f"{evidence}/source-manifest.json",
        source_manifest_sha256=AFAF_FILES["source-manifest.json"]["sha256"],
        run_index=f"{evidence}/run.json", run_index_sha256=AFAF_FILES["run.json"]["sha256"],
        archived_recorder_sources_sha256=old_recorders, stages=stages_record,
        completed_stage_count=sum(stage["exit_code"] == 0 for stage in stages_record),
        failed_stage="clippy", failed_exit_code=failed[0]["exit_code"],
        planned_but_not_started=index["stages"][len(AFAF_STAGE_NAMES):], files=AFAF_FILES,
        reason=("This source-bound attempt stopped at strict Clippy: the new logo conversion used "
                "chunks_exact_mut(4), triggering chunks_exact_to_as_chunks. The first nine stages, "
                "including workspace tests, native tests and native rendering, completed for this old "
                "source. The tenth stage returned exit code 101 with source_verified_after=true. "
                "This was a lint failure, not an operator interruption or a recorded functional-test "
                "assertion failure."),
        counts_scope="Historical source only. No completed stage, test count, duration or capture is reused as acceptance or performance evidence for the next source.",
        evidence_scope="Exact original source map, run index, ten log/proof pairs and eight capture files. Producer headers are validated against that archived run index, never relabelled with successor hashes.")
    plan.write(ROOT / evidence / "attempt.json", value, immutable=True)
    return dict(source_fingerprint_sha256=AFAF_SOURCE, record=f"{evidence}/attempt.json",
        failure_kind=value["failure_kind"], run_index_sha256=value["run_index_sha256"],
        source_manifest_sha256=value["source_manifest_sha256"], completed_stage_count=value["completed_stage_count"],
        failed_stage=value["failed_stage"], failed_exit_code=value["failed_exit_code"],
        scope=value["counts_scope"], reason=value["reason"])


def local_git_identity() -> dict:
    """Read local source identity only; never query a remote or infer push parity."""
    def git(*args):
        return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True,
                              check=False, timeout=30)
    commit = git("rev-parse", "--verify", "HEAD")
    require(commit.returncode == 0 and re.fullmatch(r"[0-9a-f]{40}", commit.stdout.strip()),
            "A local committed Git identity is required for this Xylo report")
    branch = git("symbolic-ref", "--quiet", "--short", "HEAD")
    require(branch.returncode in (0, 1), "Could not determine the local Git branch")
    status = git("status", "--porcelain=v1", "--untracked-files=normal")
    require(status.returncode == 0, "Could not determine the local source worktree state")
    return dict(head=commit.stdout.strip(), branch=branch.stdout.strip() if branch.returncode == 0 else None,
                worktree_clean_before_documentation_publication=not status.stdout,
                worktree_status_sha256=hashlib.sha256(status.stdout.encode()).hexdigest(),
                remote_publication_or_parity="not checked by this postprocessor")


def recording_policy(implementation_sha256: str) -> dict:
    """Identify this successor separately from the unchanged gate producers."""
    return {
        "role": "Independent postvalidation policy; not a gate producer",
        "source_path": str(Path(__file__).resolve()),
        "sha256": implementation_sha256,
        "original_gate_producer_scripts_sha256": recorder_sources(),
        "policies": {
            "current_bundle_notices": "strict schema-2 validation with frozen-catalogue binding",
            "historical_evidence": "exact original bytes and embedded identities retained",
            "gate_headers_relabelled": False,
            "gates_rerun_by_postprocessor": False,
            "legal_approval_claimed": False,
        },
    }


def bundle_evidence(plan: Plan, gates: dict, manifest: dict) -> dict:
    bundle = ROOT / CURRENT_PREVIEW_BUNDLE
    subprocess.run(["codesign", "--verify", "--deep", "--strict", str(bundle)],
                   check=True, capture_output=True, timeout=120)
    subprocess.run(["plutil", "-lint", str(bundle / "Contents/Info.plist")],
                   check=True, capture_output=True, timeout=30)
    info = plistlib.loads(plan.read(bundle / "Contents/Info.plist"))
    require(info.get("CFBundleIdentifier") == CURRENT_PREVIEW_IDENTIFIER
            and info.get("CFBundleName") == "Xylo"
            and info.get("CFBundleDisplayName") == "Xylo",
            "Current preview bundle has a different product name or preview identity")
    components = {name: legacy.binary(plan, bundle / "Contents/MacOS" / name)
                  for name in legacy.BINARIES}
    debug = {name: legacy.binary(plan, ROOT / "target/debug" / name) for name in legacy.BINARIES}
    for name in legacy.BINARIES:
        require((bundle / "Contents/MacOS" / name).stat().st_mtime
                >= gates["preview-build"]["header"]["started_at_unix"] - 2,
                f"Bundle component predates current source-bound build: {name}")
    notices = bundle / "Contents/Resources/licenses/dependencies"
    sys.path.insert(0, str(ROOT / "scripts"))
    from release_metadata import validate_inventory
    validate_inventory(notices, strict=True)  # Existing current bytes only; history is archived separately.
    inventory = plan.json(notices / "licenses.json")
    provenance = plan.json(notices / "build-provenance.json")
    sbom = plan.json(notices / "sbom.cdx.json")
    supplements = plan.json(notices / "license-supplements.json")
    catalogue_source = "packaging/licenses/supplements.json"
    catalogue_sha256 = manifest["files"][catalogue_source]
    require(plan.track(ROOT / catalogue_source) == catalogue_sha256
            and inventory["supplements"]["schema_version"] == 2
            and supplements["schema_version"] == 2
            and inventory["supplements"]["catalogue_sha256"] == catalogue_sha256
            and supplements["catalogue_sha256"] == catalogue_sha256,
            "Current notice schema/catalogue differs from frozen canonical/upstream policy")
    for validator in ("scripts/release_licenses.py", "scripts/release_metadata.py"):
        require(plan.track(ROOT / validator) == manifest["files"][validator],
                "Current notice validator differs from frozen source")
    require(provenance["cargo_lock_sha256"] == manifest["files"]["Cargo.lock"]
            and provenance["target"] == "aarch64-apple-darwin", "Bundle inventory source/target differs")
    lock_packages = tomllib.loads(plan.read(ROOT / "Cargo.lock").decode())["package"]
    require(set((p["name"], p["version"]) for p in inventory["packages"]).issubset(
        {(p["name"], p["version"]) for p in lock_packages}), "Inventory package absent from frozen lock")
    require(Counter((p["name"], p["version"]) for p in inventory["packages"])
            == Counter((p["name"], p["version"]) for p in sbom["components"]), "SBOM/inventory differ")
    for package in inventory["packages"]:
        if package["archive_sha256"]:
            require(any(item.get("checksum") == package["archive_sha256"] for item in lock_packages
                        if (item["name"], item["version"]) == (package["name"], package["version"])),
                    "Inventory archive checksum differs from frozen lock")
        for item in package["texts"]:
            path = notices / item["path"]
            require(path.resolve().is_relative_to(notices.resolve()) and plan.track(path) == item["sha256"],
                    "Bundled dependency notice differs")
            plan.copy(path, ROOT / "docs/evidence" / item["path"])
    for name, destination in (("licenses.json", "dependency-license-inventory.json"),
                               ("sbom.cdx.json", "sbom.cdx.json"),
                               ("build-provenance.json", "build-provenance.json"),
                               ("license-supplements.json", "license-supplements.json")):
        plan.copy(notices / name, ROOT / "docs/evidence" / destination)
    assets = []
    asset_manifests = sorted((bundle / "Contents/Resources/native-assets").glob("*/manifest.json"))
    require(asset_manifests, "Preview omitted bundled native asset manifests")
    for path in asset_manifests:
        package = plan.json(path)
        for item in package["files"]:
            asset = path.parent / item["path"]
            require(asset.resolve().is_relative_to(path.parent.resolve()) and
                    plan.track(asset) == item["sha256"] and asset.stat().st_size == item["size_bytes"],
                    "Bundled native asset differs from its manifest")
            if asset.stat().st_mode & 0o111:
                subprocess.run(["codesign", "--verify", "--strict", str(asset)],
                               check=True, capture_output=True, timeout=30)
            assets.append(dict(package=package["asset_id"], path=item["path"],
                               sha256=item["sha256"], bytes=item["size_bytes"]))
    return dict(bundle=CURRENT_PREVIEW_BUNDLE, bundle_identifier=info["CFBundleIdentifier"],
                bundle_display_name=info["CFBundleDisplayName"], components=components, debug_artifacts=debug,
                codesign_deep_strict_verification="passed", verified_bundled_entries=assets,
                native_asset_packages=len(asset_manifests),
                bundled_asset_manifest_entries_verified=len(assets),
                inventory=inventory, provenance=provenance, supplements=supplements)


def captures_evidence(plan: Plan, run: Path, gate: dict, manifest: dict) -> list[dict]:
    directory = run / "captures"
    require({p.name for p in directory.glob("*.png")} >= {
        "home.png", "settings.png", "plugins.png", "environments.png", "recovery.png",
        "document-recovery.png"},
        "Native rendering did not produce all required captures")
    captures = []
    for path in sorted(directory.glob("*.png")):
        require(path.stat().st_mtime >= gate["header"]["started_at_unix"] - 2,
                "Native capture predates this render gate")
        item = dict(path=f"docs/native-preview/{path.name}", pixel_size=legacy.png(plan, path),
                    sha256=plan.track(path))
        captures.append(item)
        plan.copy(path, ROOT / item["path"])
        plan.copy(path, ROOT / f"docs/evidence/{DATE}-completion-{path.name}")
    for name, mapping in capture_source_maps(plan).items():
        metadata_path = directory / f"{name}.json"
        require(metadata_path.stat().st_mtime >= gate["header"]["started_at_unix"] - 2,
                f"Capture metadata predates this render gate: {name}")
        metadata = plan.json(metadata_path)
        image = next(item for item in captures if item["path"].endswith(f"/{name}.png"))
        require(metadata["file"] == f"{name}.png" and metadata["sha256"] == image["sha256"]
                and metadata["pixel_size"] == image["pixel_size"]
                and metadata["logical_size"] == [1380, 880],
                f"Capture metadata/image dimensions or bytes differ: {name}")
        require(metadata["compiled_sources_sha256"] == {
            label: manifest["files"][source] for label, source in mapping.items()},
            f"Renderer compiled different reviewed UI/fixture/service/lock bytes: {name}")
        image["metadata"] = f"docs/native-preview/{name}.json"
        image["metadata_sha256"] = plan.track(metadata_path)
        plan.copy(metadata_path, ROOT / image["metadata"])
        evidence_name = "rendering" if name == "recovery" else f"{name}-rendering"
        plan.copy(metadata_path, ROOT / f"docs/evidence/{DATE}-completion-{evidence_name}.json")
    return captures


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-dir", type=Path)
    parser.add_argument("--expected-source")
    parser.add_argument("--expected-implementation", help="Reviewed SHA256 of this independent successor")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--publish", action="store_true")
    mode.add_argument("--plan-only", action="store_true", help="Inspect inputs only; no gate validation, signing checks or publication")
    args = parser.parse_args()
    implementation_path = Path(__file__).resolve()
    implementation_sha256 = digest(implementation_path)
    if args.plan_only:
        print(json.dumps(dict(mode="plan-only", stages=dict(stages()), review_inputs=review_plan(),
                              recording_implementation=recording_policy(implementation_sha256),
                              historical_failed_lint_attempt=dict(source=AFAF_SOURCE,
                                  run=str(AFAF_RUN), pinned_original_files=len(AFAF_FILES),
                                  stage_names=list(AFAF_STAGE_NAMES))), indent=2))
        return
    require(args.run_dir is not None and args.expected_source,
            "Supply --run-dir and --expected-source to validate existing gates")
    plan = Plan(ROOT)
    require(args.expected_implementation == implementation_sha256,
            "Supply the exact reviewed successor SHA256 with --expected-implementation")
    require(plan.track(implementation_path) == implementation_sha256,
            "Independent postvalidation implementation changed")
    manifest = plan.json(MANIFEST)
    fingerprint = manifest["source_fingerprint_sha256"]
    local_git = local_git_identity()
    require(args.expected_source == fingerprint and fingerprint != PREVIOUS,
            "Expected newly approved source fingerprint required")
    assert_source(manifest)
    run = args.run_dir.resolve()
    require(run.is_relative_to((WORK / "completion-runs").resolve()), "Run directory escapes work evidence")
    run_index = plan.json(run / "run.json")
    require(run_index["reviewed_source_fingerprint_sha256"] == fingerprint
            and run_index["recorder_sources_sha256"] == recorder_sources(),
            "Run used another source or recorder implementation")
    require(plan.json(run / "source-manifest.json") == manifest, "Run's frozen source map differs")
    completed = plan.json(run / "completed.json")
    expected = stages()
    require(completed == {"source": fingerprint, "stages": len(expected)}
            and run_index["stages"] == [name for name, _ in expected], "Run did not finish every required stage")
    gates = {name: gate(plan, run, name, fingerprint, command) for name, command in expected}
    require(all(result["header"]["started_at_unix"] >= manifest["frozen_at_unix"] - 2
                for result in gates.values()), "A gate predates the current frozen manifest")
    for name in ("format", "native-format", "windows-format", "windows-workspace-format", "sdk-format"):
        require(not gates[name]["body"].strip(), f"Formatting emitted changes/errors: {name}")
    for name in ("clippy", "native-clippy", "windows-check", "windows-clippy",
                 "windows-workspace-check", "windows-workspace-clippy"):
        require("Finished `dev`" in gates[name]["body"] and not re.search(
            r"^error(?:\[|:)", gates[name]["body"], re.MULTILINE), f"Check/lint did not finish: {name}")
    require("Built:" in gates["preview-build"]["body"], "Preview bundle did not complete")
    groups, renderers = targets()
    workspace = legacy.result_counts(gates["workspace-tests"]["body"], "workspace")
    workspace_sections = legacy.test_sections(gates["workspace-tests"]["body"], "workspace")
    native = legacy.native_counts(gates["native-tests"]["body"], groups)
    for renderer in renderers:
        require(any(Path(match.group(1)).name.startswith(renderer.replace("-", "_") + "-")
                    for match in legacy.RUNNING.finditer(gates["native-render"]["body"])),
                f"Native renderer did not run: {renderer}")
    release = gates["release-tests"]["body"]
    python_inventory = python_test_inventory(plan, manifest)
    python_count = python_inventory["count"]
    require(re.findall(r"^Ran (\d+) tests? in .+$", release, re.MULTILINE) == [str(python_count)]
            and re.search(r"^OK$", release, re.MULTILINE),
            "Python release-script results differ from the frozen source inventory")
    sdk = sdk_evidence(plan, gates["sdk-script"]["body"], gates["sdk-script"]["header"]["started_at_unix"])

    # Validate the pre-overwrite snapshot, including the original failed 12e3
    # source/proof and the unbound SDK development collision, before replacing
    # a single canonical log, capture, inventory file or report.
    archive_index_path = Path(run_index["archive"]).resolve()
    require(archive_index_path == WORK / "completion-history" / PREVIOUS / "archive.json"
            and plan.track(archive_index_path) == run_index["archive_sha256"], "Historical archive identity differs")
    archive = plan.json(archive_index_path)
    require(archive["owner_report_source_fingerprint_sha256"] == PREVIOUS, "Wrong owner report in archive")
    historical_root = archive_index_path.parent
    archive_relative = f"docs/evidence/{DATE}-pre-completion-recovery"
    for relative, item in archive["files"].items():
        path = historical_root / relative
        require(path.resolve().is_relative_to(historical_root.resolve())
                and plan.track(path) == item["sha256"] and path.stat().st_size == item["bytes"],
                "Historical snapshot file changed")
        plan.copy(path, ROOT / archive_relative / relative, immutable=True)
    plan.copy(archive_index_path, ROOT / archive_relative / "archive.json", immutable=True)
    previous = plan.json(historical_root / "docs/build-validation.json")
    require(previous["source_fingerprint_sha256"] == PREVIOUS, "Archived prior report source differs")
    current = plan.json(ROOT / "docs/build-validation.json")
    require(current["source_fingerprint_sha256"] in (PREVIOUS, fingerprint), "Canonical report changed to another milestone")
    # Do not rewrite accepted historical source identities inside this note.
    initial = legacy.initial_recovery_failure(plan, ROOT / "docs/evidence", PREVIOUS)
    require(initial is not None, "Original initial recovery failure must remain preserved")
    pre_cache_attempt = pre_cache_format_attempt(plan)
    prior_lint_attempt = afaf_lint_attempt(plan)

    package = bundle_evidence(plan, gates, manifest)
    captures = captures_evidence(plan, run, gates["native-render"], manifest)
    source_relative = f"docs/evidence/{DATE}-completion-source-manifest.json"
    plan.copy(MANIFEST, ROOT / source_relative)
    plan.copy(MANIFEST, ROOT / "docs/evidence/source-manifest.json")
    for name, result in gates.items():
        target = "native-ui-tests" if name == "native-tests" else name
        for suffix, source in ((".log", result["path"]), (".result.json", result["proof_path"])):
            plan.copy(source, ROOT / f"docs/evidence/{target}{suffix}")
            plan.copy(source, ROOT / f"docs/evidence/{DATE}-completion-{target}{suffix}")
    sdk_relative = f"docs/evidence/{DATE}-completion-sdk-validation.json"
    plan.copy(sdk["validation"], ROOT / sdk_relative)
    sdk["validation"] = sdk_relative
    sdk.pop("package")  # Hashes describe it; no developer binary/package is redistributed.
    windows = windows_inventories(plan, manifest)

    record = dict(previous)
    record.pop("current_worktree_note", None)
    record.update(recorded_at=datetime.now(timezone.utc).isoformat(),
                  source_fingerprint_sha256=fingerprint, source_manifest=source_relative)
    state = "clean" if local_git["worktree_clean_before_documentation_publication"] else "dirty"
    branch = local_git["branch"] or "detached HEAD"
    record["source_state"] = (
        f"Local Git HEAD {local_git['head']} ({branch}); {state} worktree before documentation "
        "publication. Frozen source inputs verified; remote publication/parity was not checked "
        "by this postprocessor.")
    record["local_source_binding"] = dict(local_git,
        source_fingerprint_sha256=fingerprint,
        bundled_build_git_head=package["provenance"].get("commit"),
        scope="Local HEAD/worktree snapshot rechecked before report publication; the bundle's build identity remains separate.")
    record["platform_metadata_scope"] = (
        "Host fields are retained historical metadata. Current build toolchain/SDK provenance is "
        "recorded in docs/evidence/build-provenance.json; no new hardware measurement is claimed.")
    implementation_relative = f"docs/evidence/{DATE}-xylo-postvalidation-{implementation_sha256[:16]}.py"
    plan.copy(implementation_path, ROOT / implementation_relative, immutable=True)
    record["recording_implementation"] = dict(recording_policy(implementation_sha256),
        archived_source=implementation_relative,
        reviewed_source_fingerprint_sha256=fingerprint,
        original_gate_run_index=str(run / "run.json"),
        original_gate_run_index_sha256=plan.track(run / "run.json"))
    record["historical_validation"] = {
        "source_fingerprint_sha256": PREVIOUS,
        "report": archive_relative + "/docs/build-validation.json",
        "archive_manifest": archive_relative + "/archive.json",
        "archive_manifest_sha256": plan.track(archive_index_path),
        "scope": "Exact prior report/log/capture/notice bytes. Embedded 3c/497a/plugin/document/semantic/worker measurement identities are historical and unchanged."}
    record["historical_incomplete_attempts"] = [pre_cache_attempt, prior_lint_attempt]
    record["workspace_tests"] = dict(**workspace, command=shlex.join(dict(expected)["workspace-tests"]),
        evidence="docs/evidence/workspace-tests.log", executable_groups={
            name: {key: value for key, value in section.items() if not key.endswith("_names")}
            for name, section in workspace_sections.items()})
    record["native_ui"] = dict(**native, command=shlex.join(dict(expected)["native-tests"]),
        evidence="docs/evidence/native-ui-tests.log", screenshots=[item["path"] for item in captures],
        capture_evidence=captures, recovery_capture_metadata="docs/native-preview/recovery.json",
        document_recovery_capture_metadata="docs/native-preview/document-recovery.json",
        note="Counts derive from named current gate logs; shared cases are counted per executable. Captures are offscreen GPU fixtures, not physical OS input/device/accessibility acceptance.")
    record["gate_proofs"] = {name: {"log": f"docs/evidence/{DATE}-completion-{'native-ui-tests' if name == 'native-tests' else name}.log",
        "proof": f"docs/evidence/{DATE}-completion-{'native-ui-tests' if name == 'native-tests' else name}.result.json",
        "log_sha256": result["proof"]["log_sha256"], "seconds": result["proof"]["seconds"]}
        for name, result in gates.items()}
    record["lint"] = dict(status="passed", evidence="docs/evidence/clippy.log")
    record["format"] = dict(status="passed", stages=["format", "native-format", "windows-format", "windows-workspace-format", "sdk-format"])
    record["plugin_sdk"] = sdk
    record.update(windows)
    record["package"] = dict(previous["package"], source_fingerprint_sha256=fingerprint,
        **{key: value for key, value in package.items() if key not in ("inventory", "provenance", "supplements")})
    record["distribution"] = dict(previous["distribution"], release_script_tests=python_count,
        release_script_inventory=python_inventory,
        platform_inventory_components=len(package["inventory"]["packages"]),
        missing_upstream_license_texts=len(package["inventory"]["missing_texts"]),
        verified_supplemented_packages=package["supplements"]["applied_packages"],
        available_license_texts=(
            f"docs/evidence/licenses with schema-2 hashed upstream/canonical provenance; "
            f"{len(package['inventory']['missing_texts'])} recorded exact-version full-text gaps; "
            "canonical standards are distinct from package upstream license files"),
        notice_provenance_schema=package["supplements"]["schema_version"],
        notice_catalogue_sha256=package["supplements"]["catalogue_sha256"],
        notice_validation="strict copied-byte/grant/required-notice validation; not legal approval",
        inventory_source_cargo_lock_sha256=package["provenance"]["cargo_lock_sha256"])
    record["historical_release_build"] = previous["release_build"]
    record["release_build"] = {"status": "not_run_for_this_source", "source_fingerprint_sha256": fingerprint,
        "note": "Only current debug siblings/bundle were rebuilt and checked. Prior optimized artifacts remain historical."}
    record["performance"] = dict(previous["performance"],
        current_source_measurements="No controlled or optimized benchmark in this milestone; developer GUI observations are separately scoped", scope="All inherited performance reports retain their original sources/binaries. No current optimized build, new benchmark, Rust-versus-Python speedup or full-app performance acceptance is claimed. Any later live-GUI observations are separate exploratory evidence and do not relabel these historical measurements.")
    record["capability_fixtures"] = {
        "scope": "Current namespace counts from actual current workspace logs; prior detailed counts remain in historical_validation.report.",
        "workspace_namespaces": {name: dict(Counter(test.split("::", 1)[0] for test in section["passed_names"]))
                                 for name, section in workspace_sections.items()}}
    record["speech"] = dict(previous["speech"], historical_component_evidence=True, rerun_in_current_milestone=False)
    record["validation_limitations"] = dict(previous["validation_limitations"],
        initial_recovery_run=initial,
        pre_cache_format_attempt=pre_cache_attempt,
        prior_strict_lint_attempt=prior_lint_attempt,
        initial_sdk_development_run=dict(archive["sdk_development_failure"],
            evidence=archive_relative + "/development/plugin-sdk-development.log",
            sha256=archive["files"]["development/plugin-sdk-development.log"]["sha256"]),
        current_scope="Local frozen-source gates and controlled fixtures only. Initial 12e3/SDK development failures, the pre-cache operator stop and the prior strict-lint attempt remain separate historical evidence, never included in current passed/failed totals.")
    record["full_product_acceptance"] = "Development preview. Current source-bound local gates, debug bundle integrity and standalone SDK checks are recorded. Live OS/account/device behavior, full-app performance/quality, native/model/SDK redistribution obligations and publisher signing remain separate; consult docs/completion-audit.md. Colibri remains deferred."
    report = ROOT / "docs/build-validation.json"
    plan.write(report, record)
    assert_source(manifest)
    require(recorder_sources() == run_index["recorder_sources_sha256"], "Recorder changed before publication")
    require(local_git_identity() == local_git, "Local Git identity/worktree changed during postvalidation")
    plan.recheck()
    summary = dict(mode="publish" if args.publish else "validated-only", source=fingerprint,
        workspace=workspace, native={key: native[key] for key in ("passed", "failed", "ignored", "distinct_flows", "shared_cases_per_executable", "executable_count")},
        sdk={key: sdk[key] for key in ("unit_executions", "doctest_executions", "installed_host_flow_checks")},
        python_tests=python_count, recording_implementation_sha256=implementation_sha256,
        local_git_head=local_git["head"], local_worktree_clean=local_git["worktree_clean_before_documentation_publication"],
        original_gate_producers_unchanged=True, optimized_application_build=False, new_benchmarks=False,
        windows={name: {key: value[key] for key in ("test_bodies_typechecked", "os_test_executions")}
                 for name, value in windows.items()},
        historical_report_source=PREVIOUS, planned_outputs=len(plan.outputs))
    if args.publish:
        plan.publish(report)
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
