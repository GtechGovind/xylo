#!/usr/bin/env python3
"""Offline release-gate regressions. No Cargo, installers, signing or GitHub writes."""
import json
from pathlib import Path
import struct
import sys
import tempfile
import unittest
from unittest.mock import patch
import zipfile

sys.dont_write_bytecode = True
from release_common import ROOT, TARGETS, digest, workspace_version, write_json
from release_preview import COMMON_CHECKS, create_draft, prepare, validate_results
from release_smoke import JsonProcess, binary_architecture
from release import linux_dependencies
from release_windows import PHASE
from test_release_linux_notices import inventory_fixture

COMMIT = "a" * 40


class PipelineTests(unittest.TestCase):
    def fixture(self, root):
        lock = digest(ROOT / "Cargo.lock")
        for target, (platform, _) in TARGETS.items():
            folder = root / target
            folder.mkdir(parents=True)
            metadata = folder / "release-metadata"
            metadata.mkdir()
            text = metadata / "LICENSE"
            text.write_text("Fixture package license text.\n")
            write_json(metadata / "licenses.json", {"packages": [{"name": "fixture", "version": "1.0.0",
                       "texts": [{"path": "LICENSE", "sha256": digest(text)}]}], "missing_texts": []})
            write_json(metadata / "sbom.cdx.json", {"bomFormat": "CycloneDX", "components": [{"name": "fixture"}]})
            write_json(metadata / "build-provenance.json", {"commit": COMMIT, "dirty": False,
                       "target": target, "cargo_lock_sha256": lock})
            source = {"commit": COMMIT, "dirty": False, "source_date_epoch": 1}
            write_json(folder / "build-plan.json", {"source": source, "target": target, "mode": "preview"})
            write_json(folder / "release-status.json", {"mode": "preview", "license_inventory_complete": True,
                       "updater_feed_enabled": False, "published": False, "os_signed": False})
            extensions = [".dmg", ".app.tar.gz"] if "apple" in target else [".exe"] if "windows" in target else [".AppImage", ".deb"]
            artifacts = []
            for extension in extensions:
                file = folder / ("fixture" + extension)
                file.write_bytes((target + extension).encode())
                artifacts.append({"name": file.name, "size_bytes": file.stat().st_size, "sha256": digest(file)})
            architecture = "aarch64" if target.startswith("aarch64") else "x86_64"
            package_check = "app_bundle_codesign_verified" if "apple" in target else "nsis_silent_install_executed" if "windows" in target else "appimage_native_extraction_executed"
            platform_checks = {package_check}
            if "linux" in target:
                platform_checks.add("packaged_appimage_apprun_launch_alive_8s")
                platform_checks.add("packaged_debian_control_validated")
                platform_checks.add("packaged_linux_distro_native_notices_validated")
                platform_checks.add("packaged_linux_appimage_infrastructure_notices_validated")
                platform_checks.add("packaged_linux_debian_native_notices_validated")
            if "windows" in target:
                platform_checks.add("packaged_windows_crt_imports_validated")
            proof = {"schema": 1, "target": target, "platform": platform, "source": source,
                     "cargo_lock_sha256": lock, "checks": sorted(COMMON_CHECKS | platform_checks),
                     "passed": True, "qualification": "native_package_smoke", "license_inventory_complete": True,
                     "binaries": [{"name": name, "architecture": architecture, "sha256": "b" * 64, "size_bytes": 1}
                                  for name in ["agent-workbench", "workbench-worker", "workbench-helper", "whisper-cli"]],
                     "artifacts": artifacts}
            write_json(folder / "qualification.json", proof)
            if "linux" in target:
                inventory_fixture(metadata, target, proof["binaries"])
                proof["linux_native_notices_report_sha256"] = digest(metadata / "linux-native-validation.json")
                write_json(folder / "qualification.json", proof)
            if "windows" in target:
                # Offline report fixture, not an LLVM inspection or native run.
                write_json(metadata / "windows-pe-imports.json", {
                    "schema": 1, "target": target, "phase": PHASE, "passed": True,
                    "tool": {"sha256": "d" * 64, "version": "LLVM offline fixture"},
                    "binaries": [{**item,
                                  "format": "COFF-ARM64" if architecture == "aarch64" else "COFF-x86-64",
                                  "imports": ["KERNEL32.dll"], "delay_imports": [],
                                  "report_sha256": "e" * 64, "forbidden_runtime_imports": []}
                                 for item in proof["binaries"]]})
        return lock

    def test_all_six_source_bound_targets_prepare_unique_assets_and_complete_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            self.fixture(root / "inputs")
            output = root / "prepared"
            manifest = prepare(root / "inputs", output, COMMIT, workspace_version())
            self.assertEqual(len(manifest["platforms"]), 6)
            self.assertTrue(manifest["draft"])
            self.assertTrue(manifest["prerelease"])
            self.assertFalse(manifest["signed"])
            names = [item["name"] for platform in manifest["platforms"] for item in platform["artifacts"]]
            self.assertEqual(len(names), len(set(names)))
            self.assertTrue(any("windows-aarch64.exe" in name for name in names))
            for platform in manifest["platforms"]:
                with zipfile.ZipFile(output / platform["metadata"]["name"]) as archive:
                    self.assertIn("release-metadata/LICENSE", archive.namelist())
                    self.assertIn("qualification.json", archive.namelist())
            self.assertIn("Unsigned preview", (output / "RELEASE_NOTES.md").read_text())
            with self.assertRaisesRegex(ValueError, "immutable"):
                prepare(root / "inputs", output, COMMIT, workspace_version())

    def test_missing_target_and_wrong_commit_fail_before_output_mutation(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            self.fixture(root / "inputs")
            with self.assertRaisesRegex(ValueError, "another source"):
                prepare(root / "inputs", root / "output", "c" * 40, workspace_version())
            self.assertFalse((root / "output").exists())
            next((root / "inputs").rglob("qualification.json")).unlink()
            with self.assertRaisesRegex(ValueError, "All six"):
                prepare(root / "inputs", root / "output", COMMIT, workspace_version())
            self.assertFalse((root / "output").exists())

    def test_tampered_payload_license_and_incomplete_smoke_fail_closed(self):
        for mutation, message in [("payload", "hash mismatch"), ("license", "license hash mismatch"), ("smoke", "incomplete")]:
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                lock = self.fixture(root)
                folder = next(root.iterdir())
                if mutation == "payload":
                    next(folder.glob("fixture.*")).write_bytes(b"changed")
                elif mutation == "license":
                    (folder / "release-metadata/LICENSE").write_bytes(b"changed")
                else:
                    path = folder / "qualification.json"
                    proof = json.loads(path.read_text())
                    proof["checks"].remove("packaged_desktop_isolated_profile_launch_alive_8s")
                    write_json(path, proof)
                with self.assertRaisesRegex(ValueError, message):
                    validate_results(root, COMMIT, lock)

    def test_missing_license_and_wrong_architecture_are_not_publishable_previews(self):
        for mutation in ["license", "architecture"]:
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                lock = self.fixture(root)
                folder = next(root.iterdir())
                if mutation == "license":
                    path = folder / "release-metadata/licenses.json"
                    inventory = json.loads(path.read_text())
                    inventory["packages"][0]["texts"] = []
                    inventory["missing_texts"] = ["fixture 1.0.0"]
                    write_json(path, inventory)
                else:
                    path = folder / "qualification.json"
                    proof = json.loads(path.read_text())
                    proof["binaries"][0]["architecture"] = "unsupported"
                    write_json(path, proof)
                with self.assertRaises(ValueError):
                    validate_results(root, COMMIT, lock)

    def test_linux_direct_binary_launch_cannot_replace_actual_apprun_qualification(self):
        for target in ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"):
            with self.subTest(target=target), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                self.fixture(root / "inputs")
                path = root / "inputs" / target / "qualification.json"
                proof = json.loads(path.read_text())
                proof["checks"].remove("packaged_appimage_apprun_launch_alive_8s")
                write_json(path, proof)
                with self.assertRaisesRegex(ValueError, "incomplete"):
                    prepare(root / "inputs", root / "prepared", COMMIT, workspace_version())
                self.assertFalse((root / "prepared").exists())

    def test_linux_packages_require_debian_control_inspection(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            lock = self.fixture(root)
            path = root / "aarch64-unknown-linux-gnu/qualification.json"
            proof = json.loads(path.read_text())
            proof["checks"].remove("packaged_debian_control_validated")
            write_json(path, proof)
            with self.assertRaisesRegex(ValueError, "incomplete"):
                validate_results(root, COMMIT, lock)

    def test_linux_native_notices_are_required_and_tied_to_copied_bytes_and_binaries(self):
        for mutation in ("missing_check", "missing_report", "text", "report", "binary"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                lock = self.fixture(root)
                folder = root / "x86_64-unknown-linux-gnu"
                path = folder / "qualification.json"
                proof = json.loads(path.read_text())
                if mutation == "missing_check":
                    proof["checks"].remove("packaged_linux_distro_native_notices_validated")
                    write_json(path, proof)
                elif mutation == "missing_report":
                    (folder / "release-metadata/linux-native-validation.json").unlink()
                elif mutation == "text":
                    next((folder / "release-metadata/linux-native/texts").iterdir()).write_bytes(b"Tampered notice")
                elif mutation == "report":
                    (folder / "release-metadata/linux-native-validation.json").write_text("{}", encoding="utf-8", newline="\n")
                else:
                    proof["binaries"][0]["sha256"] = "c" * 64
                    write_json(path, proof)
                with self.assertRaises(ValueError):
                    validate_results(root, COMMIT, lock)

    def test_windows_requires_import_proof_bound_to_installed_binaries(self):
        for mutation in ("missing_check", "missing_report", "different_binary", "forbidden_delay_import"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                lock = self.fixture(root)
                folder = root / "aarch64-pc-windows-msvc"
                path = folder / "release-metadata/windows-pe-imports.json"
                if mutation == "missing_check":
                    proof = json.loads((folder / "qualification.json").read_text())
                    proof["checks"].remove("packaged_windows_crt_imports_validated")
                    write_json(folder / "qualification.json", proof)
                elif mutation == "missing_report":
                    path.unlink()
                else:
                    report = json.loads(path.read_text())
                    if mutation == "different_binary":
                        report["binaries"][0]["sha256"] = "f" * 64
                    else:
                        report["binaries"][0]["delay_imports"] = ["VCRUNTIME140.dll"]
                    write_json(path, report)
                with self.assertRaises(ValueError):
                    validate_results(root, COMMIT, lock)

    def test_native_machine_headers_are_parsed_not_inferred_from_names(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "wrong-name.exe"
            for architecture, pe, elf, macho in [("aarch64", 0xAA64, 183, 0x0100000C), ("x86_64", 0x8664, 62, 0x01000007)]:
                header = bytearray(128)
                header[:2] = b"MZ"
                struct.pack_into("<I", header, 60, 64)
                struct.pack_into("<4sH", header, 64, b"PE\0\0", pe)
                path.write_bytes(header)
                self.assertEqual(binary_architecture(path), architecture)
                header[:6] = b"\x7fELF\x02\x01"
                struct.pack_into("<H", header, 18, elf)
                path.write_bytes(header)
                self.assertEqual(binary_architecture(path), architecture)
                struct.pack_into("<4sI", header, 0, b"\xcf\xfa\xed\xfe", macho)
                path.write_bytes(header)
                self.assertEqual(binary_architecture(path), architecture)
            path.write_bytes(b"not executable")
            with self.assertRaises(ValueError):
                binary_architecture(path)

    def test_correlated_jsonl_smoke_uses_actual_bounded_child_and_clean_shutdown(self):
        source = ("import json,sys\nfor line in sys.stdin:\n r=json.loads(line); "
                  "print(json.dumps({'protocol_version':1,'id':r['id'],'outcome':{'status':'completed','value':{'ok':True}}}),flush=True)\n")
        with tempfile.TemporaryDirectory() as temporary:
            child = JsonProcess([sys.executable, "-u", "-c", source], Path(temporary) / "child.log")
            try:
                self.assertEqual(child.execute("fixture", {}), {"ok": True})
            finally:
                child.close()
            self.assertEqual(child.process.returncode, 0)

    def test_draft_creation_refuses_private_destination_before_any_write(self):
        result = type("Result", (), {"stdout": json.dumps({"visibility": "PRIVATE", "nameWithOwner": "owner/repo"}).encode("utf-8"), "stderr": b""})()
        with patch("release_preview.subprocess.run", return_value=result) as command:
            with self.assertRaisesRegex(ValueError, "public"):
                create_draft(Path("unused"), {"tag": "v0.1.0"}, "owner/repo")
            self.assertEqual(command.call_count, 1)
            self.assertEqual(command.call_args.args[0][:3], ["gh", "repo", "view"])

    def test_debian_dependency_inventory_uses_all_four_real_executable_paths(self):
        result = type("Result", (), {"stdout": b"shlibs:Depends=libc6 (>= 2.38), libstdc++6 (>= 13)\n", "stderr": b""})()
        with patch("release.subprocess.run", return_value=result) as command:
            dependencies = linux_dependencies(Path("/binaries"), Path("/native"), ["libvulkan1"])
            self.assertEqual(dependencies, ["libc6 (>= 2.38)", "libstdc++6 (>= 13)", "libvulkan1"])
            argv = command.call_args.args[0]
            self.assertEqual(argv[:2], ["dpkg-shlibdeps", "-O"])
            self.assertEqual(sum(arg.startswith("-e") for arg in argv), 4)
            expected = Path("/native") / "whisper-runtime/runtime/whisper-cli"
            self.assertIn("-e" + str(expected.resolve()), argv)


if __name__ == "__main__":
    unittest.main()
