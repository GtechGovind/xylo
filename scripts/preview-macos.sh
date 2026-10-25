#!/usr/bin/env bash
# Build a development bundle. This is not a signed distribution installer.
set -euo pipefail

if [[ "$(uname -s)" != Darwin ]]; then
  echo "This preview bundler requires macOS." >&2
  exit 1
fi

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$project_root"
cargo build --locked -p agent-workbench -p workbench-worker -p workbench-helper

preview_bundle="$project_root/target/debug/Xylo.app"
preview_macos="$preview_bundle/Contents/MacOS"
preview_resources="$preview_bundle/Contents/Resources"
mkdir -p -- "$preview_macos" "$preview_resources/licenses"
for executable in agent-workbench workbench-worker workbench-helper; do
  install -m 755 "target/debug/$executable" "$preview_macos/$executable"
done
rm -rf -- "$preview_macos/native-assets"
if [[ -d target/debug/native-assets ]]; then
  # Asset manifests hash the signed native bytes; preserve them while bundling.
  rm -rf -- "$preview_resources/native-assets"
  cp -R target/debug/native-assets "$preview_resources/native-assets"
fi
install -m 644 packaging/icons/app.icns "$preview_resources/app.icns"
cp LICENSE NOTICE "$preview_resources/licenses/"
cp vendor/hermes-ultra/LICENSE "$preview_resources/licenses/Hermes-Ultra-LICENSE"
cp vendor/hermes-ultra/LICENSE-UPSTREAM "$preview_resources/licenses/Hermes-upstream-LICENSE"
cp vendor/hermes-ultra/NOTICE "$preview_resources/licenses/Hermes-NOTICE"

# Build-time metadata only: Python is not packaged or used by the application.
# Keep the preview's dependency notices subject to the same pinned text/hash
# verification as installers. Unresolved gaps remain visible in its inventory.
preview_target="$(rustc -vV | sed -n 's/^host: //p')"
python3 scripts/release_metadata.py \
  --target "$preview_target" \
  --output "$preview_resources/licenses/dependencies"
python3 scripts/release_metadata.py \
  --validate-inventory "$preview_resources/licenses/dependencies"

cat > "$preview_bundle/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Xylo</string>
  <key>CFBundleDisplayName</key><string>Xylo</string>
  <key>CFBundleExecutable</key><string>agent-workbench</string>
  <key>CFBundleIconFile</key><string>app.icns</string>
  <key>CFBundleIdentifier</key><string>com.qurkos.xylo.preview</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key><string>Use speech input when you request it.</string>
  <key>NSScreenCaptureUsageDescription</key><string>Capture your screen when you request it.</string>
</dict></plist>
PLIST
plutil -lint "$preview_bundle/Contents/Info.plist"
for executable in workbench-worker workbench-helper; do
  codesign --force --sign - "$preview_macos/$executable"
done
codesign --force --sign - "$preview_bundle"
codesign --verify --deep --strict "$preview_bundle"
echo "Built: $preview_bundle"
