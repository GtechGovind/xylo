"""Exact-version supplement and copied-package validation; no Cargo/network calls."""
import hashlib
import copy
import io
import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
import release_metadata
from release_common import ROOT, digest
from release_licenses import (ATTRIBUTION_POLICY, CANONICAL_COMMIT, LicenseSupplements,
                              canonical_choice, git_blob)

REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"


class SupplementTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.directory = self.root / "packaging/licenses"
        self.directory.mkdir(parents=True)
        checked = LicenseSupplements(ROOT / "packaging/licenses")
        original = next(t for t in checked.texts.values() if t.get("license_id") == "MIT")
        self.license = (checked.directory / original["file"]).read_bytes()
        (self.directory / "MIT.txt").write_bytes(self.license)
        self.commit = "1" * 40
        self.cargo = b'[package]\nname="fixture-license"\nversion="1.2.3"\nlicense="MIT"\nrepository="https://github.com/example/fixture"\n'
        self.vcs = json.dumps({"git": {"sha1": self.commit}, "path_in_vcs": "crate"}).encode()
        self.members = {"Cargo.toml": self.cargo, ".cargo_vcs_info.json": self.vcs, "src/lib.rs": b"// published source\n"}
        self.local = self.root / "registry/src/index/fixture-license-1.2.3"
        self.local.mkdir(parents=True)
        (self.local / "Cargo.toml").write_bytes(self.cargo)
        self.archive = self.root / "registry/cache/index/fixture-license-1.2.3.crate"
        self.archive.parent.mkdir(parents=True)
        self.package = {"id": "fixture-id", "name": "fixture-license", "version": "1.2.3", "license": "MIT", "repository": "https://github.com/example/fixture", "source": REGISTRY, "manifest_path": str(self.local / "Cargo.toml")}
        self.catalogue = {"schema_version": 1, "texts": [{"id": "mit", "file": "MIT.txt", "kind": "license", "license_id": "MIT", "sha256": digest(self.directory / "MIT.txt"), "git_blob_sha1": git_blob(self.license), "repository": "example/fixture", "commit": self.commit, "path": "LICENSE", "url": f"https://github.com/example/fixture/blob/{self.commit}/LICENSE"}], "packages": [{"name": "fixture-license", "version": "1.2.3", "registry_source": REGISTRY, "published_repository": self.package["repository"], "license_expression": "MIT", "archive_sha256": "", "cargo_toml_sha256": hashlib.sha256(self.cargo).hexdigest(), "upstream": {"repository": "example/fixture", "commit": self.commit, "path": "crate/Cargo.toml", "url": f"https://github.com/example/fixture/blob/{self.commit}/crate/Cargo.toml"}, "proof": {"kind": "cargo_vcs", "vcs_metadata_sha256": hashlib.sha256(self.vcs).hexdigest(), "publication_path_in_vcs": "crate", "dirty": False}, "texts": ["mit"]}]}
        self.rebuild_archive()

    def rebuild_archive(self):
        with tarfile.open(self.archive, "w:gz") as crate:
            for name, data in self.members.items():
                info = tarfile.TarInfo("fixture-license-1.2.3/" + name)
                info.size = len(data)
                crate.addfile(info, io.BytesIO(data))
        self.checksum = digest(self.archive)
        self.catalogue["packages"][0]["archive_sha256"] = self.checksum
        self.save_catalogue()

    def save_catalogue(self):
        (self.directory / "supplements.json").write_text(json.dumps(self.catalogue))

    def resolve(self):
        return LicenseSupplements(self.directory).resolve(self.package, self.checksum)

    def generate(self, strict=True):
        output = self.root / "inventory"
        (self.root / "Cargo.lock").write_text(f'[[package]]\nname="fixture-license"\nversion="1.2.3"\nsource="{REGISTRY}"\nchecksum="{self.checksum}"\n')
        metadata = {"packages": [self.package], "resolve": {"nodes": [{"id": "fixture-id", "deps": []}]}}
        with patch.object(release_metadata, "ROOT", self.root), patch.object(release_metadata, "cargo_inventory", return_value=(metadata, {"fixture-id"})), patch.object(release_metadata, "run", return_value="fixture toolchain"), patch.object(release_metadata, "source_identity", return_value={"commit": None}), patch.object(release_metadata.platform, "system", return_value="Linux"):
            release_metadata.generate("fixture-target", output, strict=strict)
        return output

    def test_actual_copied_license_and_proof_validate_without_network(self):
        self.assertTrue(self.resolve()["has_full_text"])
        output = self.generate()
        inventory = json.loads((output / "licenses.json").read_text())
        self.assertEqual(inventory["missing_texts"], [])
        self.assertEqual(inventory["supplements"]["applied_packages"], 1)
        text = inventory["packages"][0]["texts"][0]
        self.assertEqual((output / text["path"]).read_bytes(), self.license)
        release_metadata.validate_inventory(output, strict=True)

    def test_actual_bundle_resource_copy_retains_verified_supplements(self):
        import shutil
        generated = self.generate()
        resources = self.root / "Xylo.app/Contents/Resources/licenses/dependencies"
        shutil.copytree(generated, resources)
        release_metadata.validate_inventory(resources, strict=True)
        inventory = json.loads((resources / "licenses.json").read_text())
        (resources / inventory["packages"][0]["texts"][0]["path"]).unlink()
        with self.assertRaisesRegex(ValueError, "regular contained file"):
            release_metadata.validate_inventory(resources, strict=True)

    def test_changed_lock_source_repository_or_expression_cannot_reuse_supplement(self):
        with self.assertRaises(ValueError):
            LicenseSupplements(self.directory).resolve(self.package, "2" * 64)
        for key, value in (("source", None), ("repository", "https://github.com/other/owner"), ("license", "Apache-2.0")):
            with self.subTest(key=key):
                changed = {**self.package, key: value}
                with self.assertRaises(ValueError):
                    LicenseSupplements(self.directory).resolve(changed, self.checksum)

    def test_version_update_requires_a_new_explicit_mapping(self):
        self.assertIsNone(LicenseSupplements(self.directory).resolve({**self.package, "version": "1.2.4"}, self.checksum))

    def test_archive_or_text_tampering_is_not_accepted(self):
        self.archive.write_bytes(self.archive.read_bytes() + b"tampered")
        with self.assertRaisesRegex(ValueError, "archive checksum"):
            self.resolve()
        (self.directory / "MIT.txt").write_bytes(b"edited notice")
        with self.assertRaisesRegex(ValueError, "text hash"):
            LicenseSupplements(self.directory)

    def test_guidance_cannot_be_relabelled_as_full_license(self):
        data = b"Licensed MIT. See https://opensource.org/license/mit\n"
        (self.directory / "MIT.txt").write_bytes(data)
        self.catalogue["texts"][0].update(sha256=hashlib.sha256(data).hexdigest(), git_blob_sha1=git_blob(data))
        self.save_catalogue()
        with self.assertRaisesRegex(ValueError, "not full license"):
            self.resolve()
        self.catalogue["texts"][0]["kind"] = "licensing_guidance"
        self.save_catalogue()
        self.assertFalse(self.resolve()["has_full_text"])
        with self.assertRaisesRegex(ValueError, "review required for 1"):
            self.generate()
        self.assertEqual(json.loads((self.root / "inventory/licenses.json").read_text())["missing_texts"], ["fixture-license 1.2.3"])

    def test_dirty_vcs_or_metadata_change_requires_new_verified_proof(self):
        self.members[".cargo_vcs_info.json"] = json.dumps({"git": {"sha1": self.commit, "dirty": True}, "path_in_vcs": "crate"}).encode()
        self.rebuild_archive()
        with self.assertRaisesRegex(ValueError, "VCS metadata"):
            self.resolve()
        self.catalogue["packages"][0]["proof"].update(vcs_metadata_sha256=hashlib.sha256(self.members[".cargo_vcs_info.json"]).hexdigest(), dirty=True)
        self.save_catalogue()
        with self.assertRaisesRegex(ValueError, "Dirty publication"):
            self.resolve()

    def test_dirty_publication_accepts_only_the_recorded_dev_dependency_transform(self):
        self.members[".cargo_vcs_info.json"] = json.dumps({"git": {"sha1": self.commit, "dirty": True}, "path_in_vcs": "crate"}).encode()
        self.members["Cargo.toml.keep"] = self.cargo + b'\n[dev-dependencies]\nfixture="1"\n'
        self.members["Cargo.toml.orig"] = self.cargo
        proof = self.catalogue["packages"][0]["proof"]
        proof.update(kind="source_bindings_dirty_publication", dirty=True,
                     vcs_metadata_sha256=hashlib.sha256(self.members[".cargo_vcs_info.json"]).hexdigest(),
                     bindings=[{"archive_path": "Cargo.toml.keep", "upstream_path": "crate/Cargo.toml", "git_blob_sha1": git_blob(self.members["Cargo.toml.keep"])}])
        self.rebuild_archive()
        self.assertTrue(self.resolve()["has_full_text"])
        self.members["Cargo.toml.orig"] += b'description="Unrecorded publication change"\n'
        self.rebuild_archive()
        with self.assertRaisesRegex(ValueError, "unverified manifest change"):
            self.resolve()

    def test_and_license_obligations_are_never_silently_satisfied_by_one_text(self):
        self.catalogue["packages"][0]["license_expression"] = "MIT AND Apache-2.0"
        self.save_catalogue()
        with self.assertRaisesRegex(ValueError, "explicit coverage review"):
            LicenseSupplements(self.directory)

    def test_version_tag_source_binding_must_match_exact_archive_bytes(self):
        self.catalogue["packages"][0]["proof"] = {"kind": "version_tag_source_bindings", "tag": "refs/tags/v1.2.3", "tag_commit": self.commit, "bindings": [{"archive_path": "src/lib.rs", "upstream_path": "crate/src/lib.rs", "git_blob_sha1": git_blob(self.members["src/lib.rs"])}]}
        self.save_catalogue()
        self.assertTrue(self.resolve()["has_full_text"])
        self.members["src/lib.rs"] = b"changed published source"
        self.rebuild_archive()
        with self.assertRaisesRegex(ValueError, "source file differs"):
            self.resolve()

    def test_paths_symlinks_and_wrong_upstream_ownership_are_rejected(self):
        for value in ("../outside", "/absolute", "texts/../MIT.txt"):
            with self.subTest(value=value):
                self.catalogue["texts"][0]["file"] = value
                self.save_catalogue()
                with self.assertRaises(ValueError):
                    LicenseSupplements(self.directory)
        self.catalogue["texts"][0]["file"] = "link.txt"
        (self.directory / "link.txt").symlink_to(self.directory / "MIT.txt")
        self.save_catalogue()
        with self.assertRaisesRegex(ValueError, "symbolic"):
            LicenseSupplements(self.directory)
        self.catalogue["texts"][0]["file"] = "MIT.txt"
        self.catalogue["texts"][0].update(repository="other/owner", url=f"https://github.com/other/owner/blob/{self.commit}/LICENSE")
        self.save_catalogue()
        with self.assertRaisesRegex(ValueError, "ownership"):
            LicenseSupplements(self.directory)

    def test_packaged_bytes_and_provenance_are_checked_after_copy(self):
        output = self.generate()
        inventory = json.loads((output / "licenses.json").read_text())
        text = output / inventory["packages"][0]["texts"][0]["path"]
        text.write_bytes(b"changed after copying")
        with self.assertRaisesRegex(ValueError, "license hash"):
            release_metadata.validate_inventory(output)
        text.write_bytes(self.license)
        (output / "license-supplements.json").write_text("{}")
        with self.assertRaisesRegex(ValueError, "provenance hash"):
            release_metadata.validate_inventory(output)

    def test_packaged_notice_set_must_include_non_license_upstream_notices(self):
        data = b"Original upstream attribution; retain with redistributions.\n"
        (self.directory / "NOTICE").write_bytes(data)
        self.catalogue["texts"].append({"id": "notice", "file": "NOTICE", "kind": "notice",
            "sha256": hashlib.sha256(data).hexdigest(), "git_blob_sha1": git_blob(data),
            "repository": "example/fixture", "commit": self.commit, "path": "NOTICE",
            "url": f"https://github.com/example/fixture/blob/{self.commit}/NOTICE"})
        self.catalogue["packages"][0]["texts"].append("notice")
        self.save_catalogue()
        output = self.generate()
        inventory_path = output / "licenses.json"
        inventory = json.loads(inventory_path.read_text())
        texts = inventory["packages"][0]["texts"]
        notice = next(t for t in texts if t["kind"] == "notice")
        (output / notice["path"]).unlink()
        texts.remove(notice)
        inventory_path.write_text(json.dumps(inventory))
        with self.assertRaisesRegex(ValueError, "auxiliary notice is missing"):
            release_metadata.validate_inventory(output, strict=True)
        forged = copy.deepcopy(texts[0])
        forged["evidence"]["id"] = "notice"
        texts.append(forged)
        inventory_path.write_text(json.dumps(inventory))
        with self.assertRaisesRegex(ValueError, "evidence differs from its immutable proof"):
            release_metadata.validate_inventory(output, strict=True)

    def test_checked_in_catalogue_contains_only_exact_pins_and_full_text_coverage(self):
        catalogue = LicenseSupplements(ROOT / "packaging/licenses")
        for entry in catalogue.packages.values():
            self.assertTrue(any(catalogue.texts[key]["kind"] == "license" for key in entry["texts"]))
            if entry["proof"]["kind"] != "registry_manifest_grant":
                self.assertEqual(len(entry["upstream"]["commit"]), 40)
        self.assertEqual(len(catalogue.packages), 121)


