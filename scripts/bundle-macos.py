#!/usr/bin/env python3
"""Build a complete local macOS app; no links into Cargo's disposable cache."""
import argparse
import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--profile', choices=('debug', 'release'), default='release')
parser.add_argument('--skip-build', action='store_true')
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
version = json.loads((root / 'packager.json').read_text())['version']
if not args.skip_build:
    command = ['cargo', 'build', '--locked', '--bins']
    if args.profile == 'release':
        command.append('--release')
    subprocess.run(command, cwd=root, check=True)
names = ('xylo', 'xylo-worker', 'xylo-helper', 'xylo-cli')
source = root / 'target' / args.profile
for name in names:
    if not (source / name).is_file():
        raise SystemExit(f'Missing {source / name}; run without --skip-build to build the complete app.')
out = root / 'dist'
out.mkdir(exist_ok=True)
with tempfile.TemporaryDirectory(prefix='xylo-bundle-', dir=out) as temporary:
    stage = Path(temporary) / 'Xylo.app'
    macos = stage / 'Contents' / 'MacOS'
    resources = stage / 'Contents' / 'Resources'
    macos.mkdir(parents=True)
    resources.mkdir()
    for name in names:
        shutil.copy2(source / name, macos / name)
    with (stage / 'Contents' / 'Info.plist').open('wb') as handle:
        plistlib.dump(dict(CFBundleExecutable='xylo', CFBundleDisplayName='Xylo',
            CFBundleName='Xylo', CFBundleIdentifier='com.qurkos.xylo', CFBundlePackageType='APPL',
            CFBundleShortVersionString=version, CFBundleVersion=version.split('-')[0], CFBundleIconFile='xylo.icns',
            NSHighResolutionCapable=True, NSPrincipalClass='NSApplication'), handle)
    iconset = Path(temporary) / 'xylo.iconset'
    iconset.mkdir()
    for size in (16, 32, 128, 256, 512):
        for scale in (1, 2):
            filename = f'icon_{size}x{size}' + ('@2x' if scale == 2 else '') + '.png'
            subprocess.run(['sips', '-z', str(size * scale), str(size * scale),
                str(root / 'crates/desktop/assets/xylo-icon.png'), '--out', str(iconset / filename)],
                check=True, stdout=subprocess.DEVNULL)
    subprocess.run(['iconutil', '-c', 'icns', str(iconset), '-o', str(resources / 'xylo.icns')], check=True)
    helper = root / 'crates/services/sandbox-helper'
    subprocess.run(['npm', 'ci', '--prefix', str(helper), '--ignore-scripts', '--no-audit', '--no-fund'], check=True)
    shutil.copytree(helper, resources / 'sandbox-helper', ignore=shutil.ignore_patterns('.git'))
    for name in ('LICENSE', 'NOTICE'):
        shutil.copy2(root / name, resources / name)
    destination = out / 'Xylo.app'
    # Stage the complete bundle before replacing the local build.
    if destination.exists():
        shutil.rmtree(destination)
    shutil.move(str(stage), destination)
print(f'Built {out / "Xylo.app"} ({args.profile}); all four executables are included.')
