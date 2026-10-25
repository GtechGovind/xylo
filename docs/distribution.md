# Native distribution and authenticated updates

The application uses cargo-packager **0.11.8** for native installers and the compatible cargo-packager-updater **0.2.3** types/Windows/Linux installation path. `workbench-distribution` adds signed release metadata, bounded downloads, a durable rollback journal, and a transactional macOS replacement. The native interface and built-in execution require no Python or Node runtime; browser automation uses native Chromium. Python 3.11+ is used only by these release build scripts.

The checked-in update policy is deliberately disabled. No maintainer private key, public key, release domain, Apple identity, or Windows certificate has been invented. A development preview and the tests below do not qualify a production release. [First native preview release](preview-release.md) describes the six native targets, strict license-complete unsigned mode and separate manual draft workflow.

## Build and package

Use a clean checkout on the target OS, with the pinned Rust **1.98.0** toolchain and checked-in `Cargo.lock`. macOS has a 15.0 package baseline; Windows uses Windows 10+ with MSVC/Windows SDK; the Linux installer baseline is Ubuntu 24.04 with a working Vulkan driver and graphical Wayland/X11 session. See [GPUI Kit's native requirements](https://gpui-kit.com/docs/installation/) and [cargo-packager formats](https://docs.crabnebula.dev/packager/).

Install CMake, a C/C++ compiler, platform SDKs, Python 3.11+, and the packaging tool. On macOS, full Xcode with the Metal toolchain is required for GPUI shaders. On Ubuntu install the development headers listed in `.github/workflows/native-release.yml`, including ALSA and pkg-config for native audio. Windows needs Visual Studio's Desktop development with C++ workload, LLVM and NSIS; run from a developer shell with CMake, Git Bash, and the Windows SDK available. ARM64 speech builds require the ClangCL toolset.

```sh
rustup toolchain install 1.98.0 --profile minimal --component rustfmt --component clippy
cargo install cargo-packager --version 0.11.8 --locked --root .release-tools

# Inspect the exact configuration without compiling, signing, or publishing:
python3 scripts/release.py --target aarch64-apple-darwin --plan

# Build only pinned native runtime code, without downloading model weights:
bash scripts/native-assets-build.sh target/aarch64-apple-darwin/release/native-assets/whisper-runtime

# Point WORKBENCH_PACKAGER at .release-tools/bin/cargo-packager (Windows: .exe).
WORKBENCH_PACKAGER="$PWD/.release-tools/bin/cargo-packager" \
  python3 scripts/release.py --target aarch64-apple-darwin --mode development
```

The script builds all three native binaries with `--locked --release`, collects dependency notices/SBOM/provenance, and produces a `.app`, `.dmg`, and updater `.app.tar.gz` on macOS; `.AppImage` and `.deb` on Linux; or a per-user NSIS `.exe` on Windows. Development macOS bundles receive an ad hoc signature, which is not a trusted Developer ID signature. Model weights are selected and installed explicitly inside the product; release packaging never copies the user's app-data models or credentials.

The product is Xylo: macOS bundles are `Xylo.app`, with artifacts named `Xylo-VERSION-PLATFORM.dmg` and `Xylo-VERSION-PLATFORM.app.tar.gz`. The production bundle/application ID is `com.qurkos.xylo`; `scripts/preview-macos.sh` creates `target/debug/Xylo.app` with ID `com.qurkos.xylo.preview`. Executable names and `WORKBENCH_*` build variables retain their existing names. [Identity and profile compatibility](branding.md) documents new installation paths, reuse of existing profiles and stable credential references. Historical validation records keep the product and bundle identities recorded when they were produced.

The original Xylo artwork is recorded in `packaging/branding/brand.json`. Native icon derivatives and their source/output hashes are in `packaging/icons/manifest.json`; packaging uses the checked-in PNG, ICO and ICNS files. To regenerate them after an intentional artwork change, explicitly build the native converter and run the build-time driver:

```sh
cargo build --locked -p agent-workbench --example prepare_brand_icons
python3 scripts/release_icon.py --tool target/debug/examples/prepare_brand_icons
```

On Windows, append `.exe` to the converter path. The driver does not build the converter automatically. Python release tests validate the checked-in icon bytes and metadata without launching Cargo. The shipped application does not use Python for image conversion.

Native runtime assets are mapped to `Contents/Resources/native-assets` on macOS, adjacent `native-assets` on Windows, and `usr/bin/native-assets` beside the helper in Linux packages. Do not move macOS assets into `Contents/MacOS`. The asset manifest binds the final native runtime bytes. macOS signing therefore signs nested binaries first, regenerates their bundled asset digests, then signs the outer app. Windows signs native binaries before bundling and regenerates affected asset digests. Cross-platform bundle import remains subject to the capability subsystem's pinned-source checks; a package build alone does not prove native speech works.

Windows release builds request `+crt-static`; the native speech build selects the static MSVC runtime and disables OpenMP. An LLVM `llvm-readobj --coff-imports` audit inspects normal and delayed imports of all four final executable inputs before packaging. `WORKBENCH_LLVM_READOBJ` may select an installed tool; otherwise the script searches PATH and the standard LLVM installation directory. The retained report records tool/input hashes and rejects unstaged VC runtime, C++ runtime and OpenMP DLL imports. Native smoke binds that report to the installed executables. It does not qualify dynamically loaded model or speech packages.

The pinned Linux packager scans top-level executables for dependencies. An additional Whisper dependency seed lets that scan cover the nested speech executable while preserving its original manifest-bound bytes. Linux smoke exercises the extracted `AppRun` entry point and inspects Debian control metadata; it does not install the Debian package or test FUSE mounting.

Production mode requires a clean committed source tree, fresh build, enabled maintainer policy, complete dependency license-text review, and existing signing configuration. It invokes native signing/notarization and never creates accounts or publishes releases.

Windows signing is application-owned: sign inputs, inspect their imports, build the installer without implicit packager re-signing, then sign and verify the finished installer. The embedded NSIS uninstaller currently remains unsigned; its signing policy and actual production installation/uninstallation acceptance are outstanding. This does not change the unsigned preview policy.

Required configuration:

| Platform | Existing maintainer configuration |
| --- | --- |
| All | `--policy /private/release-policy.json`, `CARGO_PACKAGER_SIGN_PRIVATE_KEY` (secret-key file or supported secret value), optional `CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD` |
| macOS | `WORKBENCH_MACOS_SIGNING_IDENTITY`, `WORKBENCH_NOTARY_KEYCHAIN_PROFILE`, matching `macos_team_id` in policy; a usable Developer ID identity and saved notary keychain profile must already exist |
| Windows | `WORKBENCH_WINDOWS_CERTIFICATE_THUMBPRINT`, `WORKBENCH_WINDOWS_TIMESTAMP_URL` (HTTPS), available private certificate in Windows certificate store, `signtool` |
| Linux | Native build dependencies; release authenticity is supplied by the signed manifest/artifact; Debian package-manager update policy remains separate |

The private release policy has the same schema as `packaging/update-policy.json`. Set `enabled` to true, use the final application ID/channel, credential-free HTTPS manifest/signature URLs, an explicit origin allowlist, and `public_key` equal to **base64 of the complete minisign public-key file** produced by cargo-packager. Do not embed a private key. Keep `max_artifact_bytes` bounded; the default is 512 MiB. If the final app exceeds it, review and explicitly increase the build policy (hard maximum 2 GiB). macOS policy must pin the maintainer's actual 10-character Apple Team ID. The release version comes from workspace Cargo metadata.

## Signed release files

Finalize OS signatures and notarization before signing the updater artifact. The production macOS script notarizes/staples the application and DMG. Its normalized tar archive contains the already stapled app. Never mutate the payload after producing its detached signature.

```sh
# Example structure only: use your actual policy, artifact, URL, notes and sequence.
python3 scripts/release_manifest.py \
  --policy /private/release-policy.json \
  --artifact macos-aarch64 app /release/Xylo-VERSION-macos-aarch64.app.tar.gz \
  --output /release/signed \
  --base-url https://YOUR-RELEASE-HOST/releases/VERSION \
  --sequence YOUR_MONOTONIC_SEQUENCE \
  --notes /release/notes.md
```

Repeat `--artifact PLATFORM FORMAT PATH` to combine qualified platform artifacts. Platform IDs follow the actual updater library: `macos-aarch64`, `macos-x86_64`, `linux-x86_64`, `linux-aarch64`, `windows-x86_64`, `windows-aarch64`. Formats are `app`, `appimage`, and `nsis`. Linux updater bytes are the **raw `.AppImage`**, not an enclosing tar archive. The producer emits immutable `release-VERSION-SEQUENCE.json` and `.sig` files, refuses overwrites, signs exact UTF-8 bytes, and runs the application's offline verifier for every artifact. Key/configuration mismatches fail before handoff. It does not upload files.

Metadata binds application ID, channel, semver version, positive monotonic sequence, publication/expiry time, release notes, and each exact platform's URL, format, SHA-256, size, and artifact minisign signature. Expiry defaults to 14 days and is capped at 31. Keep immutable release URLs. A feed operator may copy the exact signed bytes to the configured channel endpoint with an atomic manifest/signature promotion; partial promotion fails verification. Do not reserialize signed JSON. Do not claim a complete TUF implementation: this design has one pinned signing key and does not provide delegated roles, threshold signing, or a remote key-rotation protocol. A compromised signing key needs an independently trusted recovery release.

## Host integration

`workbench_runtime::updates::Service` wraps the blocking updater for the host. It exposes `new(data_dir, executable)`, `status()`, and async `check`, `stage`, `install` accepting a `CancellationToken`. Initialize `new` off UI. Its memory-only status contains a monotonic `sequence` observation number, `enabled`, `phase`, safe `available` metadata, latest bounded `progress`, safe `message`, `restart_required`, and `can_check`/`can_download`/`can_install`. The UI rejects older sequence observations. Action exclusion survives an aborted waiting task until its blocking job actually stops; dropping that waiter cancels ongoing connection/download work. Cancellation/failure retain a previous authenticated offer/stage for retry, and pending OS-install outcomes remain journalled. The service does not stop tasks itself: host-level admission/quiescence must precede the explicit install call.

Run all `Updater` blocking methods through `tokio::task::spawn_blocking`; they must never run on GPUI's UI thread or directly inside an async task. Download entrypoints use a private async reactor so cancellation interrupts connection/TLS/body waits. The product receives only sanitized summaries/progress, while the host retains opaque authenticated objects:

```rust,ignore
let updater = Updater::from_embedded(data_dir.join("updates"), current_executable)?;
// None means updates are unavailable in this build; show that honestly.
let offer = updater.check(&cancellation)?;                 // explicit Check
let staged = updater.stage(&offer, &cancellation, progress)?; // explicit Download
// After the user's Install action: save edits and quiesce workers/helpers.
let outcome = updater.install(staged, &cancellation)?;     // explicit Install
```

`VerifiedUpdate::summary()` and `StagedUpdate::summary()` expose version, notes, size, and platform. `UpdateProgress` has `phase` (`downloading`, `verifying`, `ready`), downloaded and total bytes. `product_error` supplies safe product messages without remote URLs, trust details, or raw logs. `verify_artifact` and `workbench-release-verify` are offline build validation helpers; they never install.

Keep the host alive while installation runs, so it can recover product controls on failure. On macOS a successful replacement requires explicit relaunch afterward. Windows' upstream installer handoff can terminate the current process; install intent is persisted first. Linux resolves `APPIMAGE` rather than writing into its mounted image; `.deb` installations must use their package manager. Never run an updater from an arbitrary development executable and claim a bundle installation succeeded.

Verification rejects unsigned/tampered metadata before parsing, unknown fields, wrong identity/channel/platform/format, stable-channel prereleases, expired/future metadata, insecure/untrusted redirect origins, excessive sizes, bad SHA-256, and bad artifact signatures. Downloads are streamed to private disk and verified again immediately before install. Metadata/signature limits are 1 MiB/16 KiB. Modern prehashed minisign signatures are required; legacy verification is disabled. No update check advances the installed rollback floor. The highest accepted version/sequence/hash are atomically persisted only after validated installation, or reconciliation with a newly running version after a crash/Windows installer handoff. Pending intent remains on uncertain final durability errors. Corrupt journals fail closed instead of resetting rollback protection.

The macOS path intentionally avoids the upstream administrator/AppleScript deletion fallback. It preflights one complete `.app`, rejects traversal, escaping symlinks, hardlinks, special files/permissions, duplicate entries, more than 100,000 entries, or more than 2 GiB expanded data. Before touching the old bundle it verifies codesign, the pinned Team ID, bundle ID/version, and Gatekeeper. Extraction and a kernel-level atomic old/new exchange happen on the installed app's volume, with no interval where the installed path is absent. Filesystems without exchange support fail safely instead of falling back to two renames. Failure to validate leaves the old app in place; inability to write the install folder requests manual installation instead of privilege escalation. Local administrators controlling the app/data directory remain outside the remote-update threat model.

## Reproducibility and inventory

`release_metadata.py` emits a CycloneDX 1.6 SBOM for the platform-filtered normal/build Cargo dependency closure of all three delivered binaries, copies available license/notice texts, and records missing texts for review. It includes registry archive hashes from `Cargo.lock`, not claims of independently audited source or executable hashes. Build provenance records source commit/dirty state, toolchain, target, lock digest, SDK/OS and pinned packager. Bundled native asset manifests record pinned source revisions/hashes and final file digests. `SHA256SUMS` covers top-level output files; signed release metadata separately authenticates update artifacts.

The build pins dependencies/toolchain and normalizes `SOURCE_DATE_EPOCH`, root-path remapping, archive order, ownership and timestamps. This establishes reproducible inputs and deterministic archive packaging for identical payload bytes. It does **not** establish bit-for-bit reproducibility of native compiler output or timestamped OS signatures/notarized DMGs. Compare payload hashes from two isolated builds with matching SDK/compiler/native inputs before making that claim. Keep signing credentials and app-data directories out of source archives, provenance, logs and installers.

## Evidence and remaining acceptance

Local Rust tests prove signature/tamper rejection, identity/platform/expiry/rollback rules, cancelled TLS waits, archive safety, and macOS fixture transactional replacement. Python tests prove deterministic packaging, immutable outputs, native manifest rehashing, URL policy and platform mappings. Fixture keys exist only in test memory; they are not production identities. The native GitHub workflow is a **qualification** matrix using disabled feeds, no signing secrets and read-only repository permissions. The separate manual preview workflow can create an unpublished draft only after all six jobs and strict inventories pass. Hosted image labels move; retain actual SDK/compiler provenance with artifacts. Windows/Linux installer execution and a Developer ID/Gatekeeper live update remain unverified until run on clean machines.

Follow [release-checklist.md](release-checklist.md) before publishing. Authoritative API references: [updater docs](https://docs.crabnebula.dev/packager/updater/), [updater 0.2.3 source](https://github.com/crabnebula-dev/cargo-packager/blob/cargo-packager-updater-v0.2.3/crates/updater/src/lib.rs), [packager 0.11.8 configuration](https://github.com/crabnebula-dev/cargo-packager/blob/cargo-packager-v0.11.8/crates/packager/src/config/mod.rs), [packager signing implementation](https://github.com/crabnebula-dev/cargo-packager/blob/cargo-packager-v0.11.8/crates/packager/src/sign.rs), [streaming signature verification](https://docs.rs/minisign-verify/0.2.4/minisign_verify/struct.StreamVerifier.html), and [Apple notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow).
