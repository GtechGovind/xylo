"""Preserve actual Debian-owned ELF dependency notices in Linux packages.

The source hashes bind the runner's installed files. GNU build ID, machine and
SONAME link a packaged library back to that source after linuxdeploy changes its
RPATH; this is an identity/provenance check, not a byte-equivalence assertion.
No SPDX expression, license grant, or legal approval is inferred from filenames.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
import threading
import time
import urllib.parse
import urllib.request

from release_common import BINARIES, ROOT, digest, write_json
from release_windows import run_llvm as run_bounded

TARGETS = {"x86_64-unknown-linux-gnu": 62, "aarch64-unknown-linux-gnu": 183}
MAX_LIBRARIES = 256
MAX_TEXT = 8 * 1024**2
MAX_TOTAL_TEXT = 32 * 1024**2
MAX_JSON = 2 * 1024**2
IDENTITY_NOTE = "GNU build ID + ELF machine + SONAME; not byte equivalence after RPATH rewriting"
INFRASTRUCTURE_ROOT = ROOT / "packaging/licenses/linux-infrastructure"


def command(arguments: list[str], *, limit: int = MAX_JSON) -> str:
    # Native tools' output is parsed below; translations must not alter fields.
    # run_bounded supplies bounded native pipe readers and process cleanup. Set
    # locale on the child via env rather than changing this process globally.
    result = run_bounded(["env", "LC_ALL=C", *arguments], timeout=60, limit=limit)
    if result.returncode != 0:
        raise ValueError("Linux dependency notice inspection failed: " + arguments[0] + ": " + result.stderr[:1024])
    return result.stdout


def regular_bytes(path: Path, limit: int) -> bytes:
    with path.open("rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or not 0 < info.st_size <= limit:
            raise ValueError("Linux notice input must be a bounded regular file")
        value = stream.read(limit + 1)
    if len(value) > limit or len(value) != info.st_size:
        raise ValueError("Linux notice input changed or exceeded its size budget")
    return value


def read_json(path: Path) -> dict:
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError("Duplicate Linux notice JSON key")
            result[key] = value
        return result
    if path.is_symlink():
        raise ValueError("Linux notice metadata must not be a symbolic link")
    return json.loads(regular_bytes(path, MAX_JSON), object_pairs_hook=pairs)


def elf_identity(path: Path) -> dict:
    with path.open("rb") as stream:
        header = stream.read(20)
    if len(header) != 20 or header[:6] != b"\x7fELF\x02\x01":
        raise ValueError("Linux notice inspection requires a 64-bit little-endian ELF")
    machine = int.from_bytes(header[18:20], "little")
    text = command(["readelf", "--wide", "--dynamic", "--notes", str(path)])
    # GNU binutils 2.42 print_note() emits the note owner/type followed by a
    # TAB in --wide mode; print_gnu_note() then appends Build ID on that row.
    # LLVM and non-wide GNU output put the same field on its own line.
    builds = re.findall(r"(?<![A-Za-z0-9_])Build ID:[ \t]+([0-9a-fA-F]{8,128})(?=\s|$)", text)
    sonames = re.findall(r"\(SONAME\)\s+Library soname: \[([^\]\n]{1,255})\]", text)
    if len(builds) != 1 or len(sonames) > 1:
        raise ValueError("ELF dependency has a missing/ambiguous GNU build ID or SONAME: "
                         + str(path)[:512] + f" (build_ids={len(builds)}, sonames={len(sonames)}). readelf output: " + text[:4096])
    return {"machine": machine, "build_id": builds[0].lower(), "soname": sonames[0] if sonames else None}


def identity_key(value: dict) -> tuple:
    if (value.get("machine") not in TARGETS.values()
            or not re.fullmatch(r"[0-9a-f]{8,128}", value.get("build_id", ""))
            or value.get("soname") is not None and not re.fullmatch(r"[A-Za-z0-9_.+-]{1,255}", value["soname"])):
        raise ValueError("Invalid native ELF identity")
    return value["machine"], value["build_id"], value.get("soname")


def dependency_paths(text: str) -> list[Path]:
    paths = set()
    for line in text.splitlines():
        line = line.strip()
        if not line or re.fullmatch(r"linux-vdso\.so\.\d+ \(0x[0-9a-fA-F]+\)", line):
            continue
        if "not found" in line:
            raise ValueError("An ELF dependency is not installed on the packaging host")
        match = re.fullmatch(r"(?:[^\s]+ => )?(/[^\r\n]+?) \(0x[0-9a-fA-F]+\)", line)
        if match is None:
            raise ValueError("Unrecognized ldd dependency record")
        paths.add(Path(match[1]))
    if not paths or len(paths) > MAX_LIBRARIES:
        raise ValueError("ELF dependency closure is empty or exceeds its budget")
    return sorted(paths)


def package_owner(path: Path) -> dict:
    resolved = path.resolve(strict=True)
    # dpkg may retain /lib while the installed file resolves through merged-/usr.
    candidates = {path.as_posix(), resolved.as_posix()}
    for candidate in list(candidates):
        if candidate.startswith("/usr/lib/") or candidate.startswith("/usr/lib64/"):
            candidates.add(candidate.removeprefix("/usr"))
        elif candidate.startswith("/lib/") or candidate.startswith("/lib64/"):
            candidates.add("/usr" + candidate)
    owners, evidence = set(), []
    for candidate in sorted(candidates):
        result = run_bounded(["env", "LC_ALL=C", "dpkg-query", "--search", candidate], timeout=30, limit=64 * 1024)
        evidence.append({"path": candidate, "exit": result.returncode, "stdout": result.stdout[:2048], "stderr": result.stderr[:512]})
        if result.returncode == 1:
            if result.stdout.strip():
                raise ValueError("Unexpected dpkg owner output for an unmatched path: " + result.stdout[:2048])
            continue
        if result.returncode:
            raise ValueError("dpkg could not identify a native dependency owner")
        for line in result.stdout.splitlines():
            # dpkg-query --search also emits "diversion by <pkg> from/to:"
            # and "local diversion from/to:". Those are metadata, not owners.
            # Mirror dpkg-shlibdeps' distinction; never infer authority from
            # the package which registered a diversion.
            if re.fullmatch(r"(?:diversion by [a-z0-9][a-z0-9+.-]* (?:from|to)|local diversion (?:from|to)): /.*", line):
                continue
            match = re.fullmatch(r"([a-z0-9][a-z0-9+.-]*(?::[a-z0-9-]+)?(?:, [a-z0-9][a-z0-9+.-]*(?::[a-z0-9-]+)?)*): (/.*)", line)
            if match is None:
                raise ValueError("Unrecognized dpkg owner output: " + line[:2048])
            if match[2] == candidate:
                owners.update(match[1].split(", "))
    if len(owners) != 1:
        raise ValueError("Native dependency has an unknown or ambiguous installed package owner: " + str(path)[:512]
                         + "; owners=" + repr(sorted(owners))[:1024] + "; queries=" + json.dumps(evidence)[:8192])
    owner = owners.pop()
    if not re.fullmatch(r"[a-z0-9][a-z0-9+.-]*(?::[a-z0-9-]+)?", owner):
        raise ValueError("Invalid native dependency package name")
    record = command(["dpkg-query", "--show", "--showformat=${binary:Package}\t${Version}\t${source:Package}\t${source:Version}\n", owner])
    fields = record.rstrip("\n").split("\t")
    if len(fields) != 4 or fields[0] != owner or any(not field or len(field) > 255 or "\n" in field for field in fields):
        raise ValueError("Installed package provenance is incomplete")
    return dict(zip(("package", "version", "source_package", "source_version"), fields))


def notice_inputs(owner: dict, *, system: Path = Path("/")) -> list[tuple[str, Path, bytes]]:
    package = owner["package"].split(":", 1)[0]
    copyright_path = system / "usr/share/doc" / package / "copyright"
    resolved = copyright_path.resolve(strict=True)
    if not resolved.is_relative_to((system / "usr/share/doc").resolve(strict=True)):
        raise ValueError("Installed copyright file escapes the distribution documentation root")
    content = regular_bytes(resolved, MAX_TEXT)
    result = [("installed_package_copyright", copyright_path, content)]
    references = set(re.findall(rb"/usr/share/common-licenses/([A-Za-z0-9][A-Za-z0-9.+_-]{0,100})", content))
    for raw in sorted(references):
        name = raw.decode("ascii")
        candidate = system / "usr/share/common-licenses" / name
        # Sentence punctuation is common immediately after a documentation path.
        while not candidate.exists() and name.endswith((".", "-")):
            name = name[:-1]
            candidate = candidate.with_name(name)
        actual = candidate.resolve(strict=True)
        if not actual.is_relative_to((system / "usr/share/common-licenses").resolve(strict=True)):
            raise ValueError("A referenced distribution license escapes its trusted directory")
        result.append(("referenced_distribution_license", candidate, regular_bytes(actual, MAX_TEXT)))
    return result


def collect_linux_notices(binaries: Path, native: Path, metadata: Path, target: str) -> dict:
    if target not in TARGETS:
        raise ValueError("Native Linux notices require a supported GNU/Linux target")
    paths = [(name, binaries / name) for name in BINARIES]
    paths.append(("whisper-cli", native / "whisper-runtime/runtime/whisper-cli"))
    inputs, dependencies = [], set()
    for name, path in paths:
        if path.is_symlink() or not path.is_file():
            raise ValueError("Linux release executable must be a regular file")
        identity = elf_identity(path)
        if identity["machine"] != TARGETS[target]:
            raise ValueError("Linux release executable architecture differs from its target")
        inputs.append({"name": name, "sha256": digest(path), "size_bytes": path.stat().st_size, "identity": identity})
        dependencies.update(dependency_paths(command(["ldd", str(path.resolve())])))
    if len(dependencies) > MAX_LIBRARIES:
        raise ValueError("Native dependency closure exceeds its library budget")
    libraries, packages, texts, total = [], {}, {}, 0
    for source in sorted(dependencies):
        actual = source.resolve(strict=True)
        identity = elf_identity(actual)
        if identity["machine"] != TARGETS[target] or not identity["soname"]:
            raise ValueError("Native dependency architecture or SONAME is unsupported")
        owner = package_owner(source)
        key = owner["package"]
        if key not in packages:
            records = []
            for kind, original, data in notice_inputs(owner):
                checksum = hashlib.sha256(data).hexdigest()
                relative = "linux-native/texts/" + checksum + ".txt"
                if relative not in texts:
                    total += len(data)
                    if total > MAX_TOTAL_TEXT:
                        raise ValueError("Native dependency notice texts exceed their total budget")
                    texts[relative] = data
                records.append({"kind": kind, "installed_path": str(original), "resolved_path": str(original.resolve(strict=True)),
                                "path": relative, "sha256": checksum, "size_bytes": len(data)})
            packages[key] = {**owner, "texts": records}
        libraries.append({"installed_path": str(source), "resolved_path": str(actual), "sha256": digest(actual),
                          "size_bytes": actual.stat().st_size, "identity": identity, "package": key})
    inventory = {"schema": 1, "target": target, "scope": "installed distro dependency closure of the four native executables",
                 "identity_link": IDENTITY_NOTE, "inputs": inputs, "libraries": libraries,
                 "packages": sorted(packages.values(), key=lambda item: item["package"]),
                 "limitations": ["Installed distribution notices are preserved verbatim; no legal approval or inferred SPDX grant.",
                                 "AppRun and the outer AppImage runtime are packaging infrastructure, not dpkg-owned dependency libraries."]}
    for relative, data in texts.items():
        destination = metadata / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)
    write_json(metadata / "linux-native/inventory.json", inventory, exclusive=True)
    collect_infrastructure(metadata)
    return validate_linux_inventory(metadata, target)


def validate_infrastructure_inventory(metadata: Path) -> list[dict]:
    folder = metadata / "linux-infrastructure"
    catalogue = read_json(folder / "catalogue.json")
    entries = catalogue.get("components", [])
    expected = {(kind, architecture) for kind in ("apprun", "appimage-runtime") for architecture in ("x86_64", "aarch64")}
    if catalogue.get("schema") != 1 or len(entries) != 4 or {(entry.get("id"), entry.get("architecture")) for entry in entries} != expected:
        raise ValueError("Linux infrastructure notice catalogue is incomplete")
    total, seen = 0, set()
    for entry in entries:
        if (not re.fullmatch(r"[0-9a-f]{64}", entry.get("sha256", ""))
                or not isinstance(entry.get("size_bytes"), int) or not 0 < entry["size_bytes"] <= 8 * 1024**2
                or not entry.get("upstream", "").startswith("https://") or not entry.get("source_revision") or not entry.get("texts")):
            raise ValueError("Linux infrastructure provenance is incomplete")
        if entry["id"] == "appimage-runtime":
            normalization = entry.get("normalization", {})
            url = urllib.parse.urlsplit(entry.get("download_url", ""))
            if (normalization.get("section") != ".digest_md5" or normalization.get("size_bytes") != 16
                    or not isinstance(normalization.get("offset"), int) or not 0 <= normalization["offset"] <= entry["size_bytes"] - 16
                    or not re.fullmatch(r"[0-9a-f]{64}", entry.get("normalized_sha256", ""))
                    or url.scheme != "https" or url.hostname not in ("github.com", "api.github.com") or url.username or url.password or url.fragment):
                raise ValueError("Linux AppImage runtime pin/normalization is invalid")
        for text in entry["texts"]:
            relative = text.get("path", "")
            if not re.fullmatch(r"texts/[A-Za-z0-9_.-]{1,96}", relative):
                raise ValueError("Unsafe Linux infrastructure notice text path")
            path = folder / relative
            if path.is_symlink() or path.parent.is_symlink():
                raise ValueError("Linux infrastructure notice text traverses a symbolic link")
            data = regular_bytes(path, MAX_TEXT)
            if hashlib.sha256(data).hexdigest() != text.get("sha256") or len(data) != text.get("size_bytes"):
                raise ValueError("Linux infrastructure notice text hash/size mismatch")
            if relative not in seen:
                total += len(data)
                seen.add(relative)
    if total > MAX_TOTAL_TEXT:
        raise ValueError("Linux infrastructure notice text budget exceeded")
    for document in catalogue.get("documents", []):
        relative = document.get("path", "")
        if not re.fullmatch(r"[A-Za-z0-9_.-]{1,96}", relative):
            raise ValueError("Unsafe Linux infrastructure documentation path")
        path = folder / relative
        if path.is_symlink():
            raise ValueError("Linux infrastructure documentation must be a regular file")
        data = regular_bytes(path, MAX_TEXT)
        if hashlib.sha256(data).hexdigest() != document.get("sha256") or len(data) != document.get("size_bytes"):
            raise ValueError("Linux infrastructure documentation hash/size mismatch")
    return entries


def collect_infrastructure(metadata: Path) -> None:
    # Verify the committed exact-source catalogue before copying it into both
    # installers. No arbitrary directory content is included in the notice set.
    entries = validate_infrastructure_inventory(INFRASTRUCTURE_ROOT.parent)
    destination = metadata / "linux-infrastructure"
    destination.mkdir()
    shutil.copyfile(INFRASTRUCTURE_ROOT / "catalogue.json", destination / "catalogue.json")
    documents = read_json(INFRASTRUCTURE_ROOT / "catalogue.json").get("documents", [])
    for relative in sorted({item["path"] for entry in entries for item in entry["texts"]} | {item["path"] for item in documents}):
        path = destination / relative
        path.parent.mkdir(exist_ok=True)
        shutil.copyfile(INFRASTRUCTURE_ROOT / relative, path)
    validate_infrastructure_inventory(metadata)


def pinned_binary(metadata: Path, target: str, cache: Path, component_id: str) -> Path:
    """Download only an explicit reviewed infrastructure binary."""
    if target not in TARGETS or component_id not in ("apprun", "appimage-runtime"):
        raise ValueError("Expected an explicit supported Linux infrastructure target/component")
    architecture = "aarch64" if target.startswith("aarch64") else "x86_64"
    entry = next(item for item in validate_infrastructure_inventory(metadata)
                 if item["id"] == component_id and item["architecture"] == architecture)
    url = urllib.parse.urlsplit(entry.get("download_url", ""))
    if url.scheme != "https" or url.hostname not in ("github.com", "api.github.com") or url.username or url.password or url.fragment:
        raise ValueError("Linux infrastructure binary download URL is not an official HTTPS pin")
    cache.mkdir(parents=True, exist_ok=True)
    destination = cache / (entry["sha256"] + ".bin")
    if destination.exists() or destination.is_symlink():
        if destination.is_symlink() or destination.stat().st_size != entry["size_bytes"] or digest(destination) != entry["sha256"]:
            raise ValueError("Cached AppImage runtime differs from its reviewed pin")
        return destination.resolve(strict=True)

    class Redirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, request, fp, code, message, headers, url):
            parsed = urllib.parse.urlsplit(url)
            if (parsed.scheme != "https" or parsed.hostname not in ("github.com", "api.github.com", "release-assets.githubusercontent.com", "objects.githubusercontent.com")
                    or parsed.username or parsed.password):
                raise ValueError("AppImage runtime download redirected outside its official HTTPS hosts")
            return super().redirect_request(request, fp, code, message, headers, url)

    request = urllib.request.Request(entry["download_url"], headers={"User-Agent": "Xylo-native-release", "Accept": "application/octet-stream"})
    descriptor, temporary = tempfile.mkstemp(prefix=".runtime-", dir=cache)
    temporary = Path(temporary)
    deadline = time.monotonic() + 120
    try:
        checksum, size = hashlib.sha256(), 0
        with os.fdopen(descriptor, "wb") as output, urllib.request.build_opener(Redirect()).open(request, timeout=30) as response:
            if response.status != 200:
                raise ValueError("Official pinned AppImage runtime download failed")
            while data := response.read(min(64 * 1024, entry["size_bytes"] + 1 - size)):
                size += len(data)
                if size > entry["size_bytes"] or time.monotonic() > deadline:
                    raise ValueError("Pinned AppImage runtime download exceeded its byte/time budget")
                checksum.update(data)
                output.write(data)
            output.flush()
            os.fsync(output.fileno())
        if size != entry["size_bytes"] or checksum.hexdigest() != entry["sha256"]:
            raise ValueError("Downloaded AppImage runtime differs from its reviewed hash/size")
        temporary.replace(destination)
    finally:
        temporary.unlink(missing_ok=True)
    return destination.resolve(strict=True)


def pinned_runtime(metadata: Path, target: str, cache: Path) -> Path:
    return pinned_binary(metadata, target, cache, "appimage-runtime")


def inspect_infrastructure(metadata: Path, appdir: Path, appimage: Path, target: str) -> list[dict]:
    architecture = "aarch64" if target.startswith("aarch64") else "x86_64"
    records = []
    for entry in validate_infrastructure_inventory(metadata):
        if entry["architecture"] != architecture:
            continue
        path = appdir / "AppRun" if entry["id"] == "apprun" else appimage
        if path.is_symlink():
            raise ValueError("Packaged Linux infrastructure must be a regular file")
        with path.open("rb") as stream:
            data = stream.read(entry["size_bytes"] + (1 if entry["id"] == "apprun" else 0))
        if len(data) != entry["size_bytes"]:
            raise ValueError("Packaged Linux infrastructure size differs from its reviewed source")
        actual_hash = hashlib.sha256(data).hexdigest()
        record = {"id": entry["id"], "architecture": architecture, "size_bytes": len(data), "sha256": actual_hash,
                  "source_sha256": entry["sha256"]}
        if entry["id"] == "apprun":
            if actual_hash != entry["sha256"]:
                raise ValueError("Packaged AppRun differs from its reviewed upstream binary")
        else:
            sections = command(["readelf", "--wide", "--sections", str(path)])
            found = re.findall(r"^\s*\[\s*\d+\]\s+\.digest_md5\s+\S+\s+[0-9a-fA-F]+\s+([0-9a-fA-F]+)\s+([0-9a-fA-F]+)\s", sections, re.M)
            normalization = entry["normalization"]
            if len(found) != 1 or tuple(int(value, 16) for value in found[0]) != (normalization["offset"], 16):
                raise ValueError("Packaged AppImage runtime has a different digest section layout")
            offset = normalization["offset"]
            normalized = data[:offset] + bytes(16) + data[offset + 16:]
            record["normalized_sha256"] = hashlib.sha256(normalized).hexdigest()
            if record["normalized_sha256"] != entry["normalized_sha256"]:
                raise ValueError("Packaged AppImage runtime differs outside the reviewed digest section")
        records.append(record)
    return records


def validate_linux_inventory(metadata: Path, target: str) -> dict:
    inventory = read_json(metadata / "linux-native/inventory.json")
    if target not in TARGETS or inventory.get("schema") != 1 or inventory.get("target") != target or inventory.get("identity_link") != IDENTITY_NOTE:
        raise ValueError("Linux native notice inventory has a different target/schema")
    inputs, libraries, packages = inventory.get("inputs", []), inventory.get("libraries", []), inventory.get("packages", [])
    if (len(inputs) != 4 or {item.get("name") for item in inputs} != {*BINARIES, "whisper-cli"}
            or not 0 < len(libraries) <= MAX_LIBRARIES or not 0 < len(packages) <= MAX_LIBRARIES):
        raise ValueError("Linux native notice inventory has incomplete input/library coverage")
    owners, total, unique_texts = set(), 0, set()
    for package in packages:
        key = package.get("package")
        if key in owners or not isinstance(key, str) or not package.get("version") or not package.get("texts"):
            raise ValueError("Linux native notice package provenance is incomplete")
        owners.add(key)
        if not any(item.get("kind") == "installed_package_copyright" for item in package["texts"]):
            raise ValueError("Linux native notice package has no copyright file")
        for item in package["texts"]:
            relative = item.get("path", "")
            if not re.fullmatch(r"linux-native/texts/[0-9a-f]{64}\.txt", relative):
                raise ValueError("Unsafe Linux native notice text path")
            path = metadata / relative
            if any(parent.is_symlink() for parent in [path, *path.parents] if parent.is_relative_to(metadata)):
                raise ValueError("Linux native notice text traverses a symbolic link")
            data = regular_bytes(path, MAX_TEXT)
            if hashlib.sha256(data).hexdigest() != item.get("sha256") or len(data) != item.get("size_bytes"):
                raise ValueError("Linux native notice text hash/size mismatch")
            if relative not in unique_texts:
                unique_texts.add(relative)
                total += len(data)
    if total > MAX_TOTAL_TEXT:
        raise ValueError("Linux native notice text inventory exceeds its budget")
    for item in inputs + libraries:
        if (identity_key(item.get("identity", {}))[0] != TARGETS[target]
                or not re.fullmatch(r"[0-9a-f]{64}", item.get("sha256", ""))
                or not isinstance(item.get("size_bytes"), int) or item["size_bytes"] <= 0):
            raise ValueError("Invalid Linux native source identity")
    for item in libraries:
        if item.get("package") not in owners or not item["identity"]["soname"]:
            raise ValueError("Linux native dependency has no exact notice owner")
    return inventory


def validate_linux_appimage(metadata: Path, appdir: Path, target: str, binaries: list[dict], appimage: Path) -> dict:
    inventory = validate_linux_inventory(metadata, target)
    known_inputs = {identity_key(item["identity"]): item for item in inventory["inputs"]}
    known_libraries = {}
    for item in inventory["libraries"]:
        known_libraries.setdefault(identity_key(item["identity"]), []).append(item)
    recorded, infrastructure, actual_inputs, count = [], [], {}, 0
    for root, directories, files in os.walk(appdir, followlinks=False):
        for name in directories:
            path = Path(root) / name
            if path.is_symlink() and not path.resolve(strict=True).is_relative_to(appdir.resolve(strict=True)):
                raise ValueError("AppImage contains an escaping symbolic directory link")
        directories[:] = sorted(name for name in directories if not (Path(root) / name).is_symlink())
        for name in sorted(files):
            count += 1
            if count > 100_000:
                raise ValueError("AppImage inventory exceeds the file inspection budget")
            path = Path(root) / name
            if path.is_symlink():
                if not path.resolve(strict=True).is_relative_to(appdir.resolve(strict=True)):
                    raise ValueError("AppImage contains an escaping symbolic link")
                continue
            with path.open("rb") as stream:
                if stream.read(4) != b"\x7fELF":
                    continue
            relative = path.relative_to(appdir).as_posix()
            record = {"path": relative, "sha256": digest(path), "size_bytes": path.stat().st_size}
            if relative == "AppRun":
                infrastructure.append({**record, "coverage": "packaging infrastructure; separate upstream notice review"})
                continue
            identity = elf_identity(path)
            key = identity_key(identity)
            if key in known_inputs:
                expected = known_inputs[key]
                if name != expected["name"]:
                    raise ValueError("Packaged ELF executable has an unexpected path/name")
                canonical = "usr/bin/" + name if name in BINARIES else "usr/bin/native-assets/whisper-runtime/runtime/whisper-cli"
                if relative == canonical:
                    actual_inputs[name] = record
                continue
            matches = known_libraries.get(key, [])
            candidates = {(item["package"], item["sha256"]) for item in matches}
            if len(candidates) != 1:
                raise ValueError("Packaged ELF library has an unknown or ambiguous distribution notice owner: " + relative)
            source = matches[0]
            recorded.append({**record, "identity": identity, "package": source["package"], "source_sha256": source["sha256"]})
    if not recorded or len(recorded) > MAX_LIBRARIES or len(infrastructure) != 1:
        raise ValueError("AppImage bundled native library/launcher coverage is incomplete")
    if (set(actual_inputs) != {*BINARIES, "whisper-cli"}
            or len(binaries) != 4 or any(item.get("name") not in actual_inputs
                or any(item.get(field) != actual_inputs[item["name"]][field] for field in ("sha256", "size_bytes")) for item in binaries)):
        raise ValueError("Linux native notice proof does not match all four inspected packaged executables")
    result = {"schema": 1, "target": target, "passed": True, "scope": "bundled distro ELF shared libraries",
              "inventory_sha256": digest(metadata / "linux-native/inventory.json"), "identity_link": IDENTITY_NOTE,
              "binaries": binaries, "libraries": recorded,
              "packaging_infrastructure": inspect_infrastructure(metadata, appdir, appimage, target),
              "infrastructure_catalogue_sha256": digest(metadata / "linux-infrastructure/catalogue.json"),
              "appimage": {"name": appimage.name, "sha256": digest(appimage), "size_bytes": appimage.stat().st_size},
              "limitations": ["Distribution notices and exact-source infrastructure notices are retained; not legal approval.",
                              "GNU build identity links rewritten library bytes to the installed distribution source; it is not byte equivalence."]}
    validate_linux_report(result, metadata, target, binaries, debian_required=False)
    return result


def extract_debian_notices(archive, destination: Path) -> int:
    """Read bounded notice files only, with no tar pathname extraction."""
    prefix, count, total, retained = None, 0, 0, 0
    for member in archive:
        count += 1
        total += member.size
        if count > 100_000 or total > 2 * 1024**3:
            raise ValueError("Debian archive exceeds its file/unpacked-byte inspection budget")
        parts = PurePosixPath(member.name).parts
        if member.name.startswith("/") or ".." in parts:
            raise ValueError("Unsafe Debian archive member")
        markers = [index for index in range(len(parts) - 1) if parts[index:index + 2] == ("licenses", "dependencies")]
        if not markers:
            continue
        if len(markers) != 1:
            raise ValueError("Ambiguous Debian notice namespace")
        index = markers[0]
        relative = parts[index + 2:]
        if not relative or relative[0] not in ("linux-native", "linux-infrastructure") or member.isdir():
            continue
        if prefix is None:
            prefix = parts[:index + 2]
        if parts[:index + 2] != prefix or not member.isfile() or not 0 < member.size <= MAX_TEXT:
            raise ValueError("Debian native notices are ambiguous or not bounded regular files")
        retained += member.size
        if retained > 2 * MAX_TOTAL_TEXT:
            raise ValueError("Debian native notice texts exceed their total budget")
        path = destination.joinpath(*relative)
        path.parent.mkdir(parents=True, exist_ok=True)
        with archive.extractfile(member) as stream:
            data = stream.read(MAX_TEXT + 1)
        if len(data) != member.size:
            raise ValueError("Truncated Debian native notice file")
        with path.open("xb") as output:
            output.write(data)
    if not retained:
        raise ValueError("Debian package contains no native dependency notices")
    return retained


def validate_linux_debian(metadata: Path, package: Path, target: str, scratch: Path) -> dict:
    destination = scratch / "debian-native-notices"
    destination.mkdir()
    with tempfile.TemporaryFile() as errors:
        process = subprocess.Popen(["dpkg-deb", "--fsys-tarfile", str(package)], stdout=subprocess.PIPE, stderr=errors)
        timer = threading.Timer(120, process.kill)
        timer.daemon = True
        timer.start()
        try:
            with process.stdout, tarfile.open(fileobj=process.stdout, mode="r|") as archive:
                extract_debian_notices(archive, destination)
            if process.wait(timeout=5) != 0:
                raise ValueError("Native Debian notice archive inspection failed")
        finally:
            timer.cancel()
            if process.poll() is None:
                process.kill()
            process.wait(timeout=5)
    validate_linux_inventory(destination, target)
    validate_infrastructure_inventory(destination)
    for relative in ("linux-native/inventory.json", "linux-infrastructure/catalogue.json"):
        if digest(destination / relative) != digest(metadata / relative):
            raise ValueError("Packaged Debian native notice inventory differs from retained metadata")
    return {"name": package.name, "sha256": digest(package), "size_bytes": package.stat().st_size,
            "inventory_sha256": digest(destination / "linux-native/inventory.json"),
            "infrastructure_catalogue_sha256": digest(destination / "linux-infrastructure/catalogue.json")}


def validate_linux_report(report: dict, metadata: Path, target: str, binaries: list[dict], *, debian_required: bool = True) -> None:
    inventory = validate_linux_inventory(metadata, target)
    if (report.get("schema") != 1 or report.get("target") != target or report.get("passed") is not True
            or report.get("scope") != "bundled distro ELF shared libraries" or report.get("identity_link") != IDENTITY_NOTE
            or report.get("inventory_sha256") != digest(metadata / "linux-native/inventory.json")
            or report.get("binaries") != binaries or not 0 < len(report.get("libraries", [])) <= MAX_LIBRARIES):
        raise ValueError("Linux bundled native notice proof differs from its inventory/installed binaries")
    sources = {(identity_key(item["identity"]), item["package"], item["sha256"]) for item in inventory["libraries"]}
    seen = set()
    for item in report["libraries"]:
        path = Path(item.get("path", ""))
        if (path.is_absolute() or ".." in path.parts or not path.parts or path.as_posix() in seen
                or not re.fullmatch(r"[0-9a-f]{64}", item.get("sha256", ""))
                or not isinstance(item.get("size_bytes"), int) or item["size_bytes"] < 1
                or (identity_key(item.get("identity", {})), item.get("package"), item.get("source_sha256")) not in sources):
            raise ValueError("Invalid or unmapped packaged Linux native library identity")
        seen.add(path.as_posix())
    architecture = "aarch64" if target.startswith("aarch64") else "x86_64"
    expected = {entry["id"]: entry for entry in validate_infrastructure_inventory(metadata) if entry["architecture"] == architecture}
    infrastructure = report.get("packaging_infrastructure", [])
    if (report.get("infrastructure_catalogue_sha256") != digest(metadata / "linux-infrastructure/catalogue.json")
            or len(infrastructure) != 2 or {entry.get("id") for entry in infrastructure} != set(expected)):
        raise ValueError("Linux infrastructure notice proof is incomplete or changed")
    for record in infrastructure:
        source = expected[record["id"]]
        if (record.get("architecture") != architecture or record.get("source_sha256") != source["sha256"]
                or record.get("size_bytes") != source["size_bytes"]
                or not re.fullmatch(r"[0-9a-f]{64}", record.get("sha256", ""))
                or record["id"] == "apprun" and record["sha256"] != source["sha256"]
                or record["id"] == "appimage-runtime" and record.get("normalized_sha256") != source["normalized_sha256"]):
            raise ValueError("Linux infrastructure notice proof differs from its reviewed binary identity")
    image = report.get("appimage", {})
    if (Path(image.get("name", "")).name != image.get("name") or not image["name"].endswith(".AppImage")
            or not re.fullmatch(r"[0-9a-f]{64}", image.get("sha256", ""))
            or not isinstance(image.get("size_bytes"), int) or image["size_bytes"] <= 0):
        raise ValueError("Linux infrastructure proof is missing its actual AppImage identity")
    if debian_required:
        package = report.get("debian", {})
        if (Path(package.get("name", "")).name != package.get("name") or not package["name"].endswith(".deb")
                or not re.fullmatch(r"[0-9a-f]{64}", package.get("sha256", ""))
                or not isinstance(package.get("size_bytes"), int) or package["size_bytes"] < 1
                or package.get("inventory_sha256") != report["inventory_sha256"]
                or package.get("infrastructure_catalogue_sha256") != report["infrastructure_catalogue_sha256"]):
            raise ValueError("Linux native notice proof has no matching Debian archive verification")