class CanonicalSupplementTests(unittest.TestCase):
    """The real copied-inventory path must preserve grants and source classes."""

    def setUp(self):
        SupplementTests.setUp(self)
        checked = LicenseSupplements(ROOT / "packaging/licenses")
        original = checked.texts["canonical-spdx-MIT"]
        self.license = (checked.directory / original["file"]).read_bytes()
        (self.directory / "MIT.txt").write_bytes(self.license)
        canonical = {**original, "id": "mit", "file": "MIT.txt"}
        self.members["Cargo.toml"] = self.cargo = self.cargo + b'authors=["Published Author"]\n'
        self.members["Cargo.toml.orig"] = self.cargo
        self.catalogue = {"schema_version": 2, "texts": [canonical], "packages": [{
            "name": self.package["name"], "version": self.package["version"],
            "registry_source": REGISTRY, "published_repository": self.package["repository"],
            "license_expression": "MIT", "archive_sha256": "",
            "cargo_toml_sha256": hashlib.sha256(self.cargo).hexdigest(),
            "publication_url": "https://static.crates.io/crates/fixture-license/fixture-license-1.2.3.crate",
            "proof": {"kind": "registry_manifest_grant", "selected_license": "MIT",
                      "attribution_policy": ATTRIBUTION_POLICY,
                      "grant": {"archive_path": "Cargo.toml", "sha256": hashlib.sha256(self.cargo).hexdigest(),
                                "license_expression": "MIT", "authors": ["Published Author"],
                                "authors_field_present": True},
                      "notice_paths": sorted(self.members)},
            "texts": ["mit"]}]}
        for index, (path, data) in enumerate(sorted(self.members.items())):
            if path == "src/lib.rs":
                continue
            file = f"notice-{index}.txt"
            (self.directory / file).write_bytes(data)
            key = f"notice-{index}"
            self.catalogue["texts"].append({"id": key, "file": file,
                "kind": "documentation_notice", "source_kind": "registry_archive",
                "sha256": hashlib.sha256(data).hexdigest(), "git_blob_sha1": git_blob(data),
                "publication": {"name": self.package["name"], "version": self.package["version"],
                                "archive_sha256": "", "archive_path": path}})
            self.catalogue["packages"][0]["texts"].append(key)
        self.catalogue["packages"][0]["proof"]["notice_paths"].remove("src/lib.rs")
        self.rebuild_archive()

    def rebuild_archive(self):
        SupplementTests.rebuild_archive(self)
        for text in self.catalogue["texts"]:
            if text.get("source_kind") == "registry_archive":
                text["publication"]["archive_sha256"] = self.checksum
        self.save_catalogue()

    save_catalogue = SupplementTests.save_catalogue
    resolve = SupplementTests.resolve
    generate = SupplementTests.generate

    # The upstream-only inherited tests are intentionally not duplicated: their
    # fixtures exercise a different proof class. These are independent flows.
    def test_canonical_actual_copy_preserves_grant_attribution_and_source_distinction(self):
        output = self.generate()
        release_metadata.validate_inventory(output, strict=True)
        inventory = json.loads((output / "licenses.json").read_text())
        self.assertEqual(inventory["missing_texts"], [])
        texts = inventory["packages"][0]["texts"]
        canonical = next(t["evidence"] for t in texts if t["kind"] == "license")
        self.assertEqual(canonical["repository"], "spdx/license-list-data")
        self.assertEqual(canonical["commit"], CANONICAL_COMMIT)
        provenance = json.loads((output / "license-supplements.json").read_text())
        grant = provenance["packages"][0]["proof"]["grant"]
        self.assertEqual(grant["authors"], ["Published Author"])
        self.assertNotIn("copyright", grant)

    def test_canonical_wrong_authority_commit_or_standard_bytes_are_rejected(self):
        for key, value in (("authority", "Package upstream"), ("commit", "2" * 40),
                           ("repository", "example/fixture"), ("source_kind", "upstream_file")):
            with self.subTest(key=key):
                original = self.catalogue["texts"][0].copy()
                self.catalogue["texts"][0][key] = value
                self.save_catalogue()
                with self.assertRaises(ValueError):
                    self.resolve()
                self.catalogue["texts"][0] = original
        data = self.license.replace(b"Permission is hereby granted", b"Permission was withdrawn")
        self.assertNotEqual(data, self.license)
        (self.directory / "MIT.txt").write_bytes(data)
        self.catalogue["texts"][0].update(sha256=hashlib.sha256(data).hexdigest(), git_blob_sha1=git_blob(data))
        self.save_catalogue()
        with self.assertRaisesRegex(ValueError, "canonical license text or provenance"):
            self.resolve()

    def test_altered_grant_or_ambiguous_author_attribution_is_rejected(self):
        grant = self.catalogue["packages"][0]["proof"]["grant"]
        for key, value in (("license_expression", "Apache-2.0"), ("authors", ["Invented Owner"]),
                           ("authors_field_present", False)):
            with self.subTest(key=key):
                original = grant[key]
                grant[key] = value
                self.save_catalogue()
                with self.assertRaises(ValueError):
                    self.resolve()
                grant[key] = original
        self.catalogue["packages"][0]["proof"]["attribution_policy"] = "Cargo authors own copyright"
        self.save_catalogue()
        with self.assertRaisesRegex(ValueError, "attribution mismatch"):
            self.resolve()

    def test_canonical_choice_requires_explicit_or_grant_and_prefers_apache(self):
        self.assertEqual(canonical_choice("MIT OR Apache-2.0"), "Apache-2.0")
        self.assertEqual(canonical_choice("MIT/Apache-2.0"), "Apache-2.0")
        self.assertEqual(canonical_choice("Zlib OR Apache-2.0 OR MIT"), "Apache-2.0")
        for expression in ("MIT AND Apache-2.0", "(MIT OR Apache-2.0)", "MIT OR Unknown",
                           "MIT OR MIT", "LicenseRef-Custom", "", "Zlib"):
            with self.subTest(expression=expression), self.assertRaises(ValueError):
                canonical_choice(expression)
        self.catalogue["packages"][0]["proof"]["selected_license"] = "Apache-2.0"
        self.save_catalogue()
        with self.assertRaisesRegex(ValueError, "grant or attribution mismatch"):
            self.resolve()

    def test_missing_or_cross_package_publication_notices_are_rejected(self):
        notice = self.catalogue["texts"][1]
        notice["publication"]["name"] = "another-package"
        self.save_catalogue()
        with self.assertRaisesRegex(ValueError, "another publication"):
            self.resolve()
        notice["publication"]["name"] = self.package["name"]
        self.catalogue["packages"][0]["texts"].remove(notice["id"])
        self.save_catalogue()
        with self.assertRaisesRegex(ValueError, "notice coverage"):
            self.resolve()

    def test_new_copyright_notice_in_exact_archive_cannot_be_silently_omitted(self):
        self.members["src/owned.rs"] = b"// Copyright (c) 2020 Original Author\n// Licensed under MIT.\n"
        self.rebuild_archive()
        with self.assertRaisesRegex(ValueError, "notices were omitted"):
            self.resolve()

    def test_conflicting_original_publication_grant_remains_unresolved(self):
        path = "Cargo.toml.orig"
        data = self.members[path].replace(b'license="MIT"', b'license="Apache-2.0"')
        self.assertNotEqual(data, self.members[path])
        self.members[path] = data
        text = next(t for t in self.catalogue["texts"] if t.get("publication", {}).get("archive_path") == path)
        (self.directory / text["file"]).write_bytes(data)
        text.update(sha256=hashlib.sha256(data).hexdigest(), git_blob_sha1=git_blob(data))
        self.rebuild_archive()
        with self.assertRaisesRegex(ValueError, "publication license grants conflict"):
            self.resolve()

    def test_packaged_canonical_class_and_grant_are_rechecked_after_copy(self):
        output = self.generate()
        inventory_path = output / "licenses.json"
        inventory = json.loads(inventory_path.read_text())
        canonical = next(t for t in inventory["packages"][0]["texts"] if t["kind"] == "license")
        canonical["evidence"]["source_kind"] = "upstream_file"
        inventory_path.write_text(json.dumps(inventory))
        with self.assertRaisesRegex(ValueError, "ownership"):
            release_metadata.validate_inventory(output, strict=True)

    def test_packaged_grant_alteration_cannot_hide_behind_updated_provenance_digest(self):
        output = self.generate()
        provenance_path = output / "license-supplements.json"
        provenance = json.loads(provenance_path.read_text())
        provenance["packages"][0]["proof"]["grant"]["authors"] = ["Invented Owner"]
        provenance_path.write_text(json.dumps(provenance))
        inventory_path = output / "licenses.json"
        inventory = json.loads(inventory_path.read_text())
        inventory["supplements"]["provenance_sha256"] = digest(provenance_path)
        inventory_path.write_text(json.dumps(inventory))
        with self.assertRaisesRegex(ValueError, "author attribution was altered"):
            release_metadata.validate_inventory(output, strict=True)

    def test_copied_proof_cannot_be_orphaned_or_inventory_package_duplicated(self):
        output = self.generate()
        inventory_path = output / "licenses.json"
        inventory = json.loads(inventory_path.read_text())
        package = inventory["packages"][0]
        inventory["packages"] = []
        inventory_path.write_text(json.dumps(inventory))
        with self.assertRaisesRegex(ValueError, "no corresponding dependency"):
            release_metadata.validate_inventory(output, strict=True)
        inventory["packages"] = [package, package]
        inventory_path.write_text(json.dumps(inventory))
        with self.assertRaisesRegex(ValueError, "Duplicate packaged dependency"):
            release_metadata.validate_inventory(output, strict=True)


if __name__ == "__main__":
    unittest.main()
