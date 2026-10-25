#!/usr/bin/env python3
"""Deterministic dependency inventory, license texts, and build provenance.

This inventories the Cargo resolved normal/build closure of the three delivered
binaries. It is not a vulnerability scan or a legal approval of dependencies.
"""
from __future__ import annotations

import argparse
import json
import platform
from pathlib import Path
import shutil
import sys
import tomllib
from urllib.parse import quote

sys.dont_write_bytecode = True
from release_common import BINARIES, PACKAGER_VERSION, ROOT, digest, run, source_identity, write_json
from release_licenses import (LicenseSupplements, alternatives, canonical_source, full_license,
                              git_blob, publication_source, regular, registry_text_source,
                              source_kind, validate_registry_grant)


def cargo_inventory(target: str) -> tuple[dict, set[str]]:
    metadata = json.loads(run(["cargo", "metadata", "--locked", "--format-version", "1",
                               "--filter-platform", target], capture=True))
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    roots = {package["id"] for package in metadata["packages"]
             if package["name"] in BINARIES}
    if len(roots) != len(BINARIES):
        raise ValueError("The delivered binary roots are missing from Cargo metadata")
    pending, included = list(roots), set()
    while pending:
        item = pending.pop()
        if item in included:
            continue
        included.add(item)
        for dep in nodes[item]["deps"]:
            if any(kind["kind"] in (None, "build") for kind in dep["dep_kinds"]):
                pending.append(dep["pkg"])
    return metadata, included


def license_paths(package: dict) -> list[Path]:
    directory = Path(package["manifest_path"]).parent
    candidates = {path for path in directory.iterdir() if path.is_file()
                  and path.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE"))}
    if package.get("license_file"):
        candidates.add(directory / package["license_file"])
    # Some packages inherit workspace licenses or put text in a `licenses` dir.
    nested = directory / "licenses"
    if nested.is_dir():
        candidates.update(path for path in nested.rglob("*") if path.is_file())
    if not candidates and package["source"] is None:
        candidates.add(ROOT / "LICENSE")
    return sorted(path for path in candidates if path.is_file())


def generate(target: str, output: Path, *, strict: bool) -> None:
    metadata, included = cargo_inventory(target)
    packages = sorted((p for p in metadata["packages"] if p["id"] in included),
                      key=lambda p: (p["name"], p["version"], p["id"]))
    output.mkdir(parents=True, exist_ok=True)
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))
    checksums = {(p["name"], p["version"], p.get("source")): p.get("checksum")
                 for p in lock["package"]}
    supplements = LicenseSupplements(ROOT / "packaging/licenses")
    components, licenses, missing, refs = [], [], [], {}
    applied_supplements = []
    for number, package in enumerate(packages):
        ref = f"cargo-{number}"
        refs[package["id"]] = ref
        component = {"type": "application" if package["name"] in BINARIES else "library", "bom-ref": ref, "name": package["name"],
                     "version": package["version"],
                     "purl": f"pkg:cargo/{quote(package['name'])}@{quote(package['version'])}"}
        expression = package.get("license")
        if expression:
            component["licenses"] = [{"expression": expression}]
        checksum = checksums.get((package["name"], package["version"], package["source"]))
        if checksum:
            component["hashes"] = [{"alg": "SHA-256", "content": checksum}]
        if package.get("repository"):
            component["externalReferences"] = [{"type": "vcs", "url": package["repository"]}]
        texts = license_paths(package)
        folder = output / "licenses" / f"{package['name']}-{package['version']}-{number}"
        recorded = []
        for index, text in enumerate(texts):
            folder.mkdir(parents=True, exist_ok=True)
            copied = folder / f"{index}-{text.name}"
            shutil.copyfile(text, copied)
            recorded.append({"path": copied.relative_to(output).as_posix(), "sha256": digest(copied),
                             "origin": "package", "kind": "package_text"})
        supplement = supplements.resolve(package, checksum)
        if supplement:
            for index, text in enumerate(supplement["texts"]):
                folder.mkdir(parents=True, exist_ok=True)
                copied = folder / f"supplement-{index}-{Path(text['file']).name}"
                shutil.copyfile(regular(supplements.directory, text["file"]), copied)
                recorded.append({"path": copied.relative_to(output).as_posix(), "sha256": digest(copied),
                                 "origin": "verified_supplement", "kind": text["kind"],
                                 "license_id": text.get("license_id"),
                                 "evidence": {key: value for key, value in text.items() if key != "file"}})
            applied_supplements.append({**supplement["entry"], "text_records": [
                {key: value for key, value in text.items() if key != "file"}
                for text in supplement["texts"]]})
        if not texts and not (supplement and supplement["has_full_text"]):
            missing.append(f"{package['name']} {package['version']}")
        licenses.append({"name": package["name"], "version": package["version"],
                         "expression": expression, "archive_sha256": checksum, "texts": recorded})
        components.append(component)
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    dependencies = []
    for package in packages:
        children = {refs[dep["pkg"]] for dep in nodes[package["id"]]["deps"]
                    if dep["pkg"] in refs
                    and any(kind["kind"] in (None, "build") for kind in dep["dep_kinds"])}
        dependencies.append({"ref": refs[package["id"]], "dependsOn": sorted(children)})
    write_json(output / "sbom.cdx.json", {"bomFormat": "CycloneDX", "specVersion": "1.6",
               "version": 1, "components": components, "dependencies": dependencies})
    supplement_identity = {"schema_version": 2, "catalogue_sha256": digest(supplements.path),
                           "applied_packages": len(applied_supplements)}
    write_json(output / "license-supplements.json", {**supplement_identity,
               "status": "Verified upstream or canonical-standard notice provenance; not legal or release acceptance",
               "packages": applied_supplements})
    supplement_identity["provenance_sha256"] = digest(output / "license-supplements.json")
    write_json(output / "licenses.json", {"packages": licenses, "missing_texts": missing,
                                         "supplements": supplement_identity})
    identity = source_identity()
    identity.update({"target": target, "rustc": run(["rustc", "--version", "--verbose"], capture=True),
                     "cargo": run(["cargo", "--version"], capture=True),
                     "packager_version": PACKAGER_VERSION, "build_host": platform.platform(),
                     "cargo_lock_sha256": digest(ROOT / "Cargo.lock")})
    if platform.system() == "Darwin":
        identity["sdk_version"] = run(["xcrun", "--show-sdk-version"], capture=True)
        identity["xcode_version"] = run(["xcodebuild", "-version"], capture=True)
    write_json(output / "build-provenance.json", identity)
    validate_inventory(output, strict=strict)


