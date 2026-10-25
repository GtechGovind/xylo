# Reviewed native assets

`workbench-capabilities::assets::AssetManager` provisions app-owned native components and explicit models. Construction and catalog reads start no download, browser, model, or native library. The desktop calls the persistent native helper; it needs no Python or Node runtime. The catalog is compiled into the application. An install request accepts a catalog ID, never a URL, checksum, executable path, or destination.

## Product controls

| Helper command | Arguments | Result |
| --- | --- | --- |
| `helper_asset_catalog` | `{}` | `{platform, assets:[...]}` |
| `helper_asset_install` | `{id}` | Initial `JobStatus`; work continues in the helper |
| `helper_asset_status` | `{job_id?}` | `{jobs:[JobStatus...]}` |
| `helper_asset_cancel` | `{job_id}` | `{job_id,cancellation_requested:true}` |
| `helper_asset_activate` | `{id}` | `{state:"activated",asset_id,config_patch,embedding_changed,language_servers_changed,restart_required:false}` |

Catalog entries contain `id`, `name`, `kind`, `description`, `revision`, `source_url`, `license:{identifier,url,notice}`, `requires`, `hidden`, `size_bytes`, `supported`, `unavailable_reason`, `installed`, `active`, and `source_build`. Displayed size includes selected dependencies. Hidden runtime dependencies are still represented so the UI can explain them. Support means a pinned package and adapter exist for this platform; it does not assert runtime or account acceptance.

`JobStatus` contains `job_id`, `asset_id`, `phase`, `downloaded_bytes`, `total_bytes`, `progress` (0–1), `message`, `updated_at_ms`, and `owner_pid`. Active phases are `queued`, `downloading`, `extracting`, `building`, and `verifying`. Terminal phases are `installed`, `failed`, `cancelled`, and `interrupted`. An interrupted/cancelled installation is resumed by another explicit Install action. Status polls perform no downloads. Install does not activate a model. Use verifies the installed bytes again and applies their owned paths.

The helper's protocol reports background job counts, including a completion update even when the UI stops polling. `HelperClient::pending_count()` includes those jobs to prevent host idle eviction during an installation. Cancellation retains incomplete verified-cache candidates for range resume, stops extraction/build work cooperatively, and never promotes incomplete bytes. A cancelled or failed job does not erase an earlier valid installation or configuration.

## Catalog and primary sources

Each download in `crates/capabilities/src/assets/catalog.json` has an exact HTTPS URL, byte count, and SHA256. GitHub binary hashes come from the publisher's release-asset metadata; model hashes come from the publisher's Hugging Face LFS metadata. Small configuration/license files and the pinned whisper source archive were independently hashed. All archive contents, model cards, notices, and separately fetched license files are retained; `ORIGIN.json` records the reviewed provenance.

