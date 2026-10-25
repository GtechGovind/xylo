#!/usr/bin/env python3
"""Run the actual build script offline; fixture tools do not compile or sign code.

macOS deliberately uses its /bin/bash 3.2, matching the native release runner.
Source hash verification is simulated; published fixture file hashes are real.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
from release_common import ROOT


REVISION = "d1be6fde11ac6e0407606b4e42fe72d34add8037"
SOURCE_SHA = "645445da884df6dbd52b30f82d7be7c01d1767c938714e477e786b88d529e9f4"
if sys.platform == "darwin":
    BASH = "/bin/bash"
elif os.name == "nt":
    # Prefer Git for Windows rather than an unrelated WSL bash.exe shim.
    git_bash = Path(os.environ.get("ProgramFiles", "C:/Program Files")) / "Git/bin/bash.exe"
    BASH = str(git_bash) if git_bash.is_file() else shutil.which("bash")
else:
    BASH = shutil.which("bash")

# One fixture driver handles only the build script's five external native tools.
# An unexpected invocation fails rather than falling through to the network or a
# compiler installed on the machine running the tests.
TOOL_DRIVER = r'''
import hashlib
import json
import os
from pathlib import Path
import sys

tool, *args = sys.argv[1:]
with Path(os.environ["FIXTURE_LOG"]).open("a", encoding="utf-8") as log:
    log.write(json.dumps({"tool": tool, "args": args}) + "\n")
failure = os.environ.get("FIXTURE_FAILURE", "")
if tool == "uname" and args == ["-s"]:
    print(os.environ["FIXTURE_OS"])
elif tool == "curl":
    Path(args[args.index("-o") + 1]).write_bytes(b"offline source archive fixture")
elif tool == "tar":
    source = Path(args[args.index("-C") + 1]) / ("whisper.cpp-" + os.environ["FIXTURE_REVISION"])
    source.mkdir()
    (source / "LICENSE").write_bytes(b"offline upstream license fixture\n")
elif tool == "cmake" and args[:2] == ["-E", "sha256sum"]:
    path = Path(args[2])
    if path.name == "source.tar.gz":
        value = "0" * 64 if failure == "source_sha" else os.environ["FIXTURE_SOURCE_SHA"]
    else:
        value = hashlib.sha256(path.read_bytes()).hexdigest()
    print(value + "  " + str(path))
elif tool == "cmake" and args[0] == "-S":
    if failure == "configure":
        sys.exit(7)
    if os.environ["FIXTURE_OS"].startswith("MINGW"):
        assert "-DCMAKE_POLICY_DEFAULT_CMP0091=NEW" in args
        assert "-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded" in args
        assert "-DGGML_OPENMP=OFF" in args
        if os.environ["CARGO_BUILD_TARGET"].startswith("aarch64"):
            # Model the exact pinned ggml precondition: MSVC proper rejects
            # GGML_NATIVE=OFF on ARM, while ClangCL supports a generic ISA.
            assert args[args.index("-T") + 1] == "ClangCL"
            assert "-DGGML_CPU_ARM_ARCH=armv8-a" in args
    Path(args[args.index("-B") + 1]).mkdir()
elif tool == "cmake" and args[0] == "--build":
    if failure == "build":
        sys.exit(8)
    windows = os.environ["FIXTURE_OS"].startswith("MINGW")
    output = Path(args[1]) / ("bin/Release" if windows else "bin")
    output.mkdir(parents=True)
    (output / ("whisper-cli.exe" if windows else "whisper-cli")).write_bytes(b"offline binary fixture\n")
elif tool == "codesign" and args[:3] == ["--force", "--sign", "-"]:
    if failure == "codesign":
        sys.exit(9)
    path = Path(args[3])
    path.write_bytes(path.read_bytes() + b"fixture signature\n")
elif tool == "codesign" and args[:2] == ["--verify", "--strict"]:
    assert Path(args[2]).read_bytes().endswith(b"fixture signature\n")
else:
    raise AssertionError((tool, args))
'''


@unittest.skipUnless(BASH, "A native Bash installation is required for build-tool fixtures")
class NativeAssetBuildTests(unittest.TestCase):
    def run_build(self, platform="Darwin", target="x86_64-apple-darwin", failure="", interrupt=False):
        # Spaces exercise the same quoted tool/driver paths used by Git Bash
        # with a native Python installation on a Windows hosted runner.
        temporary = tempfile.TemporaryDirectory(prefix="xylo native build test ")
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        tools, scratch, output = root / "tools", root / "scratch", root / "output"
        tools.mkdir()
        scratch.mkdir()
        driver = root / "tools.py"
        driver.write_text(TOOL_DRIVER, encoding="utf-8", newline="\n")
        self.assertNotIn(b"\r", driver.read_bytes())
        for tool in ("curl", "cmake", "tar", "uname", "codesign"):
            wrapper = tools / tool
            wrapper.write_text('#!/usr/bin/env bash\nexec "$FIXTURE_PYTHON" "$FIXTURE_DRIVER" '
                               + tool + ' "$@"\n', encoding="utf-8", newline="\n")
            self.assertTrue(wrapper.read_bytes().startswith(b"#!/usr/bin/env bash\n"))
            self.assertNotIn(b"\r", wrapper.read_bytes())
            wrapper.chmod(0o755)
        log = root / "commands.jsonl"
        env = os.environ.copy()
        for key in ("BASH_ENV", "ENV", "MSYS_NO_PATHCONV", "MSYS2_ARG_CONV_EXCL", "MSYS2_ENV_CONV_EXCL"):
            env.pop(key, None)
        env.update(PATH=str(tools) + os.pathsep + env["PATH"], TMPDIR=scratch.as_posix(),
                   FIXTURE_TOOLS=tools.as_posix(), FIXTURE_DRIVER=driver.as_posix(),
                   FIXTURE_PYTHON=Path(sys.executable).as_posix(), FIXTURE_LOG=log.as_posix(),
                   FIXTURE_REVISION=REVISION, FIXTURE_SOURCE_SHA=SOURCE_SHA,
                   FIXTURE_OS=platform, FIXTURE_FAILURE=failure,
                   FIXTURE_NATIVE_WINDOWS="1" if os.name == "nt" else "0",
                   FIXTURE_BUILD_SCRIPT=(ROOT / "scripts/native-assets-build.sh").as_posix(),
                   CARGO_BUILD_TARGET=target, NATIVE_BUILD_JOBS="2")
        if interrupt:
            # Inject a premature parent-shell exit after its cleanup trap exists.
            # Bash 3.2's empty-array nounset abort can itself report status zero;
            # newer Bash versions instead reach the explicit zero exit here.
            startup = root / "interrupted.bash"
            startup.write_text('''cmake() {
  if [[ "$1" == -S ]]; then
    local omitted=()
    printf '%s\\n' "${omitted[@]}"
    exit 0
  fi
  "$FIXTURE_TOOLS/cmake" "$@"
}
''', encoding="utf-8", newline="\n")
            self.assertNotIn(b"\r", startup.read_bytes())
            env["BASH_ENV"] = startup.as_posix()
        # Git/bin/bash.exe prepends Git's own tools to PATH. Restore stub priority
        # inside Bash, check every tool, then exec its actual Bash executable.
        # cygpath makes the native Windows tools directory one POSIX PATH entry;
        # the Python executable/driver retain quoted C:/ paths for native Python.
        launcher = '''set -euo pipefail
fixture_tools_path="$FIXTURE_TOOLS"
if [[ "$FIXTURE_NATIVE_WINDOWS" == 1 ]]; then
  fixture_tools_path=$(cygpath -u "$FIXTURE_TOOLS")
fi
export PATH="$fixture_tools_path:$PATH"
for tool in curl cmake tar uname codesign; do
  [[ "$(type -P "$tool")" == "$fixture_tools_path/$tool" ]] || exit 91
done
exec "$BASH" "$FIXTURE_BUILD_SCRIPT" output
'''
        result = subprocess.run([BASH, "-c", launcher],
                                cwd=root, env=env, capture_output=True, text=True, timeout=15)
        self.assertEqual(list(scratch.iterdir()), [], "Build scratch must be cleaned on every exit")
        calls = [json.loads(line) for line in log.read_text(encoding="utf-8").splitlines()] if log.exists() else []
        return result, output, calls

    def assert_manifest(self, result, output, executable):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Verified native component prepared", result.stdout)
        manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual((manifest["revision"], manifest["source_sha256"]), (REVISION, SOURCE_SHA))
        self.assertEqual(manifest["cpu_policy"], "portable-static-x64-sse2-arm-baseline")
        self.assertEqual({item["path"] for item in manifest["files"]},
                         {"runtime/" + executable, "notices/whisper-LICENSE"})
        for item in manifest["files"]:
            data = (output / item["path"]).read_bytes()
            self.assertEqual((item["size_bytes"], item["sha256"]),
                             (len(data), hashlib.sha256(data).hexdigest()))

    def test_macos_bash_has_no_empty_arguments_and_hashes_final_signed_bytes(self):
        if sys.platform == "darwin":
            version = subprocess.run([BASH, "--version"], capture_output=True, text=True, check=True, timeout=5)
            self.assertIn("version 3.2", version.stdout, "Keep this regression on the runner's system Bash")
        result, output, calls = self.run_build()
        self.assert_manifest(result, output, "whisper-cli")
        configure = next(call["args"] for call in calls if call["tool"] == "cmake" and call["args"][0] == "-S")
        self.assertIn("-DCMAKE_BUILD_TYPE=Release", configure)
        self.assertNotIn("-A", configure)
        self.assertIn("-DGGML_AVX2=OFF", configure)
        self.assertIn("-DGGML_NATIVE=OFF", configure)
        self.assertIn("-DGGML_METAL_EMBED_LIBRARY=ON", configure)
        self.assertFalse(any(argument.startswith("-DCMAKE_MSVC_RUNTIME_LIBRARY=") for argument in configure))
        self.assertFalse(any(argument.startswith("-DGGML_OPENMP=") for argument in configure))
        manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual((manifest["compiler_policy"], manifest["runtime_policy"]),
                         ("system-toolchain", "platform-runtime"))
        self.assertEqual([call["args"][:2] for call in calls if call["tool"] == "codesign"],
                         [["--force", "--sign"], ["--verify", "--strict"]])
        self.assertTrue((output / "runtime/whisper-cli").read_bytes().endswith(b"fixture signature\n"))

    def test_linux_keeps_portable_cpu_policy_without_platform_signing(self):
        result, output, calls = self.run_build("Linux", "x86_64-unknown-linux-gnu")
        self.assert_manifest(result, output, "whisper-cli")
        self.assertFalse(any(call["tool"] == "codesign" for call in calls))

    def test_windows_native_architectures_use_actual_release_executable(self):
        for target, architecture in [("aarch64-pc-windows-msvc", "ARM64"), ("x86_64-pc-windows-msvc", "x64")]:
            with self.subTest(target=target):
                result, output, calls = self.run_build("MINGW64_NT-10.0", target)
                self.assert_manifest(result, output, "whisper-cli.exe")
                configure = next(call["args"] for call in calls if call["tool"] == "cmake" and call["args"][0] == "-S")
                self.assertEqual(configure[configure.index("-A") + 1], architecture)
                for argument in ("-DCMAKE_BUILD_TYPE=Release", "-DCMAKE_POLICY_DEFAULT_CMP0091=NEW",
                                 "-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded", "-DGGML_OPENMP=OFF",
                                 "-DBUILD_SHARED_LIBS=OFF", "-DGGML_NATIVE=OFF", "-DGGML_AVX2=OFF"):
                    prefix = argument.split("=", 1)[0] + "="
                    self.assertEqual([value for value in configure if value.startswith(prefix)], [argument])
                manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
                self.assertEqual(manifest["runtime_policy"], "static-msvc-no-openmp")
                if architecture == "ARM64":
                    self.assertEqual(configure[configure.index("-T") + 1], "ClangCL")
                    self.assertIn("-DGGML_CPU_ARM_ARCH=armv8-a", configure)
                    self.assertEqual(manifest["compiler_policy"], "clang-cl-arm64")
                else:
                    self.assertNotIn("-T", configure)
                    self.assertFalse(any(argument.startswith("-DGGML_CPU_ARM_ARCH=") for argument in configure))
                    self.assertEqual(manifest["compiler_policy"], "msvc-x64")
                build = next(call["args"] for call in calls if call["tool"] == "cmake" and call["args"][0] == "--build")
                self.assertEqual(build[build.index("--config") + 1], "Release")
                self.assertFalse(any(call["tool"] == "codesign" for call in calls))

    def test_unknown_windows_architecture_never_configures_or_publishes(self):
        result, output, calls = self.run_build("MINGW64_NT-10.0", "i686-pc-windows-msvc")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("native Windows ARM64/x64 target", result.stderr)
        self.assertFalse(any(call["tool"] == "cmake" and call["args"][0] == "-S" for call in calls))
        self.assertFalse((output / "manifest.json").exists())

    def test_integrity_configure_build_and_signing_failures_are_not_successful(self):
        for failure, status in [("source_sha", 1), ("configure", 7), ("build", 8), ("codesign", 9)]:
            with self.subTest(failure=failure):
                result, output, calls = self.run_build(failure=failure)
                self.assertEqual(result.returncode, status, result.stdout + result.stderr)
                self.assertNotIn("Verified native component prepared", result.stdout)
                self.assertFalse((output / "manifest.json").exists())
                if failure == "source_sha":
                    self.assertFalse(any(call["tool"] == "tar" for call in calls))

    def test_premature_zero_exit_cannot_report_success_or_publish_manifest(self):
        result, output, _ = self.run_build(interrupt=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("stopped before completion", result.stderr)
        self.assertNotIn("Verified native component prepared", result.stdout)
        self.assertFalse((output / "manifest.json").exists())


if __name__ == "__main__":
    unittest.main()
