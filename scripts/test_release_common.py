"""Real UTF-8 child/metadata regressions; no compilation, credentials or packaging."""
from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from release_common import ROOT, capture_utf8, digest, run, write_json
from release_metadata import checksums, validate_inventory


class ReleaseEncodingTests(unittest.TestCase):
    def test_actual_utf8_child_preserves_json_and_diagnostics_under_cp1252_default(self):
        payload = {"authors": ["Łukasz — 測試"], "path": "資料/naïve.rs"}
        stdout = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        stderr = "Warning: Łódź 測試\n".encode("utf-8")
        command = [sys.executable, "-c", "import sys; "
                   f"sys.stdout.buffer.write(bytes.fromhex('{stdout.hex()}')); "
                   f"sys.stderr.buffer.write(bytes.fromhex('{stderr.hex()}'))"]
        # The old text-mode boundary selected cp1252 and failed on byte 0x81.
        with patch("subprocess._text_encoding", return_value="cp1252") as locale_default:
            result = capture_utf8(command, check=True, timeout=10)
        self.assertEqual(json.loads(result.stdout), payload)
        self.assertEqual(result.stderr, stderr.decode("utf-8"))
        locale_default.assert_not_called()
        with patch("subprocess._text_encoding", return_value="cp1252"):
            self.assertEqual(json.loads(run(command, capture=True)), payload)

    def test_invalid_utf8_in_either_pipe_propagates_on_the_calling_thread(self):
        for pipe in ("stdout", "stderr"):
            with self.subTest(pipe=pipe), self.assertRaises(UnicodeDecodeError):
                capture_utf8([sys.executable, "-c", f"import sys; sys.{pipe}.buffer.write(b'\\xff')"],
                             check=True, timeout=10)

    def test_failed_child_retains_return_code_and_original_binary_evidence(self):
        with self.assertRaises(subprocess.CalledProcessError) as caught:
            capture_utf8([sys.executable, "-c", "import sys; sys.stdout.buffer.write(b'ok'); "
                          "sys.stderr.buffer.write(b'\\xff'); sys.exit(7)"], check=True, timeout=10)
        self.assertEqual(caught.exception.returncode, 7)
        self.assertEqual(caught.exception.stdout, b"ok")
        self.assertEqual(caught.exception.stderr, b"\xff")

    def test_uncaptured_commands_keep_inherited_native_console_streams(self):
        with patch("release_common.subprocess.run", return_value=subprocess.CompletedProcess([], 0)) as child:
            self.assertEqual(run(["controlled-native-console"]), "")
        self.assertNotIn("capture_output", child.call_args.kwargs)
        self.assertNotIn("text", child.call_args.kwargs)
        self.assertNotIn("encoding", child.call_args.kwargs)

    def test_unicode_inventory_paths_and_checksum_names_round_trip_as_utf8(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            notice = root / "LICENSE-Ł測試.txt"
            notice.write_text("Controlled license fixture: Ł測試\n", encoding="utf-8", newline="\n")
            value = {"packages": [{"name": "Łfixture", "version": "1", "expression": "MIT", "texts": [
                {"path": notice.name, "sha256": digest(notice), "origin": "package"}]}], "missing_texts": []}
            write_json(root / "licenses.json", value)
            validate_inventory(root, strict=True)
            checksums(root)
            self.assertEqual(json.loads((root / "licenses.json").read_bytes()), value)
            self.assertIn(notice.name, (root / "SHA256SUMS").read_text(encoding="utf-8"))

    def test_actual_locked_cargo_metadata_json_decodes_before_optimized_build(self):
        target = os.environ.get("CARGO_BUILD_TARGET")
        if not target:
            version = capture_utf8(["rustc", "--version", "--verbose"], check=True, timeout=30).stdout
            target = next(line.removeprefix("host: ") for line in version.splitlines() if line.startswith("host: "))
        result = capture_utf8(["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", target],
                              cwd=ROOT, check=True, timeout=120)
        metadata = json.loads(result.stdout)
        self.assertTrue({"agent-workbench", "workbench-worker", "workbench-helper"}.issubset(
            {package["name"] for package in metadata["packages"]}))
        self.assertIsNotNone(metadata["resolve"])


if __name__ == "__main__":
    unittest.main()
