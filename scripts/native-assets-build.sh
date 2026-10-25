#!/usr/bin/env bash
# Build the pinned whisper native component for the distribution's own platform.
# Build-time tools only: curl, CMake, a C/C++ compiler, and system tar. No Node/Python.
set -euo pipefail
revision=d1be6fde11ac6e0407606b4e42fe72d34add8037
source_sha=645445da884df6dbd52b30f82d7be7c01d1767c938714e477e786b88d529e9f4
script_root=$(cd -- "$(dirname -- "$0")/.." && pwd)
output=${1:-"$script_root/target/debug/native-assets/whisper-runtime"}
case "$output" in /*) ;; *) output="$PWD/$output" ;; esac
scratch=$(mktemp -d "${TMPDIR:-/tmp}/workbench-native-assets.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
curl --fail --location --proto '=https' --tlsv1.2 --connect-timeout 15 --max-time 300 --retry 2 \
  "https://codeload.github.com/ggml-org/whisper.cpp/tar.gz/$revision" -o "$scratch/source.tar.gz"
actual=$(cmake -E sha256sum "$scratch/source.tar.gz")
actual=${actual%% *}
[[ "$actual" == "$source_sha" ]] || { printf '%s\n' 'Pinned native source SHA256 mismatch' >&2; exit 1; }
tar -xzf "$scratch/source.tar.gz" -C "$scratch"
source_dir="$scratch/whisper.cpp-$revision"
# Packagers may set an explicit build concurrency. The conservative fallback
# keeps build-time memory bounded; runtime inference uses adaptive resources.
jobs=${NATIVE_BUILD_JOBS:-2}
[[ "$jobs" =~ ^[1-9][0-9]*$ ]] || { printf '%s\n' 'NATIVE_BUILD_JOBS must be a positive integer' >&2; exit 1; }
cmake_platform=()
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*)
    case "${CARGO_BUILD_TARGET:-${PROCESSOR_ARCHITECTURE:-}}" in
      aarch64-*|ARM64|arm64) cmake_platform=(-A ARM64) ;;
      x86_64-*|AMD64|amd64) cmake_platform=(-A x64) ;;
      *) printf '%s\n' 'Set CARGO_BUILD_TARGET to the native Windows ARM64/x64 target' >&2; exit 1 ;;
    esac
    ;;
esac
# Upstream GGML_NATIVE=OFF alone still enables x64 AVX-family defaults. This
# static preview runtime intentionally keeps a portable SSE2 CPU baseline.
# ARM NEON/Metal remain available. Multi-variant shared backend loading requires
# separate loader/packaging qualification; no unmeasured acceleration claim.
cmake -S "$source_dir" -B "$scratch/build" -DCMAKE_BUILD_TYPE=Release \
  "${cmake_platform[@]}" \
  -DWHISPER_BUILD_TESTS=OFF -DWHISPER_BUILD_EXAMPLES=ON -DWHISPER_BUILD_SERVER=OFF \
  -DWHISPER_CURL=OFF -DWHISPER_SDL2=OFF -DBUILD_SHARED_LIBS=OFF -DGGML_NATIVE=OFF \
  -DGGML_SSE42=OFF -DGGML_AVX=OFF -DGGML_AVX2=OFF -DGGML_BMI2=OFF -DGGML_FMA=OFF -DGGML_F16C=OFF \
  -DGGML_METAL_EMBED_LIBRARY=ON
cmake --build "$scratch/build" --config Release --target whisper-cli --parallel "$jobs"
mkdir -p "$output/runtime" "$output/notices"
native_name=whisper-cli
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) native_name=whisper-cli.exe ;; esac
native_binary=''
for candidate in "$scratch/build/bin/$native_name" "$scratch/build/bin/Release/$native_name" "$scratch/build/Release/bin/$native_name"; do
  if [[ -f "$candidate" ]]; then native_binary="$candidate"; break; fi
done
[[ -n "$native_binary" ]] || { printf '%s\n' 'Native build did not produce the expected whisper CLI' >&2; exit 1; }
cp "$native_binary" "$output/runtime/$native_name"
chmod 755 "$output/runtime/$native_name"
if [[ "$(uname -s)" == Darwin ]]; then
  codesign --force --sign - "$output/runtime/$native_name"
  codesign --verify --strict "$output/runtime/$native_name"
fi
cp "$source_dir/LICENSE" "$output/notices/whisper-LICENSE"
manifest="$output/manifest.json.tmp"
printf '{"asset_id":"whisper-runtime","revision":"%s","source_sha256":"%s","cpu_policy":"portable-static-x64-sse2-arm-native","files":[' "$revision" "$source_sha" > "$manifest"
separator=''
for relative in "runtime/$native_name" notices/whisper-LICENSE; do
  checksum=$(cmake -E sha256sum "$output/$relative"); checksum=${checksum%% *}
  bytes=$(wc -c < "$output/$relative"); bytes=${bytes//[[:space:]]/}
  printf '%s{"path":"%s","size_bytes":%s,"sha256":"%s"}' "$separator" "$relative" "$bytes" "$checksum" >> "$manifest"
  separator=','
done
printf ']}\n' >> "$manifest"
mv "$manifest" "$output/manifest.json"
printf 'Verified native component prepared at %s\n' "$output"
