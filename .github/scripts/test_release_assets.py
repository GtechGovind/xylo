import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from publish_release import validate_receipts, wait_for_ci
from release_assets import platform_name, sha256, verify_macos_bundle


@unittest.skipUnless(sys.platform == 'darwin', 'Requires native macOS codesign')
class MacBundleIntegrityTests(unittest.TestCase):
    def test_linker_signature_is_insufficient_and_tampered_resources_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            app = root / 'Xylo.app'
            executables = app / 'Contents/MacOS'
            helper = app / 'Contents/Resources/sandbox-helper'
            executables.mkdir(parents=True)
            (helper / 'node_modules').mkdir(parents=True)
            (helper / 'run.mjs').write_text('// original helper\n')
            (app / 'Contents/Info.plist').write_bytes(plistlib.dumps({
                'CFBundleExecutable': 'xylo', 'CFBundleIdentifier': 'com.qurkos.xylo',
                'CFBundlePackageType': 'APPL', 'CFBundleShortVersionString': '0.0.0-test'}))
            binary = root / 'fixture'
            subprocess.run(['cc', '-x', 'c', '-', '-o', str(binary)],
                input='int main(void) { return 0; }\n', text=True, check=True)
            for name in ('xylo', 'xylo-worker', 'xylo-helper', 'xylo-cli'):
                shutil.copy2(binary, executables / name)
            with self.assertRaises(subprocess.CalledProcessError):
                verify_macos_bundle(app, '0.0.0-test')
            for executable in executables.iterdir():
                subprocess.run(['codesign', '--force', '--sign', '-', str(executable)], check=True)
            subprocess.run(['codesign', '--force', '--sign', '-', str(app)], check=True)
            verify_macos_bundle(app, '0.0.0-test')
            (helper / 'run.mjs').write_text('// changed after signing\n')
            with self.assertRaises(subprocess.CalledProcessError):
                verify_macos_bundle(app, '0.0.0-test')


class ReleaseAssetsTests(unittest.TestCase):
    def fixture(self, root):
        for platform in ('macos', 'windows', 'linux'):
            folder = root / platform
            folder.mkdir()
            artifact = folder / 'application.bin'
            artifact.write_bytes(b'fixture download')
            Path(str(artifact) + '.sig').write_text('verified during packaging')
            receipt = folder / f'Xylo-{platform}.json'
            receipt.write_text(json.dumps({'version': '0.1.1-rc.1', 'commit': 'fixture',
                'platform': platform, 'artifact_signatures': 'verified',
                'files': [{'name': artifact.name, 'sha256': sha256(artifact), 'size': artifact.stat().st_size}]}))
            Path(str(receipt) + '.sig').write_text('verified during packaging')

    def test_all_native_platforms_and_matching_contents_are_required(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.fixture(root)
            self.assertEqual(len(validate_receipts(root, '0.1.1-rc.1', 'fixture')), 3)
            (root / 'windows/Xylo-windows.json').unlink()
            with self.assertRaisesRegex(ValueError, 'three platform'):
                validate_receipts(root, '0.1.1-rc.1', 'fixture')

    def test_changed_artifact_and_mixed_commits_are_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.fixture(root)
            with self.assertRaisesRegex(ValueError, 'different versions or commits'):
                validate_receipts(root, '0.1.1-rc.1', 'other')
            (root / 'macos/application.bin').write_bytes(b'tampered download')
            with self.assertRaisesRegex(ValueError, 'does not match'):
                validate_receipts(root, '0.1.1-rc.1', 'fixture')

    def test_macos_can_be_published_early_only_when_explicitly_requested(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.fixture(root)
            mac = root / 'macos'
            self.assertEqual(len(validate_receipts(mac, '0.1.1-rc.1', 'fixture', {'macos'})), 1)
            with self.assertRaisesRegex(ValueError, 'three platform'):
                validate_receipts(mac, '0.1.1-rc.1', 'fixture')

    def test_missing_signature_is_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.fixture(root)
            (root / 'linux/application.bin.sig').unlink()
            with self.assertRaisesRegex(ValueError, 'signature is missing'):
                validate_receipts(root, '0.1.1-rc.1', 'fixture')

    def test_native_architectures_have_consistent_labels(self):
        self.assertEqual(platform_name('Windows', 'AMD64'), ('windows', 'x64'))
        self.assertEqual(platform_name('Linux', 'x86_64'), ('linux', 'x64'))
        self.assertEqual(platform_name('Darwin', 'arm64'), ('macos', 'arm64'))

    def test_failed_ci_blocks_full_publication(self):
        with patch('publish_release.subprocess.check_output', return_value=json.dumps([
            {'status': 'completed', 'conclusion': 'failure', 'url': 'https://example.invalid/ci'}
        ])):
            with self.assertRaisesRegex(ValueError, 'CI must pass'):
                wait_for_ci('owner/repo', 'fixture')

    def test_passing_ci_allows_full_publication(self):
        with patch('publish_release.subprocess.check_output', return_value=json.dumps([
            {'status': 'completed', 'conclusion': 'success', 'url': 'https://example.invalid/ci'}
        ])):
            wait_for_ci('owner/repo', 'fixture')


if __name__ == '__main__':
    unittest.main()