def validate_inventory(directory: Path, *, strict: bool = False) -> None:
    """Validate the actual copied bytes and retain honest unresolved coverage."""
    inventory = json.loads(regular(directory, "licenses.json").read_text(encoding="utf-8"))
    missing = []
    proofs = {}
    if "supplements" in inventory:
        identity = inventory["supplements"]
        provenance_path = regular(directory, "license-supplements.json")
        if digest(provenance_path) != identity["provenance_sha256"]:
            raise ValueError("Packaged license provenance hash mismatch")
        provenance = json.loads(provenance_path.read_text(encoding="utf-8"))
        if provenance["catalogue_sha256"] != identity["catalogue_sha256"] or provenance["applied_packages"] != identity["applied_packages"] or len(provenance["packages"]) != identity["applied_packages"]:
            raise ValueError("Packaged license provenance catalogue/count mismatch")
        for entry in provenance["packages"]:
            key = (entry["name"], entry["version"])
            if key in proofs:
                raise ValueError("Duplicate packaged license package proof")
            records = entry.get("text_records")
            if provenance.get("schema_version", 1) >= 2:
                if (not isinstance(records, list)
                        or len(records) != len(entry["texts"])
                        or {text.get("id") for text in records} != set(entry["texts"])):
                    raise ValueError("Packaged license proof lacks its complete text bindings")
            elif strict:
                raise ValueError("Legacy packaged proof cannot verify its complete notice bindings")
            if entry["proof"]["kind"] == "registry_manifest_grant":
                publication_source(entry)
            else:
                LicenseSupplements._source(entry["upstream"])
            proofs[key] = entry
    package_keys = set()
    for package in inventory["packages"]:
        package_key = (package["name"], package["version"])
        if package_key in package_keys:
            raise ValueError("Duplicate packaged dependency license entry")
        package_keys.add(package_key)
        proof = proofs.get(package_key)
        if proof and (proof["archive_sha256"] != package["archive_sha256"] or proof["license_expression"] != package["expression"]):
            raise ValueError("Packaged license proof belongs to another archive/license")
        covered = False
        registry_evidence, registry_bytes = [], {}
        proof_texts = set()
        legacy_evidence = False
        for text in package["texts"]:
            path = regular(directory, text["path"])
            if digest(path) != text["sha256"]:
                raise ValueError("Packaged dependency license hash mismatch")
            if text.get("origin", "package") == "package":
                covered = True  # Preserve the existing package-native inventory policy.
            elif text["origin"] == "verified_supplement":
                # Keep canonical authority and registry notices distinct from an
                # upstream repository file. The copied grant is checked below.
                evidence = text.get("evidence")
                if evidence is None:
                    # Read-only compatibility for inventories emitted before
                    # source classes existed; those supported upstream files only.
                    evidence = {**text["upstream"], "kind": text["kind"],
                                "license_id": text.get("license_id"), "sha256": text["sha256"]}
                if evidence["sha256"] != text["sha256"] or evidence["kind"] != text["kind"] or evidence.get("license_id") != text.get("license_id"):
                    raise ValueError("Packaged license evidence metadata mismatch")
                kind = source_kind(evidence)
                is_registry_grant = proof and proof["proof"]["kind"] == "registry_manifest_grant"
                if kind == "canonical_standard":
                    canonical_source(evidence)
                    if not is_registry_grant:
                        raise ValueError("Canonical text has no exact publication grant")
                elif kind == "registry_archive":
                    registry_text_source(evidence, proof)
                    if not is_registry_grant:
                        raise ValueError("Registry notice has no exact publication grant")
                elif kind == "upstream_file":
                    LicenseSupplements._source(evidence)
                    if not proof or (evidence["repository"], evidence["commit"]) != (proof.get("upstream", {}).get("repository"), proof.get("upstream", {}).get("commit")):
                        raise ValueError("Packaged license ownership differs from package proof")
                else:
                    raise ValueError("Unknown packaged license source class")
                if not proof:
                    raise ValueError("Packaged license ownership differs from package proof")
                if git_blob(path.read_bytes()) != evidence["git_blob_sha1"]:
                    raise ValueError("Packaged license differs from pinned upstream blob")
                evidence_id = evidence.get("id")
                if evidence_id is None:
                    legacy_evidence = True
                    if strict:
                        raise ValueError("Legacy packaged proof cannot verify its complete notice set")
                elif evidence_id not in proof["texts"] or evidence_id in proof_texts:
                    raise ValueError("Duplicate or unbound packaged license text")
                else:
                    expected = next((record for record in proof.get("text_records", [])
                                     if record["id"] == evidence_id), None)
                    if expected is not None and evidence != expected:
                        raise ValueError("Packaged license evidence differs from its immutable proof")
                    proof_texts.add(evidence_id)
                if is_registry_grant:
                    key = evidence.get("id")
                    if key not in proof["texts"] or key in registry_bytes:
                        raise ValueError("Duplicate or unbound packaged publication text")
                    registry_evidence.append(evidence)
                    registry_bytes[key] = path.read_bytes()
                if text["kind"] == "license":
                    if text["license_id"] not in alternatives(package["expression"]) or not full_license(path.read_bytes(), text["license_id"]):
                        raise ValueError("Packaged guidance is not full declared license text")
                    covered = True
            else:
                raise ValueError("Unknown packaged license provenance")
        if proof and not legacy_evidence and proof_texts != set(proof["texts"]):
            raise ValueError("Packaged upstream license or auxiliary notice is missing")
        if proof and proof["proof"]["kind"] == "registry_manifest_grant":
            if set(registry_bytes) != set(proof["texts"]):
                raise ValueError("Packaged publication notice or canonical text is missing")
            validate_registry_grant(proof, registry_evidence, lambda text: registry_bytes[text["id"]])
        if not covered:
            missing.append(f"{package['name']} {package['version']}")
    if set(proofs) - package_keys:
        raise ValueError("Packaged publication proof has no corresponding dependency")
    if missing != inventory["missing_texts"]:
        raise ValueError("Dependency license coverage does not match the recorded gaps")
    if strict and missing:
        raise ValueError(f"License text review required for {len(missing)} packages; see licenses.json")


def checksums(directory: Path) -> None:
    # Exclude directories, temporary files, and this file itself.
    artifacts = sorted(path for path in directory.iterdir()
                       if path.is_file() and path.name != "SHA256SUMS")
    (directory / "SHA256SUMS").write_text("".join(f"{digest(p)}  {p.name}\n" for p in artifacts), encoding="utf-8", newline="\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--validate-inventory", type=Path)
    parser.add_argument("--strict", action="store_true")
    args = parser.parse_args()
    if args.validate_inventory:
        if args.target or args.output:
            parser.error("Use validation separately from inventory generation")
        validate_inventory(args.validate_inventory.resolve(), strict=args.strict)
    else:
        if not args.target or not args.output:
            parser.error("Generation requires --target and --output")
        generate(args.target, args.output.resolve(), strict=args.strict)
