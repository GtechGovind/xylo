"""Offline package-smoke regressions; mocked launchers are not native OS smoke."""
import copy
import json
import os
from pathlib import Path, PureWindowsPath
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from release_common import BINARIES, digest, workspace_version, write_json
from release_smoke import (artifact_relative_path, confined_artifact, install_nsis, launch_desktop,
                           nsis_command_line, validate_debian_control, validate_packaged_windows_imports)
from release_windows import PHASE


class PackagedWindowsImportTests(unittest.TestCase):
    def inputs(self, root):
        records, inspections = [], []
        for name in (*BINARIES, "whisper-cli"):
            binary = root / (name + ".exe")
            binary.write_bytes(b"Controlled installed-byte fixture: " + name.encode())
            record = {"name": name, "sha256": digest(binary), "size_bytes": binary.stat().st_size, "architecture": "aarch64"}
            records.append(record)
            inspections.append({**record, "format": "COFF-ARM64", "report_sha256": "b" * 64,
                                "imports": ["KERNEL32.dll", "ucrtbase.dll"], "delay_imports": [], "forbidden_runtime_imports": []})
        proof = {"schema": 1, "target": "aarch64-pc-windows-msvc", "phase": PHASE, "passed": True,
                 "tool": {"sha256": "c" * 64, "version": "Controlled LLVM fixture"}, "binaries": inspections}
        output, resources = root / "output", root / "installed"
        packaged = resources / "licenses/dependencies/windows-pe-imports.json"
        retained = output / "release-metadata/windows-pe-imports.json"
        write_json(packaged, proof)
        write_json(retained, proof)
        return output, resources, records, proof, packaged, retained

    def test_packaged_and_retained_reports_bind_the_actual_installed_four_hashes(self):
        with tempfile.TemporaryDirectory() as temporary:
            output, resources, records, _, _, _ = self.inputs(Path(temporary))
            validate_packaged_windows_imports(output, resources, "aarch64-pc-windows-msvc", records)
            records[0]["sha256"] = "d" * 64
            with self.assertRaisesRegex(ValueError, "installed executable"):
                validate_packaged_windows_imports(output, resources, "aarch64-pc-windows-msvc", records)

    def test_missing_failed_forbidden_and_different_packaged_reports_cannot_qualify(self):
        with tempfile.TemporaryDirectory() as temporary:
            output, resources, records, good, packaged, retained = self.inputs(Path(temporary))
            for change in ("missing", "failed", "forbidden", "different"):
                proof = copy.deepcopy(good)
                if change == "missing":
                    packaged.unlink()
                elif change == "failed":
                    proof["passed"] = False
                    write_json(packaged, proof)
                elif change == "forbidden":
                    proof["binaries"][0]["delay_imports"] = ["VCRUNTIME140.dll"]
                    write_json(packaged, proof)
                else:
                    proof["tool"]["version"] = "Another valid tool identity"
                    write_json(packaged, proof)
                with self.subTest(change=change), self.assertRaises(ValueError):
                    validate_packaged_windows_imports(output, resources, "aarch64-pc-windows-msvc", records)
                write_json(packaged, good)
            self.assertEqual(json.loads(retained.read_text()), good)


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


