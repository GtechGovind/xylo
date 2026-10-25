"""Deterministic native-notice fixtures; no Linux command execution on macOS."""
import copy
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import subprocess
import tempfile
import tarfile
import unittest
from unittest.mock import patch

from release_common import BINARIES, digest, write_json
from release_linux_notices import (IDENTITY_NOTE, collect_linux_notices, dependency_paths, elf_identity,
                                   extract_debian_notices, inspect_infrastructure, notice_inputs, package_owner, pinned_runtime,
                                   validate_infrastructure_inventory, validate_linux_appimage,
                                   validate_linux_inventory, validate_linux_report)

TARGET = "x86_64-unknown-linux-gnu"
LIB_ID = {"machine": 62, "build_id": "1" * 40, "soname": "libfixture.so.1"}
APP_RUN = b"\x7fELFpackager launcher"
RUNTIME = b"\x7fELF" + bytes(60)


def infrastructure_fixture(metadata: Path) -> list[dict]:
    folder = metadata / "linux-infrastructure"
    (folder / "texts").mkdir(parents=True)
    text = folder / "texts/fixture.txt"
    text.write_bytes(b"Exact upstream infrastructure notice fixture\n")
    entries = []
    for architecture in ("x86_64", "aarch64"):
        for kind, data in (("apprun", APP_RUN), ("appimage-runtime", RUNTIME)):
            entry = {"id": kind, "architecture": architecture, "upstream": "https://github.com/example/upstream",
                     "source_revision": "f" * 40, "size_bytes": len(data), "sha256": hashlib.sha256(data).hexdigest(),
                     "texts": [{"path": "texts/fixture.txt", "sha256": digest(text), "size_bytes": text.stat().st_size}]}
            if kind == "appimage-runtime":
                entry.update(normalized_sha256=hashlib.sha256(data).hexdigest(),
                             download_url="https://github.com/example/upstream/releases/download/exact/runtime",
                             normalization={"section": ".digest_md5", "offset": 20, "size_bytes": 16})
            entries.append(entry)
    write_json(folder / "catalogue.json", {"schema": 1, "components": entries})
    return entries


def inventory_fixture(metadata: Path, target: str, binaries: list[dict]) -> dict:
    """Explicit synthetic report inputs, never a claim of native tool execution."""
    machine = 183 if target.startswith("aarch64") else 62
    text = metadata / "linux-native/texts" / ("0" * 64 + ".txt")
    text.parent.mkdir(parents=True)
    text.write_bytes(b"Copyright fixture; retained distro notice, not a real license grant.\n")
    renamed = text.with_name(digest(text) + ".txt")
    text.rename(renamed)
    identity = {**LIB_ID, "machine": machine}
    inventory = {"schema": 1, "target": target, "identity_link": IDENTITY_NOTE,
                 "inputs": [{**item, "identity": {"machine": machine, "build_id": str(index + 2) * 40, "soname": None}}
                            for index, item in enumerate(binaries)],
                 "libraries": [{"identity": identity, "package": "libfixture1:amd64", "sha256": "f" * 64, "size_bytes": 20}],
                 "packages": [{"package": "libfixture1:amd64", "version": "1.2-3", "texts": [
                     {"kind": "installed_package_copyright", "path": renamed.relative_to(metadata).as_posix(),
                      "sha256": digest(renamed), "size_bytes": renamed.stat().st_size}]}], "limitations": ["Offline fixture"]}
    write_json(metadata / "linux-native/inventory.json", inventory)
    infrastructure = infrastructure_fixture(metadata)
    architecture = "aarch64" if target.startswith("aarch64") else "x86_64"
    image = metadata.parent / "fixture.AppImage"
    if not image.exists():
        image.write_bytes(b"Offline fixture AppImage")
    debian = metadata.parent / "fixture.deb"
    if not debian.exists():
        debian.write_bytes(b"Offline fixture Debian package")
    report = {"schema": 1, "target": target, "passed": True, "scope": "bundled distro ELF shared libraries",
              "inventory_sha256": digest(metadata / "linux-native/inventory.json"), "identity_link": IDENTITY_NOTE,
              "binaries": binaries, "libraries": [{"path": "usr/lib/libfixture.so.1", "sha256": "a" * 64,
                  "size_bytes": 21, "identity": identity, "package": "libfixture1:amd64", "source_sha256": "f" * 64}],
              "infrastructure_catalogue_sha256": digest(metadata / "linux-infrastructure/catalogue.json"),
              "packaging_infrastructure": [{"id": item["id"], "architecture": architecture, "size_bytes": item["size_bytes"],
                   "sha256": item["sha256"], "source_sha256": item["sha256"],
                   **({"normalized_sha256": item["normalized_sha256"]} if item["id"] == "appimage-runtime" else {})}
                  for item in infrastructure if item["architecture"] == architecture],
              "appimage": {"name": image.name, "sha256": digest(image), "size_bytes": image.stat().st_size},
              "debian": {"name": debian.name, "sha256": digest(debian), "size_bytes": debian.stat().st_size,
                  "inventory_sha256": digest(metadata / "linux-native/inventory.json"),
                  "infrastructure_catalogue_sha256": digest(metadata / "linux-infrastructure/catalogue.json")}}
    write_json(metadata / "linux-native-validation.json", report)
    return report