| Asset | Pinned source | License / scope |
| --- | --- | --- |
| llama.cpp native server | [b11869](https://github.com/ggml-org/llama.cpp/releases/tag/b11869), commit `d327bf1f0780f87b8c01b367b61853b0c291d7c4` | MIT; Metal macOS packages, portable CPU Linux/Windows packages |
| whisper.cpp native CLI | [b5454](https://github.com/ggml-org/whisper.cpp/releases/tag/b5454), commit `d1be6fde11ac6e0407606b4e42fe72d34add8037` | MIT; native release archives on Linux/Windows; pinned source build on macOS |
| PDFium | [bblanchon/pdfium-binaries chromium/8111](https://github.com/bblanchon/pdfium-binaries/releases/tag/chromium/8111) | PDFium BSD and archive third-party notices; independent distributor, unaffiliated with Google/Foxit |
| ONNX Runtime | [Microsoft v1.31.0](https://github.com/microsoft/onnxruntime/releases/tag/v1.31.0) | MIT and retained third-party notices; CPU package |
| sherpa-onnx native TTS | [v2.0.0](https://github.com/k2-fsa/sherpa-onnx/releases/tag/v2.0.0) | Apache-2.0; shared native runtime |
| Qwen3 0.6B Q8 | [Qwen publisher](https://huggingface.co/Qwen/Qwen3-0.6B-GGUF/tree/23749fefcc72300e3a2ad315e1317431b06b590a) | Apache-2.0; explicit optional 639MB generation model |
| Qwen3 1.7B Q8 | [Qwen publisher](https://huggingface.co/Qwen/Qwen3-1.7B-GGUF/tree/90862c4b9d2787eaed51d12237eafdfe7c5f6077) | Apache-2.0; explicit optional 1.83GB generation model |
| Whisper Tiny English | [whisper.cpp model publisher](https://huggingface.co/ggerganov/whisper.cpp/tree/5359861c739e955e79d9a303bcbc70fb988958b1) | OpenAI MIT weights, approximately 78MB; license text retained from a pinned OpenAI commit |
| BGE small English v1.5 | [BAAI publisher](https://huggingface.co/BAAI/bge-small-en-v1.5/tree/5c38ec7c405ec4b44b94cc5a9bb96e735b38267a) | Publisher declares MIT; 133MB ONNX model, 384 dimensions, CLS pooling |
| LJSpeech VITS int8 voice | [sherpa maintainer's export](https://huggingface.co/csukuangfj/vits-ljs/tree/7ac337c834f318e45a34037cb3371cc3929187ff) | Publisher declares Apache-2.0; original VITS MIT and LJSpeech public-domain provenance retained in the model card; 37MB voice |
| Rust Analyzer | [2026-10-19](https://github.com/rust-lang/rust-analyzer/releases/tag/2026-10-19), commit `5ec20d044dbf04895877a55921ddc0ae6e73e127` | MIT OR Apache-2.0; both original license texts retained |

Models are choices, not benchmark or quality rankings. No generation model is chosen automatically. The catalog deliberately lists small explicit options and continues to support separately configured providers. Other local runtimes can use the provider endpoint flow.

## Installation and resource bounds

Data lives below the canonical application data root:

```
assets/downloads/<sha256>.part     # resumable, never executable
assets/downloads/<sha256>.blob     # complete, hash checked
assets/staging/<asset>-<job>/      # private extraction/build
assets/installed/<id>/<variant-sha256>/
assets/jobs/<job-uuid>.json
assets/locks/<id>.lock
```

Downloads stream and hash bounded chunks instead of buffering models in memory. HTTP transfers require credential-free HTTPS, permit at most five HTTPS redirects, have connection/read/overall deadlines, enforce the pinned byte count, and verify SHA256 before atomic promotion. Range responses must match the existing offset and full pinned size. A server that ignores Range restarts safely. Hash failures never become installed assets. Cross-process file locks and a per-helper installation queue prevent conflicting writes; contended dependency installation returns a retryable error after five seconds.

Archive extraction rejects parent paths, absolute paths, special files and escaping links. Harmless `./` prefixes are normalized. Archive links are copied as regular owned files after bounded internal resolution. Entry/file/expanded-byte budgets bound extraction, and cancellation is checked during extraction and verification. Raw model files can share a verified cache inode to avoid duplicating large weights. Installed-file receipts contain content hashes. Activation rechecks every installed file; corruption invalidates the receipt so Install can repair it. An invalid previous directory is preserved under the owned recovery staging area during repair.

The individual download limit is 16GiB, archive entry limit 2GiB, expanded archive limit 4GiB, and entry count limit 20,000. Disk space is checked before downloading with a 64MiB reserve. There are at most eight active jobs per helper, one executing installation per helper, 64 retained in-memory statuses, and bounded durable history. Native command output is limited; builds have a 30-minute deadline and cancellation terminates their process group/job.

## Native whisper packaging

The pinned whisper release supplies an XCFramework on macOS, which is not the executable needed by this native CLI adapter. The distribution build must prebuild the CLI:

```sh
scripts/native-assets-build.sh
```

This build-time script downloads the exact pinned source, verifies SHA256, uses CMake and the native C/C++ toolchain, builds a portable CLI with static ggml/whisper and embedded Metal support, preserves its license, ad hoc signs the macOS binary, and then writes its hash manifest. It defaults to two build jobs; packagers can supply `NATIVE_BUILD_JOBS`. No Python or Node is required by the script or built helper.

Copy `target/debug/native-assets/` beside ordinary `workbench-helper` binaries. In a macOS application bundle, place it in `Contents/Resources/native-assets/`; `scripts/preview-macos.sh` handles that layout. Manifests must remain outside `Contents/MacOS`, which macOS reserves for signed code. The helper accepts only its own package assets for this adapter and verifies source revision/hash and each bundled file before installing it. Signing or changing a bundled executable after manifest creation invalidates its hash: sign first, regenerate the manifest, and preserve those bytes when bundling. A source-build fallback exists for development installations with CMake/compiler tools. End-user distribution packages must include the prebuilt component to avoid requiring those tools.

## Activation and lazy loading

Activation merges reviewed paths into `capabilities.json` under the configuration lock and managed-chat activity lease. Choosing a new component during an active managed local chat returns an actionable busy error. Unrelated settings and existing active asset directories survive. A runtime-only install can be configured while still needing its explicit model choice; status does not equate an executable path with successful inference.

BGE activation writes `embedding.json` with its model/runtime paths, CLS pooling, 384 dimensions and `cpu_threads:0` (adaptive). The host reloads that configuration without starting the model; the first memory embedding operation creates the native session. Rust Analyzer activation atomically merges `language-servers.json`, replacing only the Rust language override and preserving other languages and settings. Cargo build scripts, procedural macros, and check-on-save are disabled in the installed default. The host can restart an existing Rust language server after `language_servers_changed:true`.

Browser automation detects an existing Google Chrome, Chromium, or Microsoft Edge installation without requiring an executable picker. It uses a fresh isolated profile. If none is installed, status remains configuration needed; no browser is silently downloaded. Provider endpoints and user document/audio selection remain independent product inputs.

## Qualification and remaining limits

The normal suite uses deterministic loopback HTTP fixtures for truncated transfer/range resume, cancellation with resumable partial files, wrong Content-Range, ignored Range, hash mismatch, production HTTPS policy, safe archive extraction, durable installation and installed-byte tampering. Native helper protocol tests verify catalog/status controls without initiating a download. Platform/network tests remain explicitly ignored unless selected.

On the available macOS arm64 machine, the explicit real-package test installed and activated BGE plus Microsoft ONNX Runtime, PDFium, llama.cpp and the source-built/signed whisper CLI. PDFium extracted actual text and rendered a PNG preview in one persistent helper. Repeated PDF calls now reuse the process-global PDFium binding. Native whisper `--help` passed. Rust Analyzer has a separate selected install/version/LSP initialization check. Exact final test results are recorded in the handoff; these checks do not establish packaged GUI or cross-platform acceptance.

Official ONNX Runtime 1.31.0 does not publish an Intel macOS archive. The catalog reports that embedding option unsupported there rather than downloading an incompatible older ABI. Windows/Linux binaries are pinned and have adapters, but were not executed on this Mac. Windows native prerequisites, GPU packages other than macOS Metal, microphone permissions, actual transcription/synthesis, generation with selected weights, voice provenance/third-party redistribution review, and signed/notarized production installers require their own acceptance. No voice or generation weights were downloaded by the native runtime qualification described above.

### Recorded headless acceptance (2026-10-25, macOS arm64)

- `cargo +stable test -p workbench-capabilities -p workbench-helper -p workbench-helper-client`: 30 passed; 2 explicit platform/network checks ignored in the normal run.
- `cargo +stable clippy -p workbench-capabilities -p workbench-helper -p workbench-helper-client --all-targets -- -D warnings -A clippy::large_enum_variant`: passed. The exemption belongs to the existing core enum; the changed packages have no remaining Clippy warnings.
- Explicit helper package test with `WORKBENCH_ASSET_SMOKE_IDS=bge-small-en-v1.5,pdfium-runtime,llama-runtime,whisper-runtime`: passed, including real native PDF extraction and PNG preview. Selected assets and checksums were retained under the smoke data root.
- Explicit helper package test with `WORKBENCH_ASSET_SMOKE_IDS=rust-analyzer`: passed, including the installed server's version and LSP initialize/initialized/shutdown/exit exchange.
- The pinned whisper native source build, ad hoc signature verification, bundled-manifest content verification and native `--help` passed. Native helper/Rust Analyzer processes exited after the checks; no asset test process remained.

To repeat the opt-in network test, set an absolute `WORKBENCH_ASSET_SMOKE_DATA` directory and a comma-separated `WORKBENCH_ASSET_SMOKE_IDS` selection, then run `cargo +stable test -p workbench-helper --test asset_install -- --ignored --nocapture`. Choosing those environment variables is an explicit install request. The normal offline suite never downloads those assets. The Mac was locked during these headless checks; they are not GUI acceptance.
