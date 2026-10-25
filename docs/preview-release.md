# First native preview release

The first release is version **0.1.0**, tagged `v0.1.0` and marked as a GitHub prerelease. The workflow prepares an unpublished draft; a maintainer reviews and publishes it separately. It uses no Apple identity, Windows certificate, paid runner or signing secret, and embeds the disabled updater policy.

| Target | GitHub runner | Delivered format |
| --- | --- | --- |
| macOS ARM64 | `macos-15` | `.dmg`, `.app.tar.gz` |
| macOS Intel x64 | `macos-15-intel` | `.dmg`, `.app.tar.gz` |
| Linux x64 | `ubuntu-24.04` | `.AppImage`, `.deb` |
| Linux ARM64 | `ubuntu-24.04-arm` | `.AppImage`, `.deb` |
| Windows x64 | `windows-2022` | per-user NSIS `.exe` |
| Windows ARM64 | `windows-11-arm` | per-user NSIS `.exe` |

GitHub documents these as [standard public runners](https://docs.github.com/en/actions/reference/runners/github-hosted-runners), which are free for public repositories. Both workflows refuse private repositories rather than silently consume paid minutes. The pinned [cargo-packager 0.11.8 NSIS source](https://github.com/crabnebula-dev/cargo-packager/blob/cargo-packager-v0.11.8/crates/packager/src/package/nsis/mod.rs) maps `aarch64` to ARM64 installers. Source support and runner availability are separate from successful native execution.

## Native qualification

`.github/workflows/native-release.yml` runs on a pull request, manually, or as a reusable workflow. Each native job installs pinned Rust/native headers, runs release-script regressions, builds the three debug executables, runs workspace library tests and strict Clippy, and validates the Rust plugin SDK. Windows executes the production plugin, capture-storage and project-reader filesystem fixtures before compiling the desktop graph. The smaller `windows-filesystem.yml` workflow runs those fixtures on both native architectures for relevant pushes and pull requests; it does not qualify the whole application.

Jobs build pinned whisper.cpp without model weights, discard the tested debug graph to conserve hosted disk, then compile the optimized application. Build-time debug symbols and concurrency are bounded; the checked-in optimized profile retains thin LTO, symbol stripping and its code-generation settings. macOS ARM64 uses one build job on its 7 GB runner. Linux reclaims unused mobile/.NET/Haskell SDKs only inside the disposable hosted VM. Standard-runner memory/disk limits can still cause a qualification failure; targets are never silently omitted.

`scripts/release.py --mode preview` requires clean committed source and complete dependency license text coverage, using the strict production validator. Local development mode permits an honest incomplete inventory, but cannot pass the publishing gate. Packages contain notices, a CycloneDX SBOM, source/SDK provenance and pinned native license texts. Debian requirements combine explicit runtime backends with `dpkg-shlibdeps` results from all four actual executables.

`scripts/release_smoke.py` performs these native checks:

- Verify packaged dependency texts and native asset hashes; inspect all three application executables and whisper's actual PE, ELF or Mach-O architecture.
- Execute all three packaged `--version` commands and whisper's help command.
- Run a per-user NSIS install on Windows; extract the native AppImage on Linux; verify the ad hoc macOS app signature.
- On Windows, bind the retained and packaged normal/delayed DLL import reports to all four installed executable hashes and reject unstaged VC/OpenMP runtimes.
- On Linux, inspect the actual Debian control fields, including maintainer and architecture, and launch the extracted AppImage through its `AppRun` entry point.
- Verify Linux distro-library notices against the actual bundled ELF closure, and inspect the copied native notices inside the Debian archive. AppRun and the embedded runtime must match their separately reviewed upstream identities and texts.
- Run correlated helper status and an actual DOCX export, inspect the resulting ZIP, and shut down cleanly.
- Observe the worker's `Ready` event and clean stdin shutdown.
- Launch the desktop with an explicit temporary `--data-dir`, observe profile creation and eight seconds of process liveness, then terminate only that process tree. Linux uses Xvfb/software Vulkan.

This proves package/process behavior. Profile creation and liveness do not establish full Host initialization, visual layout, accessibility, GPU performance, microphone permissions, real accounts or provider behavior. AppImage FUSE mounting and Debian installation are not exercised. No live providers, remote accounts, microphone recording or model downloads occur. Native UI/Host fixtures recorded elsewhere remain separate evidence.

Linux native notices complement the Cargo inventory. Before packaging, `release_linux_notices.py` uses actual `ldd`, `readelf` and `dpkg-query` results from the three app executables and Whisper. It preserves each exact installed package copyright file and its referenced `/usr/share/common-licenses` texts. After extraction, every bundled distro ELF must map unambiguously by machine, SONAME and GNU build ID. Original and packaged SHA256 values are recorded separately: RPATH rewriting changes bytes, so build identity is explicitly not a byte-equivalence assertion. Both packages must contain the same hash-checked native inventories and texts. The external validation report is bound to the exact AppImage, Debian archive, installed executable identities and qualification proof.

AppRun and the outer runtime have a separate committed upstream catalogue under `packaging/licenses/linux-infrastructure`. The runtime is downloaded from its explicit official URL into a hash-checked build cache, supplied through `LDAI_RUNTIME_FILE`, and checked again after packaging. The only permitted runtime change is the pinned appimagetool's 16-byte `.digest_md5` section; an unknown launcher/runtime or altered notice fails the gate. The shipped catalogue and README retain source/build references and the static-library corresponding-source/relink qualification limits; text preservation is not legal approval. The early Linux-only native-tool fixture exercises real distro tools with tiny synthetic executables before compiling the desktop. It does not replace the actual package smoke.

The static x64 whisper executable uses an explicit portable SSE2 CPU baseline. Upstream's `GGML_NATIVE=OFF` alone still enables AVX-family defaults, which are disabled here to avoid an undocumented CPU requirement. ARM NEON/Metal remain native. Multi-variant shared backends need separate trusted-loader/packaging qualification; optimized x64 speech performance is unqualified. This policy does not change Rust release optimization or external inference providers.

## Draft workflow

The native jobs cache only the installed, pinned packaging tool, using separate keys for each runner, target architecture and Rust version. Its version is checked before use. Application builds, tests and package qualification still run for the exact candidate commit.

`.github/workflows/preview-release.yml` is manual only. It checks the exact workspace version, calls all six native jobs, downloads that run's artifacts and invokes the release gate. To prepare the same files locally without any GitHub write:

```sh
python3 scripts/release_preview.py --inputs target/native-results \
  --output target/preview-release --commit EXACT_40_CHARACTER_COMMIT --tag v0.1.0
```

All six source/lock/proof/inventory/artifact identities are validated before staging files. The script refuses missing targets and existing staging directories, and writes uniquely named binaries, metadata ZIPs, `release.json`, release notes and `SHA256SUMS`. The explicit `--create-draft --repository OWNER/REPOSITORY` option verifies a public destination and creates an unpublished prerelease using `gh`. It refuses an existing release and never moves a tag. Only the final draft job receives `contents: write`; native jobs use read-only permission, no persisted checkout credential and no signing secrets. No push/tag trigger publishes anything.

Review the full draft, source SHA, successful six-target proofs and limitations before publishing. Preserve failed runs as failures. If an upload fails after creating a draft, inspect that draft explicitly; do not assume publication or overwrite it blindly.

## Remaining acceptance

macOS is ad hoc signed, not Developer ID signed or notarized. Windows has no Authenticode certificate. OS trust prompts may appear. Signed production needs additional entitlement/library-loading and account qualification, described in [distribution](distribution.md). Updates stay disabled; model weights are chosen explicitly after installation. Bundled ONNX runtime setup is currently unavailable on macOS Intel, where full-text memory search remains available. Preview preparation does not establish full cross-platform performance, device/provider acceptance or production readiness.
