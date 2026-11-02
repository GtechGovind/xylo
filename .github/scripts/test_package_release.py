"""Release signing decisions fail closed and do not alter the source config."""
import unittest
from subprocess import CompletedProcess
from unittest.mock import patch
from package_release import configure, linux_dependencies

class SigningTests(unittest.TestCase):
    def test_missing_apple_credentials_still_seal_the_macos_bundle(self):
        config = {"macos": {"signingIdentity": "old"}, "licenseFile": "LICENSE"}
        result, apple, minisign = configure(config, {"MINISIGN_KEY": "key"}, "Darwin")
        self.assertFalse(apple)
        self.assertTrue(minisign)
        self.assertEqual(result["macos"]["signingIdentity"], "-")
        self.assertEqual(config["macos"]["signingIdentity"], "old")
        self.assertNotIn("licenseFile", result)
        self.assertEqual(configure(config, {}, "Windows")[0]["licenseFile"], "LICENSE")
        self.assertNotIn("signingIdentity", configure(config, {}, "Linux")[0]["macos"])

    def test_complete_apple_credentials_enable_signing_only_on_macos(self):
        environment = dict.fromkeys(("APPLE_CERTIFICATE", "APPLE_SIGNING_IDENTITY", "APPLE_ID", "APPLE_PASSWORD", "APPLE_TEAM_ID"), "configured")
        config, apple, minisign = configure({}, environment, "Darwin")
        self.assertTrue(apple)
        self.assertFalse(minisign)
        self.assertEqual(config["macos"]["signingIdentity"], "configured")
        self.assertFalse(configure({}, environment, "Linux")[1])


class LinuxDependenciesTests(unittest.TestCase):
    def test_abi_versions_and_dynamic_runtime_requirements_are_preserved(self):
        config = {"binariesDir": "target/release", "binaries": [{"path": "xylo"}, {"path": "xylo-worker"}],
            "deb": {"depends": ["ca-certificates"]}}
        with patch("package_release.subprocess.run", return_value=CompletedProcess([], 0,
            "shlibs:Depends=libc6 (>= 2.39), libgcc-s1 (>= 4.2)\n")) as run:
            linux_dependencies(config)
        self.assertIn("libc6 (>= 2.39)", config["deb"]["depends"])
        for name in ("libvulkan1", "libwayland-client0", "bubblewrap", "socat", "ripgrep", "ca-certificates"):
            self.assertIn(name, config["deb"]["depends"])
        self.assertEqual(len([arg for arg in run.call_args.args[0] if arg.startswith("-e")]), 2)

    def test_missing_dependency_metadata_stops_packaging(self):
        with patch("package_release.subprocess.run", return_value=CompletedProcess([], 0, "")):
            with self.assertRaisesRegex(ValueError, "runtime dependencies"):
                linux_dependencies({"binariesDir": "target/release", "binaries": [{"path": "xylo"}]})

if __name__ == "__main__":
    unittest.main()