class LinuxNoticeTests(unittest.TestCase):
    def test_committed_infrastructure_catalogue_has_all_exact_bytes_and_provenance_documents(self):
        from release_linux_notices import collect_infrastructure, INFRASTRUCTURE_ROOT
        with tempfile.TemporaryDirectory() as directory:
            metadata = Path(directory)
            collect_infrastructure(metadata)
            entries = validate_infrastructure_inventory(metadata)
            self.assertEqual(len(entries), 4)
            self.assertEqual(digest(metadata / "linux-infrastructure/catalogue.json"), digest(INFRASTRUCTURE_ROOT / "catalogue.json"))
            catalogue = json.loads((metadata / "linux-infrastructure/catalogue.json").read_text())
            self.assertTrue(catalogue.get("documents"))
            for document in catalogue["documents"]:
                self.assertEqual(digest(metadata / "linux-infrastructure" / document["path"]), document["sha256"])

    def test_ldd_and_readelf_parse_actual_shapes_and_fail_unknown_records(self):
        paths = dependency_paths("\tlinux-vdso.so.1 (0x00007ffc)\n\tlibgomp.so.1 => /lib/libgomp.so.1 (0x00007fa0)\n\t/lib64/ld-linux-x86-64.so.2 (0x00007ff0)\n")
        self.assertEqual(paths, [Path("/lib/libgomp.so.1"), Path("/lib64/ld-linux-x86-64.so.2")])
        for output in ("libmissing.so.1 => not found\n", "not a dynamic executable", ""):
            with self.subTest(output=output), self.assertRaises(ValueError):
                dependency_paths(output)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "library"
            path.write_bytes(b"\x7fELF\x02\x01" + bytes(12) + (62).to_bytes(2, "little"))
            with patch("release_linux_notices.command", return_value=" 0x000000000000000e (SONAME) Library soname: [libfixture.so.1]\n    Build ID: " + "1" * 40 + "\n"):
                self.assertEqual(elf_identity(path), LIB_ID)
            # GNU binutils 2.42 print_note(do_wide) prints a TAB rather than a
            # newline before print_gnu_note appends its Build ID field.
            wide = " 0x000000000000000e (SONAME) Library soname: [libfixture.so.1]\n  GNU 0x00000014\tNT_GNU_BUILD_ID (unique build ID bitstring)\t    Build ID: " + "1" * 40 + "\n"
            with patch("release_linux_notices.command", return_value=wide):
                self.assertEqual(elf_identity(path), LIB_ID)
            for invalid in (wide + "Build ID: " + "2" * 40 + "\n", wide.replace("1" * 40, "1" * 129), wide.replace("1" * 40, "1" * 40 + "not-hex")):
                with patch("release_linux_notices.command", return_value=invalid):
                    with self.assertRaisesRegex(ValueError, "readelf output"):
                        elf_identity(path)
            with patch("release_linux_notices.command", return_value="No build ID"):
                with self.assertRaisesRegex(ValueError, "missing/ambiguous"):
                    elf_identity(path)

    def test_merged_usr_owner_is_exact_and_unknown_or_ambiguous_owner_fails(self):
        calls = []
        def search(args, **_options):
            calls.append(args[-1])
            return subprocess.CompletedProcess(args, 0 if args[-1] == "/lib/libfixture.so.1" else 1,
                ("diversion by libfixture1 from: /lib/libfixture.so.1\n"
                 "diversion by libfixture1 to: /lib/libfixture.so.1.usr-is-merged\n"
                 "local diversion from: /lib/libfixture.so.1\n"
                 "local diversion to: /lib/libfixture.so.1.local\n"
                 "libfixture1:amd64: /lib/libfixture.so.1\n") if args[-1] == "/lib/libfixture.so.1" else "", "")
        # The resolved name represents Linux dpkg state, even when these pure
        # parser fixtures execute on a native Windows release runner.
        with patch.object(Path, "resolve", return_value=PurePosixPath("/usr/lib/libfixture.so.1")), \
                patch("release_linux_notices.run_bounded", side_effect=search), \
                patch("release_linux_notices.command", return_value="libfixture1:amd64\t1.2-3\tfixture\t1.2-3\n"):
            self.assertEqual(package_owner(Path("/usr/lib/libfixture.so.1"))["version"], "1.2-3")
        self.assertIn("/lib/libfixture.so.1", calls)
        for output in ("", "one: /lib/libfixture.so.1\ntwo: /lib/libfixture.so.1\n",
                       "diversion by libc6 from: /lib/libfixture.so.1\ndiversion by libc6 to: /lib/libfixture.so.1.usr-is-merged\n"):
            with patch.object(Path, "resolve", return_value=PurePosixPath("/lib/libfixture.so.1")), \
                    patch("release_linux_notices.run_bounded", return_value=subprocess.CompletedProcess([], 0, output, "")):
                with self.assertRaisesRegex(ValueError, "unknown or ambiguous"):
                    package_owner(Path("/lib/libfixture.so.1"))
        for code in (0, 1):
            with patch.object(Path, "resolve", return_value=PurePosixPath("/lib/libfixture.so.1")), \
                    patch("release_linux_notices.run_bounded", return_value=subprocess.CompletedProcess([], code,
                          "arbitrary ownership annotation: /lib/libfixture.so.1\n", "")):
                with self.assertRaisesRegex(ValueError, "[Uu]nexpected|Unrecognized"):
                    package_owner(Path("/lib/libfixture.so.1"))

    def test_installed_copyright_symlink_and_referenced_common_license_preserve_exact_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            doc = root / "usr/share/doc"
            (doc / "libfixture1").mkdir(parents=True)
            (doc / "fixture-base").mkdir()
            (root / "usr/share/common-licenses").mkdir()
            copyright_text = b"Copyright exact fixture\nSee /usr/share/common-licenses/GPL-3.\n"
            (doc / "fixture-base/copyright").write_bytes(copyright_text)
            if os.name == "nt":
                (doc / "libfixture1/copyright").write_bytes(copyright_text)
            else:
                (doc / "libfixture1/copyright").symlink_to("../fixture-base/copyright")
            (root / "usr/share/common-licenses/GPL-3").write_bytes(b"Exact distro canonical text\x00\n")
            records = notice_inputs({"package": "libfixture1:amd64"}, system=root)
            self.assertEqual([item[2] for item in records], [copyright_text, b"Exact distro canonical text\x00\n"])
            if os.name != "nt":
                (doc / "libfixture1/copyright").unlink()
                (root / "outside").write_bytes(b"Untrusted")
                (doc / "libfixture1/copyright").symlink_to(root / "outside")
                with self.assertRaisesRegex(ValueError, "escapes"):
                    notice_inputs({"package": "libfixture1:amd64"}, system=root)

    def test_packaged_rpath_changes_record_new_hash_and_unknown_or_ambiguous_library_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            metadata, appdir = root / "metadata", root / "app"
            binaries, native = root / "bin", root / "native"
            native.joinpath("whisper-runtime/runtime").mkdir(parents=True)
            binaries.mkdir()
            sources = [(name, binaries / name) for name in BINARIES]
            sources.append(("whisper-cli", native / "whisper-runtime/runtime/whisper-cli"))
            identities = {}
            for index, (name, source) in enumerate(sources):
                source.write_bytes(b"\x7fELF" + name.encode())
                identities[name] = {"machine": 62, "build_id": str(index + 2) * 40, "soname": None}
                destination = appdir / ("usr/bin/" + name if name in BINARIES else "usr/bin/native-assets/whisper-runtime/runtime/whisper-cli")
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(source.read_bytes())
            library = root / "libfixture.so.1"
            library.write_bytes(b"\x7fELFsource RPATH")
            copyright_file = root / "copyright"
            copyright_file.write_bytes(b"Exact copyright fixture")
            def identity(path):
                return identities.get(path.name, LIB_ID)
            def native_dependencies(output):
                # The tool reports Linux names even when the local byte
                # fixture is held in a Windows temporary directory. Exercise
                # the strict parser, then explicitly map its synthetic name.
                self.assertEqual(dependency_paths(output), [Path("/usr/lib/libfixture.so.1")])
                return [library]
            owner = {"package": "libfixture1:amd64", "version": "1.2-3", "source_package": "fixture", "source_version": "1.2-3"}
            with patch("release_linux_notices.elf_identity", side_effect=identity), \
                    patch("release_linux_notices.command", return_value="libfixture.so.1 => /usr/lib/libfixture.so.1 (0x00000001)\n"), \
                    patch("release_linux_notices.dependency_paths", side_effect=native_dependencies), \
                    patch("release_linux_notices.package_owner", return_value=owner), \
                    patch("release_linux_notices.collect_infrastructure", side_effect=infrastructure_fixture), \
                    patch("release_linux_notices.notice_inputs", return_value=[("installed_package_copyright", copyright_file, copyright_file.read_bytes())]):
                collect_linux_notices(binaries, native, metadata, TARGET)
            packaged = appdir / "usr/lib/libfixture.so.1"
            packaged.parent.mkdir(parents=True)
            packaged.write_bytes(b"\x7fELFchanged RPATH")
            (appdir / "AppRun").write_bytes(APP_RUN)
            image = root / "fixture.AppImage"
            image.write_bytes(RUNTIME + b"SquashFS fixture")
            records = [{"name": name, "sha256": digest(source), "size_bytes": source.stat().st_size, "architecture": "x86_64"} for name, source in sources]
            with patch("release_linux_notices.elf_identity", side_effect=identity), \
                    patch("release_linux_notices.command", return_value=" [29] .digest_md5 PROGBITS 00000000 000014 000010 00 A 0 0 1\n"):
                report = validate_linux_appimage(metadata, appdir, TARGET, records, image)
                self.assertNotEqual(report["libraries"][0]["sha256"], report["libraries"][0]["source_sha256"])
                self.assertIn("not byte equivalence", report["identity_link"])
                with patch("release_linux_notices.elf_identity", side_effect=lambda path: {**LIB_ID, "build_id": "9" * 40} if path == packaged else identity(path)):
                    with self.assertRaisesRegex(ValueError, "unknown or ambiguous"):
                        validate_linux_appimage(metadata, appdir, TARGET, records, image)
                inventory = json.loads((metadata / "linux-native/inventory.json").read_text())
                inventory["libraries"].append({**inventory["libraries"][0], "sha256": "a" * 64})
                write_json(metadata / "linux-native/inventory.json", inventory)
                with self.assertRaisesRegex(ValueError, "unknown or ambiguous"):
                    validate_linux_appimage(metadata, appdir, TARGET, records, image)

    def test_runtime_provision_is_hash_checked_bounded_and_reuses_only_exact_cache(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            metadata, cache = root / "metadata", root / "cache"
            infrastructure_fixture(metadata)
            def opener(data):
                response = io.BytesIO(data)
                response.status = 200
                return response
            for data, success in ((b"wrong", False), (RUNTIME + b"extra", False), (RUNTIME, True)):
                with patch("release_linux_notices.urllib.request.build_opener") as builder:
                    builder.return_value.open.return_value = opener(data)
                    if success:
                        path = pinned_runtime(metadata, TARGET, cache)
                        self.assertEqual(path.read_bytes(), RUNTIME)
                    else:
                        with self.assertRaises(ValueError):
                            pinned_runtime(metadata, TARGET, cache)
                    self.assertFalse(list(cache.glob(".runtime-*")))
            with patch("release_linux_notices.urllib.request.build_opener", side_effect=AssertionError("verified cache must avoid network")):
                self.assertEqual(pinned_runtime(metadata, TARGET, cache), path)
            path.write_bytes(b"changed")
            with self.assertRaisesRegex(ValueError, "Cached"):
                pinned_runtime(metadata, TARGET, cache)

    def test_runtime_allows_only_exact_pinned_digest_section_and_apprun_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            infrastructure_fixture(root)
            app = root / "app"
            app.mkdir()
            (app / "AppRun").write_bytes(APP_RUN)
            image = root / "fixture.AppImage"
            rewritten = RUNTIME[:20] + b"DigestIs16bytes!" + RUNTIME[36:]
            self.assertEqual(len(b"DigestIs16bytes!"), 16)
            image.write_bytes(rewritten + b"SquashFS fixture")
            sections = " [29] .digest_md5 PROGBITS 00000000 000014 000010 00 A 0 0 1\n"
            with patch("release_linux_notices.command", return_value=sections):
                records = inspect_infrastructure(root, app, image, TARGET)
                self.assertEqual(len(records), 2)
                self.assertNotEqual(records[1]["sha256"], records[1]["source_sha256"])
                image.write_bytes(b"changed" + rewritten[7:])
                with self.assertRaisesRegex(ValueError, "outside"):
                    inspect_infrastructure(root, app, image, TARGET)
            image.write_bytes(rewritten)
            with patch("release_linux_notices.command", return_value=sections.replace("000014", "000015")):
                with self.assertRaisesRegex(ValueError, "layout"):
                    inspect_infrastructure(root, app, image, TARGET)
            (app / "AppRun").write_bytes(b"changed")
            with self.assertRaisesRegex(ValueError, "size"):
                inspect_infrastructure(root, app, image, TARGET)
            text = root / "linux-infrastructure/texts/fixture.txt"
            text.write_bytes(b"Tampered")
            with self.assertRaisesRegex(ValueError, "hash/size"):
                validate_infrastructure_inventory(root)

    def test_inventory_and_report_tamper_are_rejected(self):
        for mutation in ("text", "owner", "source_hash", "report_target", "report_hash", "binary"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as directory:
                metadata = Path(directory)
                binaries = [{"name": name, "sha256": "b" * 64, "size_bytes": 1} for name in (*BINARIES, "whisper-cli")]
                report = inventory_fixture(metadata, TARGET, binaries)
                validate_linux_report(report, metadata, TARGET, binaries)
                if mutation == "text":
                    next((metadata / "linux-native/texts").iterdir()).write_bytes(b"Tampered")
                elif mutation == "owner":
                    report["libraries"][0]["package"] = "unknown"
                elif mutation == "source_hash":
                    report["libraries"][0]["source_sha256"] = "c" * 64
                elif mutation == "report_target":
                    report["target"] = "aarch64-unknown-linux-gnu"
                elif mutation == "report_hash":
                    report["inventory_sha256"] = "c" * 64
                else:
                    report["binaries"] = copy.deepcopy(binaries)
                    report["binaries"][0]["sha256"] = "c" * 64
                with self.assertRaises(ValueError):
                    validate_linux_report(report, metadata, TARGET, binaries)

    def test_debian_stream_retains_only_bounded_regular_notice_files_and_rejects_escape_or_alias(self):
        def fixture(mutation):
            data = io.BytesIO()
            with tarfile.open(fileobj=data, mode="w") as archive:
                for name, payload in (("./usr/bin/unrelated", b"Executable fixture"),
                        ("./usr/lib/Xylo/licenses/dependencies/linux-native/inventory.json", b'{"fixture":true}'),
                        ("./usr/lib/Xylo/licenses/dependencies/linux-infrastructure/catalogue.json", b'{"fixture":true}')):
                    member = tarfile.TarInfo(name)
                    member.size = len(payload)
                    if mutation == "escape" and "inventory" in name:
                        member.name = "../outside"
                    if mutation == "alias" and "inventory" in name:
                        member.type, member.linkname = tarfile.SYMTYPE, "/outside"
                        member.size = 0
                    archive.addfile(member, io.BytesIO(payload))
            data.seek(0)
            return data
        for mutation in (None, "escape", "alias"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as directory:
                destination = Path(directory)
                with tarfile.open(fileobj=fixture(mutation), mode="r|") as archive:
                    if mutation:
                        with self.assertRaises(ValueError):
                            extract_debian_notices(archive, destination)
                    else:
                        self.assertGreater(extract_debian_notices(archive, destination), 0)
                        self.assertEqual(sorted(path.name for path in destination.rglob("*.json")), ["catalogue.json", "inventory.json"])
                        self.assertFalse((destination / "usr/bin/unrelated").exists())


if __name__ == "__main__":
    unittest.main()