class NsisCommandTests(unittest.TestCase):
    def test_spaced_install_path_is_the_final_unquoted_tail_with_a_safely_quoted_executable(self):
        executable = PureWindowsPath(r"C:\Release Builds\Xylo_0.1.0_arm64-setup.exe")
        destination = PureWindowsPath(r"C:\Users\Runner Admin\Temp\A & B\installed")
        command = nsis_command_line(executable, destination)
        self.assertEqual(command, r'"C:\Release Builds\Xylo_0.1.0_arm64-setup.exe" /S /D=C:\Users\Runner Admin\Temp\A & B\installed')
        self.assertEqual(command.split(" /D=", 1)[1], str(destination))
        self.assertNotIn('"', command.split(" /D=", 1)[1])
        self.assertEqual(nsis_command_line(
            PureWindowsPath(r"\\?\UNC\server\share\Release Builds\Xylo.exe"),
            PureWindowsPath(r"\\server\share\Install Folder")),
            r'"\\server\share\Release Builds\Xylo.exe" /S /D=\\server\share\Install Folder')

    def test_execution_uses_the_raw_command_without_a_shell_and_refuses_non_windows(self):
        executable = PureWindowsPath(r"C:\Release Builds\Xylo.exe")
        destination = PureWindowsPath(r"C:\Temp\A B\installed")
        with patch("release_smoke.subprocess.run") as run:
            with patch("release_smoke.os.name", "nt"):
                install_nsis(executable, destination)
            run.assert_called_once_with(
                r'"C:\Release Builds\Xylo.exe" /S /D=C:\Temp\A B\installed',
                executable=str(executable), shell=False, check=True, timeout=180)
            run.reset_mock()
            with patch("release_smoke.os.name", "posix"):
                with self.assertRaisesRegex(ValueError, "must run on Windows"):
                    install_nsis(executable, destination)
            run.assert_not_called()

    def test_relative_device_traversal_quote_newline_and_nul_paths_reject_before_execution(self):
        executable = PureWindowsPath(r"C:\release\Xylo.exe")
        destination = PureWindowsPath(r"C:\Temp\installed")
        unsafe = [r"relative\path", r"C:relative", r"\\.\C:\device", r"\\?\GLOBALROOT\Device\volume",
                  r"C:\Temp\..\outside", 'C:\\Temp\\bad"name', "C:\\Temp\\bad\nname",
                  "C:\\Temp\\bad\rname", "C:\\Temp\\bad\0name"]
        with patch("release_smoke.os.name", "nt"), patch("release_smoke.subprocess.run") as run:
            for value in unsafe:
                for first, second in [(PureWindowsPath(value), destination),
                                      (executable, PureWindowsPath(value))]:
                    with self.subTest(first=first, second=second), self.assertRaises(ValueError):
                        install_nsis(first, second)
            run.assert_not_called()


class DesktopLauncherTests(unittest.TestCase):
    def test_each_platform_uses_its_real_entry_point_and_keeps_the_isolated_profile(self):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = Path(temporary).resolve()
            binaries = scratch / "binaries"
            data = scratch / "isolated-profile"
            appdir = scratch / "squashfs-root"
            appdir.mkdir()
            apprun = appdir / "AppRun"
            apprun.write_bytes(b"Controlled launcher fixture; not an ELF executable")
            log = object()
            for target, launcher in [
                ("aarch64-apple-darwin", binaries / "agent-workbench"),
                ("x86_64-pc-windows-msvc", binaries / "agent-workbench.exe"),
                ("aarch64-unknown-linux-gnu", apprun),
            ]:
                with self.subTest(target=target), patch("release_smoke.subprocess.Popen") as child:
                    with patch("release_smoke.os.access", return_value=True) as access:
                        process = launch_desktop(binaries, target, scratch, data, log)
                    self.assertIs(process, child.return_value)
                    self.assertEqual(child.call_args.args[0], [str(launcher), "--data-dir", str(data)])
                    self.assertIs(child.call_args.kwargs["stdout"], log)
                    if "linux" in target:
                        access.assert_called_once_with(apprun, os.X_OK)
                        self.assertEqual(child.call_args.kwargs["env"]["APPDIR"], str(appdir))
                    else:
                        access.assert_not_called()
                        self.assertIsNone(child.call_args.kwargs["env"])

    def test_linux_missing_nonregular_nonexecutable_and_symlink_launchers_never_execute(self):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = Path(temporary).resolve()
            appdir = scratch / "squashfs-root"
            appdir.mkdir()
            apprun = appdir / "AppRun"
            with patch("release_smoke.subprocess.Popen") as child:
                with self.assertRaises(FileNotFoundError):
                    launch_desktop(scratch / "bin", "x86_64-unknown-linux-gnu", scratch, scratch / "data", None)
                apprun.mkdir()
                with self.assertRaisesRegex(ValueError, "regular executable"):
                    launch_desktop(scratch / "bin", "x86_64-unknown-linux-gnu", scratch, scratch / "data", None)
                apprun.rmdir()
                apprun.write_bytes(b"Controlled nonexecutable file")
                with patch("release_smoke.os.access", return_value=False):
                    with self.assertRaisesRegex(ValueError, "regular executable"):
                        launch_desktop(scratch / "bin", "x86_64-unknown-linux-gnu", scratch, scratch / "data", None)
                if os.name != "nt":
                    apprun.unlink()
                    outside = scratch / "outside"
                    outside.write_bytes(b"Outside launcher")
                    outside.chmod(0o755)
                    apprun.symlink_to(outside)
                    with self.assertRaisesRegex(ValueError, "regular executable"):
                        launch_desktop(scratch / "bin", "x86_64-unknown-linux-gnu", scratch, scratch / "data", None)
                child.assert_not_called()


