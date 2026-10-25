"""Offline Windows release-policy fixtures; no Windows app execution is implied."""
import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from release_common import BINARIES, ROOT, build_env, digest, write_json
from release_windows import (MAX_OUTPUT, audit_windows_imports, forbidden_import,
                             llvm_readobj, parse_imports, run_llvm, validate_windows_imports)


def import_fixture(machine="COFF-x86-64", normal="KERNEL32.dll", delayed="USER32.dll"):
    # Field names/nesting match LLVM 18/20/21 COFFDumper::printCOFFImports,
    # including the nested Import blocks used only for delayed symbols.
    return f"""
File: C:/release directory/fixture.exe
Format: {machine}
Arch: {"aarch64" if machine == "COFF-ARM64" else "x86_64"}
AddressSize: 64bit
Import {{
  Name: {normal}
  ImportLookupTableRVA: 0x1000
  ImportAddressTableRVA: 0x2000
  Symbol: ExitProcess (0)
}}
DelayImport {{
  Name: {delayed}
  Attributes: 0x1
  ModuleHandle: 0x3000
  ImportAddressTable: 0x4000
  ImportNameTable: 0x5000
  BoundDelayImportTable: 0x0
  UnloadDelayImportTable: 0x0
  Import {{
    Symbol: MessageBoxW (0)
    Address: 0x6000
  }}
  Import {{
    Symbol: MessageBeep (0)
    Address: 0x6010
  }}
}}
"""


