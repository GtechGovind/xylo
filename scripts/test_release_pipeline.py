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
            proof = {"schema": 1, "target": target, "platform": platform, "source": source,
                     "cargo_lock_sha256": lock, "checks": sorted(COMMON_CHECKS | {package_check}),
                     "passed": True, "qualification": "native_package_smoke", "license_inventory_complete": True,
                     "binaries": [{"name": name, "architecture": architecture, "sha256": "b" * 64, "size_bytes": 1}
                                  for name in ["agent-workbench", "workbench-worker", "workbench-helper", "whisper-cli"]],
                     "artifacts": artifacts}
            write_json(folder / "qualification.json", proof)
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
        result = type("Result", (), {"stdout": json.dumps({"visibility": "PRIVATE", "nameWithOwner": "owner/repo"})})()
        with patch("release_preview.subprocess.run", return_value=result) as command:
            with self.assertRaisesRegex(ValueError, "public"):
                create_draft(Path("unused"), {"tag": "v0.1.0"}, "owner/repo")
            self.assertEqual(command.call_count, 1)
            self.assertEqual(command.call_args.args[0][:3], ["gh", "repo", "view"])

    def test_debian_dependency_inventory_uses_all_four_real_executable_paths(self):
        result = type("Result", (), {"stdout": "shlibs:Depends=libc6 (>= 2.38), libstdc++6 (>= 13)\n"})()
        with patch("release.subprocess.run", return_value=result) as command:
            dependencies = linux_dependencies(Path("/binaries"), Path("/native"), ["libvulkan1"])
            self.assertEqual(dependencies, ["libc6 (>= 2.38)", "libstdc++6 (>= 13)", "libvulkan1"])
            argv = command.call_args.args[0]
            self.assertEqual(argv[:2], ["dpkg-shlibdeps", "-O"])
            self.assertEqual(sum(arg.startswith("-e") for arg in argv), 4)
            self.assertIn("-e/native/whisper-runtime/runtime/whisper-cli", argv)


if __name__ == "__main__":
    unittest.main()
