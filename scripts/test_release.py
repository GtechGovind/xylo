#!/usr/bin/env python3
"""Release policy/layout/reproducibility regressions; no signing credentials."""
import json
import os
import copy
import plistlib
import sys
import struct
import stat
import zlib
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
from release import archive_bundle, configuration, preflight_configuration, rehash_native_assets
from release_common import ROOT, TARGETS, digest
from release_manifest import allowed_url, validate_policy


class ReleaseTests(unittest.TestCase):
    def test_disabled_default_cannot_publish_a_signed_feed(self):
        with self.assertRaisesRegex(ValueError, "real maintainer"):
            validate_policy(ROOT / "packaging/update-policy.json")

    def test_untrusted_and_credential_bearing_urls_are_rejected(self):
        policy = {"allowed_origins": ["https://updates.example.test"]}
        allowed_url("https://updates.example.test/releases/v1.json", policy)
        for url in ("http://updates.example.test/file", "https://other.example/file",
                    "https://user:secret@updates.example.test/file", "https://updates.example.test/file#part"):
            with self.subTest(url=url), self.assertRaises(ValueError):
                allowed_url(url, policy)

    def test_bundle_archive_is_reproducible_and_immutable(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            app = root / "Xylo.app"
            (app / "Contents/MacOS").mkdir(parents=True)
            exe = app / "Contents/MacOS/agent-workbench"
            exe.write_bytes(b"fixture binary")
            exe.chmod(0o755)
            archive_bundle(app, root / "one.app.tar.gz", 1234)
            os.utime(exe, (987654, 987654))
            archive_bundle(app, root / "two.app.tar.gz", 1234)
            self.assertEqual(digest(root / "one.app.tar.gz"), digest(root / "two.app.tar.gz"))
            with tarfile.open(root / "one.app.tar.gz", "r:gz") as archive:
                info = archive.getmember("Xylo.app/Contents/MacOS/agent-workbench")
                self.assertEqual((info.uid, info.gid, info.mtime), (0, 0, 1234))
                self.assertEqual(info.mode, stat.S_IMODE(exe.stat().st_mode))
                if os.name != "nt":
                    self.assertEqual(info.mode, 0o755)
            with self.assertRaisesRegex(ValueError, "overwrite"):
                archive_bundle(app, root / "one.app.tar.gz", 1234)

    def test_signed_native_payload_manifest_binds_final_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            asset = root / "whisper-runtime"
            asset.mkdir()
            binary = asset / "whisper-cli"
            binary.write_bytes(b"changed by signing")
            manifest = asset / "manifest.json"
            manifest.write_text(json.dumps({"files": [{"path": "whisper-cli", "sha256": "old", "size_bytes": 1}]}))
            rehash_native_assets(root)
            item = json.loads(manifest.read_text())["files"][0]
            self.assertEqual(item["sha256"], digest(binary))
            self.assertEqual(item["size_bytes"], binary.stat().st_size)

    def test_native_resource_layouts_and_no_windows_downgrade(self):
        out, native, notices = Path("/out"), Path("/native"), Path("/notices")
        mac = configuration("aarch64-apple-darwin", out, native, notices, production=False)
        self.assertIn({"src": str(native), "target": "native-assets"}, mac["resources"])
        self.assertEqual(mac["macos"]["minimumSystemVersion"], "15.0")
        linux = configuration("x86_64-unknown-linux-gnu", out, native, notices, production=False)
        self.assertEqual(linux["appimage"]["files"][str(native)], "usr/bin/native-assets")
        windows = configuration("x86_64-pc-windows-msvc", out, native, notices, production=False)
        self.assertFalse(windows["windows"]["allowDowngrades"])
        self.assertEqual(windows["nsis"]["installMode"], "currentUser")
        for config in (mac, linux, windows):
            self.assertEqual(config["name"], "agent-workbench")
            self.assertEqual(Path(config["licenseFile"]), ROOT / "LICENSE")
            self.assertTrue(Path(config["licenseFile"]).is_absolute())
            self.assertEqual(config["productName"], "Xylo")
            self.assertEqual(config["identifier"], "com.qurkos.xylo")
            self.assertEqual([binary["path"] for binary in config["binaries"]],
                             ["agent-workbench", "workbench-worker", "workbench-helper"])
        preview = (ROOT / "scripts/preview-macos.sh").read_text()
        embedded = preview.split("<<'PLIST'\n", 1)[1].split("\nPLIST\n", 1)[0]
        info = plistlib.loads(embedded.encode())
        self.assertEqual(info["CFBundleName"], "Xylo")
        self.assertEqual(info["CFBundleDisplayName"], "Xylo")
        self.assertEqual(info["CFBundleIdentifier"], "com.qurkos.xylo.preview")
        self.assertEqual(info["CFBundleExecutable"], "agent-workbench")
        self.assertEqual(info["CFBundleIconFile"], "app.icns")
        self.assertIn(str(ROOT / "packaging/icons/app.icns"), mac["icons"])

    def test_preflight_selects_only_inert_sibling_and_rejects_errors_or_output_changes(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        external = Path(temporary.name) / "output"
        config = configuration("aarch64-pc-windows-msvc", external, Path("/native"), Path("/notices"), production=False)
        original = copy.deepcopy(config)
        observed = []

        def command(args, **options):
            self.assertEqual(args[0:2], ["pinned-cargo-packager", "--packages"])
            self.assertEqual(len(args), 3)
            self.assertEqual(options["timeout"], 30)
            self.assertNotIn("CARGO_PACKAGER_SIGN_PRIVATE_KEY", options["env"])
            self.assertNotIn("CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD", options["env"])
            path = options["cwd"] / "packager.json"
            parsed = json.loads(path.read_text())
            self.assertEqual(parsed, [original, {"name": args[2], "formats": []}])
            self.assertNotEqual(parsed[0]["name"], parsed[1]["name"])
            observed.append(path)
            return type("Result", (), {"returncode": 0, "stdout": "", "stderr": ""})()

        with patch("release.subprocess.run", side_effect=command):
            preflight_configuration("pinned-cargo-packager", config)
        self.assertEqual(config, original)
        self.assertFalse(observed[0].exists(), "Parser fixture must be cleaned up")
        self.assertFalse(external.exists())
        for code, diagnostic in [(1, "Nothing to do here"), (2, "invalid arguments"),
                                 (1, "ERROR I/O Error: Not a directory"), (1, "ERROR Failed to parse config"),
                                 (1, "ERROR failed to copy unbuilt executable"), (1, "")]:
            result = type("Result", (), {"returncode": code, "stdout": diagnostic, "stderr": ""})()
            with self.subTest(code=code, diagnostic=diagnostic), patch("release.subprocess.run", return_value=result):
                with self.assertRaisesRegex(ValueError, "preflight failed"):
                    preflight_configuration("pinned-cargo-packager", config)

        for location in ("temporary", "external"):
            def changed(args, **options):
                result = command(args, **options)
                if location == "temporary":
                    (options["cwd"] / "unexpected-artifact").write_bytes(b"unwanted output")
                else:
                    external.mkdir()
                return result
            with self.subTest(location=location), patch("release.subprocess.run", side_effect=changed):
                with self.assertRaisesRegex(ValueError, "changed output"):
                    preflight_configuration("pinned-cargo-packager", config)

    def test_actual_pinned_cli_parses_all_targets_and_rejects_invalid_original(self):
        candidate = os.environ.get("WORKBENCH_PACKAGER")
        if not candidate:
            candidate = str(ROOT / ".release-tools/bin" / ("cargo-packager.exe" if os.name == "nt" else "cargo-packager"))
        if not Path(candidate).is_file():
            self.skipTest("Set WORKBENCH_PACKAGER to the pinned installed CLI; native CI installs it before this test")
        from release_common import packager
        with patch.dict(os.environ, {"WORKBENCH_PACKAGER": candidate}):
            executable = packager()
        with tempfile.TemporaryDirectory(prefix="xylo actual parser ") as directory:
            root = Path(directory)
            configs = {}
            for target in TARGETS:
                with self.subTest(target=target):
                    output = root / target
                    config = configuration(target, output, root / "missing-native", root / "missing-notices", production=False)
                    original = copy.deepcopy(config)
                    preflight_configuration(executable, config)
                    self.assertEqual(config, original)
                    self.assertFalse(output.exists(), "A parser check must not package or create an output directory")
                    configs[target] = config
            base = configs["aarch64-pc-windows-msvc"]
            for mutation in ({"formats": ["not-a-package-format"]}, {"name": 17},
                             {"windows": {"allowDowngrades": "not-a-boolean"}}, {"unknownConfigField": True}):
                with self.subTest(mutation=mutation):
                    invalid = copy.deepcopy(base)
                    invalid.update(mutation)
                    with self.assertRaisesRegex(ValueError, "preflight failed"):
                        preflight_configuration(executable, invalid)
            unnamed = copy.deepcopy(base)
            unnamed.pop("name")
            with self.assertRaisesRegex(ValueError, "preflight failed"):
                preflight_configuration(executable, unnamed)
            self.assertEqual(list(root.iterdir()), [], "Positive and negative parser checks must leave no package outputs")

    def test_packaged_icon_formats_and_original_master_provenance(self):
        def png_dimensions(data):
            self.assertTrue(data.startswith(b"\x89PNG\r\n\x1a\n"))
            offset, dimensions, finished = 8, None, False
            while offset < len(data):
                self.assertGreaterEqual(len(data) - offset, 12)
                length, kind = struct.unpack_from(">I4s", data, offset)
                end = offset + 12 + length
                self.assertLessEqual(end, len(data))
                payload = data[offset + 8:end - 4]
                self.assertEqual(zlib.crc32(kind + payload), struct.unpack_from(">I", data, end - 4)[0])
                if offset == 8:
                    self.assertEqual((kind, length), (b"IHDR", 13))
                    dimensions = struct.unpack_from(">II", payload)
                if kind == b"IEND":
                    self.assertEqual(length, 0)
                    self.assertEqual(end, len(data))
                    finished = True
                offset = end
            self.assertTrue(finished)
            return dimensions

        directory = ROOT / "packaging/icons"
        provenance = json.loads((directory / "manifest.json").read_text())
        brand = json.loads((ROOT / "packaging/branding/brand.json").read_text())
        master = provenance["master"]
        self.assertEqual(provenance["schema"], 1)
        self.assertEqual(master["path"], "packaging/branding/xylo-mark.png")
        source = ROOT / master["path"]
        self.assertEqual(master["sha256"], digest(source))
        self.assertEqual(master["sha256"], brand["sha256"])
        self.assertEqual(master["bytes"], source.stat().st_size)
        self.assertEqual(png_dimensions(source.read_bytes()), (master["width"], master["height"]))
        self.assertEqual(provenance["conversion"]["filter"], "Lanczos3")
        expected = {"app.png": [512], "app.ico": [256],
                    "app.icns": [16, 32, 64, 128, 256, 512, 1024],
                    "sidebar.png": [64], "tray.png": [24]}
        outputs = provenance["outputs"]
        self.assertEqual(len(outputs), len(expected))
        self.assertEqual({entry["path"] for entry in outputs}, set(expected))
        for entry in outputs:
            path = directory / entry["path"]
            self.assertLessEqual(path.stat().st_size, 8 * 1024 * 1024)
            data = path.read_bytes()
            self.assertEqual(entry["bytes"], len(data))
            self.assertEqual(entry["sha256"], digest(path))
            self.assertEqual(entry["sizes"], expected[entry["path"]])
            self.assertEqual(entry["format"], path.suffix[1:])
            if path.suffix == ".png":
                size = entry["sizes"][0]
                self.assertEqual(png_dimensions(data), (size, size))
            elif path.suffix == ".ico":
                self.assertEqual(struct.unpack_from("<HHH", data), (0, 1, 1))
                width, height, _, reserved, planes, depth, length, offset = struct.unpack_from("<BBBBHHII", data, 6)
                self.assertEqual((width, height, reserved, planes, depth, offset), (0, 0, 0, 1, 32, 22))
                self.assertEqual(offset + length, len(data))
                self.assertEqual(png_dimensions(data[offset:]), (256, 256))
            else:
                magic, total = struct.unpack_from(">4sI", data)
                self.assertEqual((magic, total), (b"icns", len(data)))
                offset, seen = 8, {}
                types = {b"icp4": 16, b"icp5": 32, b"icp6": 64, b"ic07": 128,
                         b"ic08": 256, b"ic09": 512, b"ic10": 1024}
                while offset < len(data):
                    self.assertGreaterEqual(len(data) - offset, 8)
                    kind, length = struct.unpack_from(">4sI", data, offset)
                    self.assertIn(kind, types)
                    self.assertNotIn(kind, seen)
                    self.assertGreaterEqual(length, 8)
                    self.assertLessEqual(offset + length, len(data))
                    size = types[kind]
                    self.assertEqual(png_dimensions(data[offset + 8:offset + length]), (size, size))
                    seen[kind] = size
                    offset += length
                self.assertEqual(seen, types)


if __name__ == "__main__":
    unittest.main()
