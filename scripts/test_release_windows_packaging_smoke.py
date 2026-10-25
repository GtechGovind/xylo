"""Portable guard tests; actual LLVM/NSIS execution belongs to native Windows CI."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from release_common import BINARIES, digest
from release_windows_packaging_smoke import (fixture_configuration, require_native_vm,
                                             remove_fixture_directory, run_tool, verify_resources)


class WindowsPackagingFixtureTests(unittest.TestCase):
    def test_configuration_matches_real_layout_without_product_install_identity(self):
        for target in ("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"):
            root = Path("fixture inputs café").resolve()
            config = fixture_configuration(target, root, "a" * 32)
            self.assertEqual(config["targetTriple"], target)
            self.assertEqual(config["formats"], ["nsis"])
            self.assertEqual([item["path"] for item in config["binaries"]], list(BINARIES))
            self.assertTrue(config["binaries"][0]["main"])
            self.assertEqual(config["nsis"], {"installMode": "currentUser"})
            self.assertEqual([item["target"] for item in config["resources"]],
                             ["native-assets", "licenses/dependencies"])
            self.assertIn("café", config["resources"][0]["src"])
            self.assertNotEqual(config["identifier"], "com.qurkos.xylo")
            self.assertNotEqual(config["productName"], "Xylo")
            self.assertFalse(any("command" in key.lower() for key in config))
        with self.assertRaises(ValueError):
            fixture_configuration("aarch64-pc-windows-msvc", root, "../bad")

    def test_real_install_requires_matching_native_disposable_vm(self):
        with patch("release_windows_packaging_smoke.platform.system", return_value="Windows"), \
                patch("release_windows_packaging_smoke.platform.machine", return_value="ARM64"), \
                patch.dict(os.environ, {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted"}):
            self.assertEqual(require_native_vm("aarch64-pc-windows-msvc"), "aarch64")
            with self.assertRaises(ValueError):
                require_native_vm("x86_64-pc-windows-msvc")
            with patch.dict(os.environ, {"RUNNER_ENVIRONMENT": "self-hosted"}), self.assertRaises(ValueError):
                require_native_vm("aarch64-pc-windows-msvc")

    def test_installed_unicode_resource_identity_rejects_missing_and_changed_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            relative = "licenses/dependencies/nested café/NOTICE 測試.txt"
            path = root / relative
            path.parent.mkdir(parents=True)
            path.write_bytes("Exact fixture Ł測試\n".encode("utf-8"))
            expected = {relative: digest(path)}
            verify_resources(root, expected)
            path.write_bytes(b"Changed")
            with self.assertRaises(ValueError):
                verify_resources(root, expected)
            path.unlink()
            with self.assertRaises(ValueError):
                verify_resources(root, expected)

    def test_native_child_output_is_explicit_utf8_and_retained(self):
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "child.log"
            text = "Native fixture Ł測試\n"
            command = [sys.executable, "-c", f"import sys; sys.stdout.buffer.write(bytes.fromhex('{text.encode().hex()}'))"]
            with patch("subprocess._text_encoding", return_value="cp1252") as locale_default:
                self.assertEqual(run_tool(command, log, timeout=10), text)
            self.assertEqual(log.read_bytes(), text.encode())
            locale_default.assert_not_called()

    def test_tool_timeout_and_diagnostic_budget_are_enforced_on_real_children(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with self.assertRaisesRegex(ValueError, "output exceeds"):
                run_tool([sys.executable, "-c", "import sys; sys.stdout.buffer.write(b'x'*4096)"],
                         root / "overflow.log", timeout=10, limit=128)
            self.assertEqual((root / "overflow.log").stat().st_size, 128)
            with self.assertRaises(subprocess.TimeoutExpired):
                run_tool([sys.executable, "-c", "import time; time.sleep(10)"],
                         root / "timeout.log", timeout=0.1)
            self.assertTrue((root / "timeout.log").is_file())

    def test_owned_cleanup_retries_only_typed_transient_windows_access_failure(self):
        root = Path(tempfile.mkdtemp(prefix="xylo-windows-packaging-"))
        (root / "uninstall.exe").write_bytes(b"owned fixture bytes")
        denied = PermissionError("Windows executable still held")
        denied.winerror = 5
        real_remove = shutil.rmtree
        attempts = []

        def attempt(path):
            attempts.append(path)
            if len(attempts) == 1:
                raise denied
            real_remove(path)

        try:
            with patch("release_windows_packaging_smoke.shutil.rmtree", side_effect=attempt), \
                    patch("release_windows_packaging_smoke.time.sleep") as pause:
                remove_fixture_directory(root)
            self.assertEqual(attempts, [root, root])
            pause.assert_called_once_with(0.1)
            self.assertFalse(root.exists())
        finally:
            if root.exists():
                real_remove(root)

    def test_permanent_cleanup_failure_keeps_last_os_cause_and_other_errors_fail_immediately(self):
        with tempfile.TemporaryDirectory(prefix="xylo-windows-packaging-") as directory:
            root = Path(directory)
            denied = PermissionError("Exact Windows sharing violation")
            denied.winerror = 32
            with patch("release_windows_packaging_smoke.shutil.rmtree", side_effect=denied) as remove, \
                    patch("release_windows_packaging_smoke.time.sleep") as pause:
                with self.assertRaises(TimeoutError) as caught:
                    remove_fixture_directory(root, timeout=0)
                self.assertIs(caught.exception.__cause__, denied)
                remove.assert_called_once_with(root)
                pause.assert_not_called()
            other = PermissionError("Different unqualified filesystem failure")
            with patch("release_windows_packaging_smoke.shutil.rmtree", side_effect=other), \
                    patch("release_windows_packaging_smoke.time.sleep") as pause:
                with self.assertRaises(PermissionError) as caught:
                    remove_fixture_directory(root)
                self.assertIs(caught.exception, other)
                pause.assert_not_called()


if __name__ == "__main__":
    unittest.main()