class DebianControlTests(unittest.TestCase):
    def test_each_architecture_and_folded_maintainer_match_the_resolved_configuration(self):
        authors = ["First Author <first@example.invalid>", "Second Author <second@example.invalid>"]
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            package = output / "xylo.deb"
            package.write_bytes(b"Controlled package fixture; dpkg is mocked")
            for target, architecture in [("aarch64-unknown-linux-gnu", "arm64"),
                                         ("x86_64-unknown-linux-gnu", "amd64")]:
                fields = (f"Package: xylo\nVersion: {workspace_version()}\nArchitecture: {architecture}\n"
                          f"Maintainer: {authors[0]},\n {authors[1]}\n")
                with self.subTest(target=target), patch("release_smoke.subprocess.run") as run:
                    run.return_value = subprocess.CompletedProcess([], 0, stdout=fields)
                    validate_debian_control(output, target, {"authors": authors})
                    run.assert_called_once_with(
                        ["dpkg-deb", "--field", str(package), "Package", "Version", "Architecture", "Maintainer"],
                        capture_output=True, text=True, encoding="utf-8", check=True, timeout=30)

    def test_empty_mismatched_missing_and_duplicate_fields_or_unusable_authors_are_refused(self):
        valid = f"Package: xylo\nVersion: {workspace_version()}\nArchitecture: amd64\nMaintainer: Test Author\n"
        invalid = [valid.replace("Maintainer: Test Author", "Maintainer:"),
                   valid.replace("Architecture: amd64", "Architecture: arm64"),
                   valid.replace("Package: xylo", "Package: other"),
                   valid.replace(f"Version: {workspace_version()}", "Version: 999.0.0"),
                   valid.replace("Maintainer: Test Author\n", ""),
                   valid + "Maintainer: Test Author\n", valid + "Unrequested: value\n",
                   valid.replace("Package: xylo", "Package xylo"), " stray continuation\n" + valid]
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            (output / "xylo.deb").write_bytes(b"Controlled package fixture")
            with patch("release_smoke.subprocess.run") as run:
                for fields in invalid:
                    with self.subTest(fields=fields), self.assertRaises(ValueError):
                        run.return_value = subprocess.CompletedProcess([], 0, stdout=fields)
                        validate_debian_control(output, "x86_64-unknown-linux-gnu", {"authors": ["Test Author"]})
                run.reset_mock()
                for authors in [None, [], "Author", [""], ["Author", 1], ["Author\nInjected"]]:
                    with self.subTest(authors=authors), self.assertRaises(ValueError):
                        validate_debian_control(output, "x86_64-unknown-linux-gnu", {"authors": authors})
                run.assert_not_called()

    def test_failed_inspection_and_missing_or_multiple_packages_never_produce_success(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            configuration = {"authors": ["Test Author"]}
            target = "x86_64-unknown-linux-gnu"
            with patch("release_smoke.subprocess.run") as run:
                with self.assertRaisesRegex(ValueError, "exactly one"):
                    validate_debian_control(output, target, configuration)
                first = output / "first.deb"
                first.write_bytes(b"Controlled package fixture")
                second = output / "second.deb"
                second.write_bytes(b"Ambiguous package")
                with self.assertRaisesRegex(ValueError, "exactly one"):
                    validate_debian_control(output, target, configuration)
                run.assert_not_called()
                second.unlink()
                run.side_effect = subprocess.CalledProcessError(2, ["dpkg-deb"], stderr="Invalid package")
                with self.assertRaises(subprocess.CalledProcessError):
                    validate_debian_control(output, target, configuration)
                self.assertEqual(run.call_count, 1)


if __name__ == "__main__":
    unittest.main()
