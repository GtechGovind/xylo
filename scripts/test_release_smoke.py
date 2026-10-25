"""Artifact confinement regressions; Windows lexical fixtures are not OS smoke."""
import os
from pathlib import Path, PureWindowsPath
import tempfile
import unittest
from unittest.mock import patch

from release_smoke import artifact_relative_path, confined_artifact


class ArtifactConfinementTests(unittest.TestCase):
    def test_windows_normal_and_extended_drive_names_share_one_comparison_namespace(self):
        roots = [r"C:\Temp\Profile\artifacts", r"\\?\C:\Temp\Profile\artifacts"]
        artifacts = [r"c:\temp\profile\ARTIFACTS\nested\report.docx",
                     r"\\?\c:\temp\profile\ARTIFACTS\nested\report.docx"]
        for root in roots:
            for artifact in artifacts:
                with self.subTest(root=root, artifact=artifact):
                    relative = artifact_relative_path(PureWindowsPath(artifact), PureWindowsPath(root))
                    self.assertEqual(relative, PureWindowsPath(r"nested\report.docx"))

    def test_windows_normal_and_extended_unc_names_share_one_comparison_namespace(self):
        roots = [r"\\server\share\Profile\artifacts", r"\\?\UNC\server\share\Profile\artifacts"]
        artifacts = [r"\\SERVER\SHARE\profile\ARTIFACTS\report.docx",
                     r"\\?\UNC\SERVER\SHARE\profile\ARTIFACTS\report.docx"]
        for root in roots:
            for artifact in artifacts:
                with self.subTest(root=root, artifact=artifact):
                    self.assertEqual(artifact_relative_path(PureWindowsPath(artifact), PureWindowsPath(root)),
                                     PureWindowsPath("report.docx"))

    def test_windows_sibling_escape_parent_traversal_and_different_volumes_are_rejected(self):
        root = PureWindowsPath(r"C:\Temp\Profile\artifacts")
        for artifact in [r"C:\Temp\Profile\artifacts-other\report.docx",
                         r"\\?\C:\Temp\Profile\elsewhere\report.docx",
                         r"C:\Temp\Profile\artifacts\..\elsewhere\report.docx",
                         r"D:\Temp\Profile\artifacts\report.docx",
                         r"\\server\share\Profile\artifacts\report.docx",
                         r"\\.\C:\Temp\Profile\artifacts\report.docx",
                         r"\\?\GLOBALROOT\Device\HarddiskVolume1\report.docx",
                         r"C:artifacts\report.docx"]:
            with self.subTest(artifact=artifact), self.assertRaises(ValueError):
                artifact_relative_path(PureWindowsPath(artifact), root)
        for artifact in [r"\\server\other-share\artifacts\report.docx",
                         r"\\other-server\share\artifacts\report.docx"]:
            with self.subTest(artifact=artifact), self.assertRaises(ValueError):
                artifact_relative_path(PureWindowsPath(artifact), PureWindowsPath(r"\\server\share\artifacts"))

    def test_actual_regular_files_are_confined_and_directories_missing_files_and_siblings_reject(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "artifacts"
            nested = root / "nested"
            nested.mkdir(parents=True)
            artifact = nested / "report.docx"
            artifact.write_bytes(b"Actual temporary artifact")
            self.assertEqual(confined_artifact(artifact, root), artifact.resolve(strict=True))
            with self.assertRaises(ValueError):
                confined_artifact(nested, root)
            with self.assertRaises(FileNotFoundError):
                confined_artifact(root / "absent.docx", root)
            sibling = Path(temporary) / "artifacts-other"
            sibling.mkdir()
            outside = sibling / "report.docx"
            outside.write_bytes(b"Outside")
            with self.assertRaises(ValueError):
                confined_artifact(outside, root)
            # POSIX exercises actual symlink resolution. Windows namespace
            # cases above require no symlink privilege and claim no OS run.
            if os.name != "nt":
                (root / "escaped.docx").symlink_to(outside)
                (root / "escaped-folder").symlink_to(sibling, target_is_directory=True)
                for escaped in [root / "escaped.docx", root / "escaped-folder/report.docx"]:
                    with self.subTest(escaped=escaped), self.assertRaises(ValueError):
                        confined_artifact(escaped, root)

    def test_physical_parent_identity_is_required_after_namespace_comparison(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            artifact = root / "report.docx"
            artifact.write_bytes(b"Actual temporary artifact")
            with patch.object(Path, "samefile", return_value=False):
                with self.assertRaisesRegex(ValueError, "physical artifact"):
                    confined_artifact(artifact, root)


if __name__ == "__main__":
    unittest.main()