class BuildEnvironmentTests(unittest.TestCase):
    def test_both_windows_targets_preserve_encoded_arguments_and_request_static_crt(self):
        original = ["--cfg=custom", "-Clink-arg=/LIBPATH:C:\\SDK path\\lib"]
        for target in ("aarch64-pc-windows-msvc", "x86_64-pc-windows-msvc"):
            with self.subTest(target=target), patch.dict(os.environ, {
                "CARGO_ENCODED_RUSTFLAGS": "\x1f".join(original),
                "RUSTFLAGS": "-Ctarget-feature=-crt-static", "CARGO_PACKAGER_SIGN_PRIVATE_KEY": "fixture",
            }, clear=True):
                env = build_env(123, ROOT / "packaging/update-policy.json", target)
                flags = env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
                self.assertEqual(flags, original + [f"--remap-path-prefix={ROOT}=/workbench", "-Ctarget-feature=+crt-static"])
                self.assertEqual(env["SOURCE_DATE_EPOCH"], "123")
                self.assertEqual(env["CARGO_INCREMENTAL"], "0")
                self.assertNotIn("CARGO_PACKAGER_SIGN_PRIVATE_KEY", env)

    def test_rustflags_use_cargo_whitespace_rules_and_encoded_empty_wins(self):
        raw = '--cfg=custom -C link-arg="C:/SDK path/lib"'
        with patch.dict(os.environ, {"RUSTFLAGS": raw}, clear=True):
            flags = build_env(0, Path("policy"), "x86_64-pc-windows-msvc")["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
        self.assertEqual(flags[:-2], raw.split())
        with patch.dict(os.environ, {"CARGO_ENCODED_RUSTFLAGS": "", "RUSTFLAGS": "-Ctarget-feature=-crt-static"}, clear=True):
            flags = build_env(0, Path("policy"), "x86_64-pc-windows-msvc")["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
        self.assertEqual(len(flags), 2)

    def test_posix_and_compatibility_builds_preserve_flags_without_windows_policy(self):
        for target in (None, "aarch64-apple-darwin", "x86_64-unknown-linux-gnu"):
            with self.subTest(target=target), patch.dict(os.environ, {"RUSTFLAGS": "-C target-feature=-crt-static"}, clear=True):
                flags = build_env(0, Path("policy"), target)["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
            self.assertEqual(flags, ["-C", "target-feature=-crt-static", f"--remap-path-prefix={ROOT}=/workbench"])
        with patch.dict(os.environ, {"CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_RUSTFLAGS": "--cfg=target_specific"}, clear=True):
            flags = build_env(0, Path("policy"), "aarch64-pc-windows-msvc")["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
        self.assertEqual(flags[0], "--cfg=target_specific")

    def test_explicit_dynamic_crt_flags_are_rejected_instead_of_overwritten(self):
        conflicts = ["-Ctarget-feature=-crt-static", "-C target-feature=+sse2,-crt-static",
                     "-Ctarget-feature=+crt-static,-crt-static", "-Clink-arg=/MD", "-Clink-arg=/MDd",
                     "-C link-arg=/DEFAULTLIB:msvcrt.lib", "-Clink-arg=/defaultlib:msvcprt",
                     "-Clink-arg=/NODEFAULTLIB:LIBCMT.lib", "-Clink-arg=/NODEFAULTLIB:LIBCMTD.lib"]
        for flags in conflicts:
            for target in ("aarch64-pc-windows-msvc", "x86_64-pc-windows-msvc"):
                with self.subTest(flags=flags, target=target), patch.dict(os.environ, {"RUSTFLAGS": flags}, clear=True):
                    with self.assertRaisesRegex(ValueError, "static CRT|dynamic CRT"):
                        build_env(0, Path("policy"), target)


class LlvmImportTests(unittest.TestCase):
    def test_normal_and_nested_delayed_import_tables_are_kept_distinct(self):
        parsed = parse_imports(import_fixture())
        self.assertEqual(parsed["imports"], ["KERNEL32.dll"])
        self.assertEqual(parsed["delay_imports"], ["USER32.dll"])
        self.assertEqual(parsed["format"], "COFF-x86-64")
        self.assertEqual(parse_imports(import_fixture("COFF-ARM64"))["format"], "COFF-ARM64")

    def test_runtime_policy_covers_crt_cpp_and_three_openmp_families_but_allows_os(self):
        for name in ("VCRUNTIME140.dll", "vcruntime140_1.dll", "MSVCP140.dll", "msvcp140_atomic_wait.dll",
                     "MSVCR120.dll", "concrt140.dll", "vcomp140.dll", "libomp.dll", "libomp140.x86_64.dll",
                     "libiomp5md.dll", "libgomp-1.dll"):
            with self.subTest(name=name):
                self.assertTrue(forbidden_import(name))
        for name in ("KERNEL32.dll", "api-ms-win-crt-runtime-l1-1-0.dll", "ucrtbase.dll", "msvcrt.dll", "MSVCP_WIN.dll"):
            with self.subTest(name=name):
                self.assertFalse(forbidden_import(name))

    def test_malformed_truncated_and_oversized_import_output_fails_closed(self):
        source = import_fixture()
        invalid = [source.rstrip()[:-1], source + "}\n", source.replace("Name: KERNEL32.dll", "Name: ../evil.dll"),
                   source.replace("Name: KERNEL32.dll", "Name: KERNEL32.dll\nName: other.dll"),
                   source.replace("ImportAddressTable:", "UnknownTable:"), source.replace("DelayImport {", "Export {"),
                   source.replace("Format: COFF-x86-64", "Format: ELF64-x86-64"),
                   source.replace("AddressSize: 64bit\n", ""), source + "Arch: x86_64\n",
                   "File: x\nFormat: COFF-x86-64\nArch: x86_64\nAddressSize: 64bit\n",
                   "x" * (MAX_OUTPUT + 1)]
        for output in invalid:
            with self.subTest(output=output[:90]), self.assertRaises(ValueError):
                parse_imports(output)

    def test_native_tool_output_is_bounded_and_timeouts_reap_the_process(self):
        result = run_llvm([sys.executable, "-c", "import sys; print('stdout'); print('stderr', file=sys.stderr)"], timeout=5)
        self.assertEqual((result.returncode, result.stdout.strip(), result.stderr.strip()), (0, "stdout", "stderr"))
        for stream in ("stdout", "stderr"):
            command = f"import sys; sys.{stream}.buffer.write(b'x'*100000); sys.{stream}.flush()"
            with self.subTest(stream=stream), self.assertRaisesRegex(ValueError, "budget"):
                run_llvm([sys.executable, "-c", command], timeout=5, limit=128)
        with self.assertRaises(subprocess.TimeoutExpired):
            run_llvm([sys.executable, "-c", "import time; time.sleep(30)"], timeout=0.1)

    def test_tool_resolution_requires_an_installed_explicit_path_or_native_search(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            selected = root / "custom/llvm-readobj.exe"
            selected.parent.mkdir()
            selected.write_bytes(b"tool fixture")
            with patch.dict(os.environ, {"WORKBENCH_LLVM_READOBJ": str(selected)}, clear=True):
                self.assertEqual(llvm_readobj(), selected.resolve())
            with patch.dict(os.environ, {"WORKBENCH_LLVM_READOBJ": str(root / "missing")}, clear=True), patch("release_windows.shutil.which", return_value=str(selected)):
                with self.assertRaisesRegex(ValueError, "WORKBENCH_LLVM_READOBJ"):
                    llvm_readobj()
            with patch.dict(os.environ, {}, clear=True), patch("release_windows.shutil.which", return_value=str(selected)):
                self.assertEqual(llvm_readobj(), selected.resolve())
            bundled = root / "Program Files/LLVM/bin/llvm-readobj.exe"
            bundled.parent.mkdir(parents=True)
            bundled.write_bytes(b"Program Files tool fixture")
            with patch.dict(os.environ, {"ProgramFiles": str(root / "Program Files")}, clear=True), patch("release_windows.shutil.which", return_value=None):
                self.assertEqual(llvm_readobj(), bundled.resolve())
            with patch.dict(os.environ, {}, clear=True), patch("release_windows.shutil.which", return_value=None):
                with self.assertRaisesRegex(ValueError, "requires installed LLVM"):
                    llvm_readobj()


class ImportAuditTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.binaries, self.native = self.root / "binaries", self.root / "native"
        self.paths = [self.binaries / (name + ".exe") for name in BINARIES]
        self.paths.append(self.native / "whisper-runtime/runtime/whisper-cli.exe")
        for index, path in enumerate(self.paths):
            path.parent.mkdir(parents=True, exist_ok=True)
            # Controlled tool fixtures, not executable PE/Windows acceptance.
            path.write_bytes(b"MZ" + bytes([index]) * 32)
        self.tool = self.root / "llvm-readobj.exe"
        self.tool.write_bytes(b"controlled tool identity")
        self.report_path = self.root / "report.json"
        self.machine, self.normal, self.delayed = "COFF-x86-64", "KERNEL32.dll", "USER32.dll"

    def tool_result(self, args, **kwargs):
        if args[-1] == "--version":
            return subprocess.CompletedProcess(args, 0, "LLVM version 21.1.8\n", "")
        self.assertEqual(args[1], "--coff-imports")
        self.assertIn(Path(args[2]), self.paths)
        self.assertEqual(kwargs["timeout"], 60)
        return subprocess.CompletedProcess(args, 0, import_fixture(self.machine, self.normal, self.delayed), "")

    def audit(self, target="x86_64-pc-windows-msvc", effect=None):
        with patch("release_windows.llvm_readobj", return_value=self.tool), patch("release_windows.run_llvm", side_effect=effect or self.tool_result):
            return audit_windows_imports(self.binaries, self.native, self.report_path, target)

    def installed(self, architecture="x86_64"):
        return [{"name": name, "sha256": digest(path), "size_bytes": path.stat().st_size, "architecture": architecture}
                for name, path in zip((*BINARIES, "whisper-cli"), self.paths)]

    def test_both_targets_record_all_four_final_hashes_and_round_trip_installed_validation(self):
        for target, architecture, machine in [("x86_64-pc-windows-msvc", "x86_64", "COFF-x86-64"),
                                               ("aarch64-pc-windows-msvc", "aarch64", "COFF-ARM64")]:
            with self.subTest(target=target):
                self.machine = machine
                report = self.audit(target)
                self.assertTrue(report["passed"])
                self.assertEqual(report["tool"]["sha256"], digest(self.tool))
                self.assertEqual(len(report["binaries"]), 4)
                self.assertEqual(validate_windows_imports(self.report_path, target, self.installed(architecture)), report)
                with self.assertRaises(FileExistsError):
                    self.audit(target)
                self.report_path.unlink()

    def test_forbidden_normal_and_delayed_runtime_tables_retain_failed_identity_proof(self):
        for field, dll in [("normal", "VCRUNTIME140_1.dll"), ("delayed", "libgomp-1.dll")]:
            with self.subTest(field=field):
                setattr(self, field, dll)
                with self.assertRaisesRegex(ValueError, "runtime audit failed"):
                    self.audit()
                report = json.loads(self.report_path.read_text())
                self.assertFalse(report["passed"])
                self.assertEqual(len(report["binaries"]), 4)
                for item, installed in zip(report["binaries"], self.installed()):
                    self.assertEqual(item["sha256"], installed["sha256"])
                    self.assertEqual(item["forbidden_runtime_imports"], [dll])
                self.report_path.unlink()
                setattr(self, field, "KERNEL32.dll" if field == "normal" else "USER32.dll")

    def test_failed_tool_machine_missing_input_and_changed_bytes_cannot_pass(self):
        def tool_failure(args, **kwargs):
            if args[-1] == "--version":
                return self.tool_result(args, **kwargs)
            return subprocess.CompletedProcess(args, 1, "", "invalid PE input")

        def changed(args, **kwargs):
            result = self.tool_result(args, **kwargs)
            if args[-1] != "--version":
                with Path(args[2]).open("ab") as stream:
                    stream.write(b"changed during inspection")
            return result

        for effect in (tool_failure, changed):
            with self.subTest(effect=effect.__name__), self.assertRaisesRegex(ValueError, "runtime audit failed"):
                self.audit(effect=effect)
            self.assertFalse(json.loads(self.report_path.read_text())["passed"])
            self.report_path.unlink()
        self.machine = "COFF-ARM64"
        with self.assertRaisesRegex(ValueError, "machine differs"):
            self.audit()
        self.report_path.unlink()
        self.paths[-1].unlink()
        with self.assertRaisesRegex(ValueError, "regular file"):
            self.audit()

    def test_report_consumer_rejects_rewritten_flags_imports_identities_and_shapes(self):
        good = self.audit()
        installed = self.installed()
        mutations = [lambda value: value.update(passed=False), lambda value: value.update(target="aarch64-pc-windows-msvc"),
                     lambda value: value.update(schema=True), lambda value: value["binaries"].pop(),
                     lambda value: value["binaries"][0].update(sha256="0" * 64),
                     lambda value: value["binaries"][0].update(size_bytes=1),
                     lambda value: value["binaries"][0].update(error="inspection failed"),
                     lambda value: value["binaries"][0].update(delay_imports=["MSVCP140.dll"], forbidden_runtime_imports=[]),
                     lambda value: value["binaries"][0].update(imports=["../../KERNEL32.dll"]),
                     lambda value: value["binaries"][0].update(imports="KERNEL32.dll"),
                     lambda value: value["binaries"][0].update(report_sha256="unbound"),
                     lambda value: value["tool"].update(sha256="unbound")]
        for mutate in mutations:
            value = copy.deepcopy(good)
            mutate(value)
            write_json(self.report_path, value)
            with self.subTest(mutation=mutations.index(mutate)), self.assertRaises(ValueError):
                validate_windows_imports(self.report_path, "x86_64-pc-windows-msvc", installed)
        self.report_path.write_bytes(b'{"schema":1,"schema":1}')
        with self.assertRaisesRegex(ValueError, "repeats"):
            validate_windows_imports(self.report_path, "x86_64-pc-windows-msvc", installed)
        self.report_path.write_bytes(b" " * (MAX_OUTPUT + 1))
        with self.assertRaisesRegex(ValueError, "budget"):
            validate_windows_imports(self.report_path, "x86_64-pc-windows-msvc", installed)


if __name__ == "__main__":
    unittest.main()
