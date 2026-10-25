"""Offline, exact-version license supplements for registry packages.

The checked-in catalogue is provenance evidence, not legal approval. Package
archives and pinned upstream text bytes must match before any supplement is used.
Canonical standard texts are a separate policy: they are never represented as a
license file from the package's repository, nor as a copyright attribution.
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import tarfile
import tomllib

from release_common import digest

HASH = re.compile(r"[0-9a-f]{64}\Z")
GIT_HASH = re.compile(r"[0-9a-f]{40}\Z")
KINDS = {"license", "notice", "licensing_guidance", "documentation_notice"}
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
CANONICAL_COMMIT = "31ba1a50e5397e00a304dbadc76531740e89ee48"
# Reviewed SPDX License List 3.29.0 bytes. Changing the catalogue alone cannot
# replace a canonical text, its source class, or its immutable upstream pin.
CANONICAL = {
    "MIT": ("b05785f9f18e6716bab63424b11454513b9943a222595b70411009202fc592b5",
            "d817195dad53ec992418c28ffca5fbd1cd86502a"),
    "Apache-2.0": ("074e6e32c86a4c0ef8b3ed25b721ca23aca83df277cd88106ef7177c354615ff",
                   "137069b823873b8bcf42979bcf8e9371052d26a2"),
    "CC0-1.0": ("a2010f343487d3f7618affe54f789f5487602331c0a8d03f49e9a7c547cf0499",
                "0e259d42c996742e9e3cba14c677129b2c1b6311"),
}
ATTRIBUTION_POLICY = "preserve_publication_metadata_without_copyright_inference"
MAX_PUBLICATION_MEMBER = 4 * 1024 * 1024


def relative(value: str) -> str:
    path = PurePosixPath(value)
    if not value or path.is_absolute() or ".." in path.parts or "\\" in value or str(path) != value:
        raise ValueError("License evidence path must be a normalized relative path")
    return value


def regular(root: Path, value: str) -> Path:
    root = root.resolve()
    name = relative(value)
    path = root / name
    if any(root.joinpath(*PurePosixPath(name).parts[:index]).is_symlink()
           for index in range(1, len(PurePosixPath(name).parts) + 1)):
        raise ValueError("License evidence must not follow symbolic links")
    if not path.is_file() or not path.resolve().is_relative_to(root.resolve()):
        raise ValueError("License evidence must be a regular contained file")
    return path


def git_blob(data: bytes) -> str:
    return hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest()


def full_license(data: bytes, identifier: str) -> bool:
    """Guard against accidentally labelling a link/short header as full text.

    This deliberately supports only the identifiers curated in this catalogue.
    Adding a new license requires an explicit rule and a provenance review.
    """
    text = " ".join(data.decode("utf-8").lower().split())
    rules = {
        "MIT": ("permission is hereby granted", "the software is provided", "copies or substantial portions"),
        "Apache-2.0": ("apache license", "version 2.0", "end of terms and conditions"),
        "Apache-2.0 WITH LLVM-exception": ("apache license", "end of terms and conditions", "llvm exceptions", "sections 4(a), 4(b) and 4(d)"),
        "MPL-2.0": ("mozilla public license", "version 2.0", "exhibit a", "10. versions of the license"),
        "Zlib": ("this software is provided", "altered source versions", "origin of this software"),
        "CC0-1.0": ("cc0 1.0 universal", "public license fallback", "limitations and disclaimers",
                    "creative commons is not a party to this document"),
        "Unlicense": ("free and unencumbered software released into the public domain",
                      "relinquishment in perpetuity", "the software is provided"),
        "BSD-2-Clause": ("redistributions of source code must retain the above copyright notice",
                         "redistributions in binary form must reproduce the above copyright notice",
                         "this software is provided by the copyright holders and contributors"),
    }
    if identifier not in rules:
        raise ValueError("Unsupported full-license identifier requires an explicit verification rule")
    return all(clause in text for clause in rules[identifier])


def alternatives(expression: str) -> set[str]:
    # Current catalogue contains single licenses and OR alternatives only. Never
    # silently treat an unknown/AND expression as satisfied by one arbitrary text.
    if " AND " in expression or "(" in expression or ")" in expression:
        raise ValueError("Supplement license expression requires explicit coverage review")
    return set(expression.replace("/", " OR ").split(" OR "))


def canonical_choice(expression: str) -> str:
    """Only curated, unambiguous OR grants qualify for the standard-text policy."""
    if expression in ("CC0-1.0", "Unlicense", "BSD-2-Clause"):
        return expression  # Reviewed single grants; not arbitrary OR extensions.
    if not isinstance(expression, str) or not re.fullmatch(
            r"(?:MIT|Apache-2\.0|Zlib)(?:(?: OR |/)(?:MIT|Apache-2\.0|Zlib))*", expression):
        raise ValueError("Canonical grant requires explicit coverage review")
    parts = expression.replace("/", " OR ").split(" OR ")
    if len(parts) != len(set(parts)):
        raise ValueError("Ambiguous canonical grant alternatives")
    if "Apache-2.0" in parts:
        return "Apache-2.0"
    if "MIT" in parts:
        return "MIT"
    raise ValueError("Canonical grant has no reviewed standard text")


def source_kind(text: dict) -> str:
    return text.get("source_kind", "upstream_file")


def canonical_source(text: dict) -> None:
    identifier = text.get("license_id")
    expected = CANONICAL.get(identifier)
    if (source_kind(text) != "canonical_standard" or text.get("kind") != "license"
            or expected is None or text.get("authority") != "SPDX License List 3.29.0"
            or (text.get("sha256"), text.get("git_blob_sha1")) != expected
            or text.get("repository") != "spdx/license-list-data"
            or text.get("commit") != CANONICAL_COMMIT
            or text.get("path") != f"text/{identifier}.txt"):
        raise ValueError("Unreviewed canonical license text or provenance")
    LicenseSupplements._source(text)


def publication_source(entry: dict) -> None:
    if (entry.get("registry_source") != REGISTRY
            or not re.fullmatch(r"[A-Za-z0-9_-]+", entry["name"])
            or not re.fullmatch(r"[A-Za-z0-9_.+-]+", entry["version"])
            or not HASH.fullmatch(entry.get("archive_sha256", ""))
            or not HASH.fullmatch(entry.get("cargo_toml_sha256", ""))
            or entry.get("publication_url") != (
                f"https://static.crates.io/crates/{entry['name']}/{entry['name']}-{entry['version']}.crate")):
        raise ValueError("Invalid exact registry publication provenance")


def registry_text_source(text: dict, entry: dict | None = None) -> None:
    source = text.get("publication", {})
    relative(source.get("archive_path", ""))
    if (source_kind(text) != "registry_archive" or text.get("kind") == "license"
            or any(key in text for key in ("repository", "commit", "url"))
            or not HASH.fullmatch(source.get("archive_sha256", ""))
            or not re.fullmatch(r"[A-Za-z0-9_-]+", source.get("name", ""))
            or not re.fullmatch(r"[A-Za-z0-9_.+-]+", source.get("version", ""))):
        raise ValueError("Invalid registry notice provenance")
    if entry and any(source[key] != entry[key] for key in ("name", "version", "archive_sha256")):
        raise ValueError("Registry notice belongs to another publication")


def publication_notice_paths(members: dict[str, bytes]) -> list[str]:
    """Preserve manifests, root documentation and actual copyright headers.

    This bounded scan is a conservative technical collection rule, not a claim
    that inferred Cargo authors are copyright owners. Unusually large notices
    need a separate reviewed mapping rather than silently truncated text.
    """
    result = set()
    copyright_header = re.compile(
        r"^\s*(?:(?://+|/\*|\*|#|<!--)\s*)?copyright\s*(?:©|\(c\)|[0-9])", re.I | re.M)
    for path, data in members.items():
        name = PurePosixPath(path).name.lower()
        root_document = "/" not in path and name.startswith(("readme", "license", "licence", "copying", "notice"))
        manifest = path in ("Cargo.toml", "Cargo.toml.orig", "Cargo.modified.toml", ".cargo_vcs_info.json")
        try:
            copyright_notice = bool(copyright_header.search(data.decode("utf-8")))
        except UnicodeDecodeError:
            copyright_notice = False
        if root_document or manifest or copyright_notice:
            if len(data) > MAX_PUBLICATION_MEMBER:
                raise ValueError("Publication notice needs an explicit bounded coverage review")
            result.add(path)
    return sorted(result)


def validate_registry_grant(entry: dict, texts: list[dict], read_text) -> None:
    """Validate the same exact grant from source and copied release inventories."""
    publication_source(entry)
    proof = entry["proof"]
    selected = canonical_choice(entry["license_expression"])
    grant = proof.get("grant", {})
    if (proof.get("kind") != "registry_manifest_grant"
            or proof.get("selected_license") != selected
            or proof.get("attribution_policy") != ATTRIBUTION_POLICY
            or set(grant) != {"archive_path", "sha256", "license_expression", "authors", "authors_field_present"}
            or grant["archive_path"] != "Cargo.toml"
            or grant["sha256"] != entry["cargo_toml_sha256"]
            or grant["license_expression"] != entry["license_expression"]
            or not isinstance(grant["authors"], list)
            or not all(isinstance(author, str) and author for author in grant["authors"])
            or not isinstance(grant["authors_field_present"], bool)):
        raise ValueError("Canonical publication grant or attribution mismatch")
    licenses = [text for text in texts if text["kind"] == "license"]
    if len(licenses) != 1 or licenses[0]["license_id"] != selected:
        raise ValueError("Full text does not match the selected publication grant")
    if source_kind(licenses[0]) == "canonical_standard":
        canonical_source(licenses[0])
    elif source_kind(licenses[0]) != "upstream_file":
        raise ValueError("A full standard or exact upstream license is required")
    notices = {}
    for text in texts:
        kind = source_kind(text)
        if kind == "registry_archive":
            registry_text_source(text, entry)
            path = text["publication"]["archive_path"]
            if path in notices:
                raise ValueError("Duplicate publication notice")
            data = read_text(text)
            if hashlib.sha256(data).hexdigest() != text["sha256"] or git_blob(data) != text["git_blob_sha1"]:
                raise ValueError("Registry notice hash mismatch")
            notices[path] = data
        elif kind == "upstream_file":
            LicenseSupplements._source(text)
            upstream = entry.get("upstream", {})
            if (text["repository"], text["commit"]) != (upstream.get("repository"), upstream.get("commit")):
                raise ValueError("Guidance ownership differs from publication source")
        elif kind != "canonical_standard":
            raise ValueError("Unknown license text provenance class")
    if sorted(notices) != proof.get("notice_paths") or "Cargo.toml" not in notices:
        raise ValueError("Publication notice coverage differs from its exact grant proof")
    cargo = notices["Cargo.toml"]
    manifest = tomllib.loads(cargo.decode("utf-8"))["package"]
    if (hashlib.sha256(cargo).hexdigest() != grant["sha256"]
            or (manifest.get("name"), manifest.get("version"), manifest.get("license"), manifest.get("repository"))
            != (entry["name"], entry["version"], entry["license_expression"], entry["published_repository"])
            or manifest.get("authors", []) != grant["authors"]
            or ("authors" in manifest) != grant["authors_field_present"]):
        raise ValueError("Published grant metadata or author attribution was altered")
    if "Cargo.toml.orig" in notices:
        original = tomllib.loads(notices["Cargo.toml.orig"].decode("utf-8"))["package"]
        if isinstance(original.get("license"), str) and original["license"] != manifest["license"]:
            raise ValueError("Original and normalized publication license grants conflict")
        if isinstance(original.get("authors"), list) and original["authors"] != manifest.get("authors", []):
            raise ValueError("Original and normalized publication authors are ambiguous")
    if entry.get("upstream"):
        LicenseSupplements._source(entry["upstream"])
        vcs = json.loads(notices.get(".cargo_vcs_info.json", b"{}"))
        if vcs.get("git", {}).get("sha1") != entry["upstream"]["commit"]:
            raise ValueError("Upstream guidance is not bound to the published VCS record")


def archive_path(package: dict) -> Path:
    directory = Path(package["manifest_path"]).parent
    registry_index = directory.parent
    registry = registry_index.parent.parent
    path = registry / "cache" / registry_index.name / f"{package['name']}-{package['version']}.crate"
    if registry_index.parent.name != "src" or not path.is_file():
        raise ValueError("Exact registry archive is unavailable; cannot apply license supplement")
    return path


class LicenseSupplements:
    def __init__(self, directory: Path):
        self.directory = directory
        self.path = regular(directory, "supplements.json")
        if self.path.stat().st_size > 2 * 1024 * 1024:
            raise ValueError("License supplement catalogue exceeds its bound")
        catalogue = json.loads(self.path.read_text(encoding="utf-8"))
        if catalogue.get("schema_version") not in (1, 2):
            raise ValueError("Unsupported license supplement catalogue version")
        self.texts = {}
        if len(catalogue["texts"]) > 512 or len(catalogue["packages"]) > 4096:
            raise ValueError("License supplement catalogue exceeds its bound")
        for text in catalogue["texts"]:
            key = text["id"]
            if key in self.texts or text["kind"] not in KINDS:
                raise ValueError("Duplicate or invalid license supplement text")
            path = regular(directory, text["file"])
            limit = MAX_PUBLICATION_MEMBER if source_kind(text) == "registry_archive" else 128 * 1024
            if path.stat().st_size > limit:
                raise ValueError("License supplement text exceeds its bound")
            data = path.read_bytes()
            if not HASH.fullmatch(text["sha256"]) or digest(path) != text["sha256"] or git_blob(data) != text["git_blob_sha1"]:
                raise ValueError("License supplement text hash mismatch")
            kind = source_kind(text)
            if kind == "canonical_standard":
                canonical_source(text)
            elif kind == "registry_archive":
                registry_text_source(text)
            elif kind == "upstream_file":
                self._source(text)
            else:
                raise ValueError("Unknown license text provenance class")
            if text["kind"] == "license" and not full_license(data, text["license_id"]):
                raise ValueError("License guidance or abbreviated header is not full license text")
            self.texts[key] = text
        self.packages = {}
        for package in catalogue["packages"]:
            key = (package["name"], package["version"])
            if key in self.packages or not HASH.fullmatch(package["archive_sha256"]):
                raise ValueError("Duplicate or invalid license supplement package")
            if len(package["texts"]) != len(set(package["texts"])):
                raise ValueError("Duplicate license package text reference")
            if package["proof"]["kind"] == "registry_manifest_grant":
                validate_registry_grant(package, [self.texts[key] for key in package["texts"]],
                                        lambda text: regular(directory, text["file"]).read_bytes())
                self.packages[key] = package
                continue
            self._source(package["upstream"])
            for key_text in package["texts"]:
                text = self.texts[key_text]
                if source_kind(text) != "upstream_file":
                    raise ValueError("Upstream-file proof cannot claim canonical or registry provenance")
                if (text["repository"], text["commit"]) != (package["upstream"]["repository"], package["upstream"]["commit"]):
                    raise ValueError("License text ownership does not match the package's pinned source")
                if text["kind"] == "license" and text["license_id"] not in alternatives(package["license_expression"]):
                    raise ValueError("Full license text is not a declared package license alternative")
            self.packages[key] = package

    @staticmethod
    def _source(source: dict) -> None:
        repo = source["repository"]
        commit = source["commit"]
        path = relative(source["path"])
        if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo) or not GIT_HASH.fullmatch(commit):
            raise ValueError("License supplement requires an exact upstream repository/commit")
        if source["url"] != f"https://github.com/{repo}/blob/{commit}/{path}":
            raise ValueError("License supplement source URL must pin its declared commit and path")

    def resolve(self, package: dict, lock_checksum: str | None) -> dict | None:
        entry = self.packages.get((package["name"], package["version"]))
        if entry is None:
            return None
        if package.get("source") != entry["registry_source"] or package.get("repository") != entry["published_repository"]:
            raise ValueError("License supplement package source/repository mismatch")
        if package.get("license") != entry["license_expression"] or lock_checksum != entry["archive_sha256"]:
            raise ValueError("License supplement lock checksum/license expression mismatch")
        archive = archive_path(package)
        if archive.is_symlink() or digest(archive) != lock_checksum:
            raise ValueError("License supplement registry archive checksum mismatch")
        prefix = f"{package['name']}-{package['version']}/"
        with tarfile.open(archive, "r:gz") as crate:
            def read(path: str) -> bytes:
                member = crate.getmember(prefix + relative(path))
                if not member.isfile() or member.size > MAX_PUBLICATION_MEMBER:
                    raise ValueError("Invalid license provenance archive member")
                return crate.extractfile(member).read()
            cargo = read("Cargo.toml")
            if hashlib.sha256(cargo).hexdigest() != entry["cargo_toml_sha256"]:
                raise ValueError("Published Cargo metadata hash mismatch")
            manifest = tomllib.loads(cargo.decode())["package"]
            if (manifest["name"], manifest["version"], manifest.get("license"), manifest.get("repository")) != (package["name"], package["version"], package.get("license"), package.get("repository")):
                raise ValueError("Published Cargo metadata differs from the resolved package")
            proof = entry["proof"]
            if proof["kind"] == "registry_manifest_grant":
                texts = [self.texts[key] for key in entry["texts"]]
                # The archive digest is already verified. Read only bounded,
                # regular members and require all collected notices to survive.
                members = {}
                for member in crate.getmembers():
                    if not member.name.startswith(prefix):
                        raise ValueError("Registry archive member is outside its package")
                    if member.isfile():
                        if member.size > MAX_PUBLICATION_MEMBER:
                            raise ValueError("Publication member needs an explicit bounded coverage review")
                        path = relative(member.name[len(prefix):])
                        if path in members:
                            raise ValueError("Duplicate registry archive member")
                        members[path] = read(path)
                if publication_notice_paths(members) != proof["notice_paths"]:
                    raise ValueError("Registry publication notices were omitted from the supplement")
                for text in texts:
                    if source_kind(text) == "registry_archive":
                        if members.get(text["publication"]["archive_path"]) != regular(self.directory, text["file"]).read_bytes():
                            raise ValueError("Registry notice differs from the exact published archive")
                validate_registry_grant(entry, texts, lambda text: regular(self.directory, text["file"]).read_bytes())
                return {"entry": entry, "texts": texts, "has_full_text": True}
            if proof["kind"] in ("cargo_vcs", "source_bindings_dirty_publication"):
                vcs_bytes = read(".cargo_vcs_info.json")
                vcs = json.loads(vcs_bytes)
                if hashlib.sha256(vcs_bytes).hexdigest() != proof["vcs_metadata_sha256"] or vcs["git"]["sha1"] != entry["upstream"]["commit"] or vcs.get("path_in_vcs", "") != proof["publication_path_in_vcs"] or bool(vcs["git"].get("dirty", False)) != proof["dirty"]:
                    raise ValueError("Published VCS metadata differs from the pinned supplement")
                if proof["dirty"] and proof["kind"] != "source_bindings_dirty_publication":
                    raise ValueError("Dirty publication needs explicit source-file proof")
            elif proof["kind"] != "version_tag_source_bindings":
                raise ValueError("Unknown license supplement publication proof")
            for binding in proof.get("bindings", []):
                if git_blob(read(binding["archive_path"])) != binding["git_blob_sha1"]:
                    raise ValueError("Published source file differs from its upstream binding")
            if proof["kind"] == "source_bindings_dirty_publication":
                # audio-core's publication removes only an unused dev dependency.
                # Its Cargo.toml.keep is the exact pinned upstream manifest.
                original = tomllib.loads(read("Cargo.toml.keep").decode())
                published = tomllib.loads(read("Cargo.toml.orig").decode())
                original.pop("dev-dependencies", None)
                if original != published or not proof.get("bindings"):
                    raise ValueError("Dirty publication contains an unverified manifest change")
            if proof["kind"] == "version_tag_source_bindings":
                if not proof.get("bindings") or proof.get("tag_commit") != entry["upstream"]["commit"]:
                    raise ValueError("Version tag supplement needs pinned source-file proof")
        texts = [self.texts[key] for key in entry["texts"]]
        return {"entry": entry, "texts": texts, "has_full_text": any(text["kind"] == "license" for text in texts)}
