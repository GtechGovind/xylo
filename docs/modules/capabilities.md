# Native capability services (`xylo-capabilities`)

## Purpose and boundaries

`xylo-capabilities` hosts the native services the agent can call as tools: the bundled Obscura browser, desktop capture and input, DOCX/XLSX/PDF documents, SSH and container environments, Wasmtime plugins, local inference and speech, reviewed asset downloads, and the scheduler tools. Its crate docs say: "Native capability services, isolated from the agent implementation."

**Constructing a service is cheap.** Browsers, model servers and native libraries start only when an operation is requested. The render-enabled Obscura v0.2.6 component is source-built and packaged as the sibling `xylo-obscura` executable (`.exe` on Windows). It is not an optional downloaded asset. Optional assets (pdfium, llama, whisper, sherpa) are local paths supplied by the asset manager or an explicit import; construction never downloads or loads them.

Dependencies (`Cargo.toml`): `xylo-core`, `xylo-resources`, `xylo-scheduler`, plus `pdfium-render`, `calamine`, `rust_xlsxwriter`, `zip`, `quick-xml`, `xcap`, `enigo`, `wasmtime` 50, `cpal`/`rodio`/`hound`/`rubato`, `reqwest` (rustls), `sysinfo`.

**The crate does not decide permissions.** `CapabilityService::execute` "must be called after the application's tool permission decision". Approval lives in the engine and runtime: see [engine-tools](engine-tools.md) and [runtime](runtime.md).

## Module map

| Path under `crates/capabilities/src` | Purpose |
| --- | --- |
| `lib.rs` | `CapabilityConfig`, `CapabilityService`, dispatch, output budget, shared helpers |
| `browser.rs` | Bundled Obscura over private local IPC (7 tools) |
| `desktop.rs` | `xcap` screenshots and `enigo` input (4 tools) |
| `documents.rs`, `documents/` | DOCX, XLSX, XLS, PDF open/edit/export with transactional saves |
| `environments.rs`, `environments/` | SSH and container execution, capture, archives, devcontainers, storage |
| `plugins.rs`, `plugins/` | Wasmtime plugin catalog, security, runtime |
| `models.rs`, `models/` | Local llama inference and speech helper processes |
| `speech.rs`, `audio/` | Speech helper protocol, microphone capture, playback, conversion |
| `assets/` | Reviewed asset catalog, download, archive extraction, activation |
| `managed_endpoints.rs` | Managed local endpoint records |
| `process.rs` | Executable validation and child process helpers |
| `input.rs`, `input/` | Confined input-file opening (`InputRoot`, `open_input`) |
| `windows_nt.rs` | Windows-only native filesystem helpers |
| `tests/`, `*/tests.rs` | Unit tests |

`examples/` holds `measure_documents.rs`, `measure_plugins.rs` and `validate_plugin_sdk.rs`; `validation/windows-read` is a separate validation crate.

## Service core (`lib.rs`)

### `CapabilityConfig`

Fields: `workspace`, `artifacts`, `management_root`, `browser_allow_private_network` (default `false`), `pdfium_library`, `llama`, `whisper`, `sherpa`, `helper_idle_seconds` (default 60), `operation_timeout_seconds` (default 120), `max_output_bytes` (default 1 MiB). `management_root` holds application-owned catalogs and lifecycle receipts; the process host replaces it and imported settings cannot redirect it.

`CapabilityService::new` **fails** unless:

- the workspace canonicalizes to a directory;
- `artifacts` is absolute (it is created and canonicalized);
- `operation_timeout_seconds` is 1..=3600;
- `max_output_bytes` is 1 KiB..=16 MiB;
- `management_root` is absolute, a real (non-symlink) directory (created `0o700` on Unix) and **disjoint** from the artifacts directory in both directions.

It also creates `xylo_scheduler::Scheduler` over `<artifacts>/schedules.json`.

### Dispatch

`tool_definitions()` concatenates browser, document, model, desktop, plugin, environment and scheduler definitions. `execute()` routes by name prefix: `browser_`, `document_`, `local_inference_`/`speech_`, `desktop_`, `plugin_`, `environment_`, `schedule_`; anything else fails with `Unknown capability tool`. A cancelled token fails before dispatch with `Operation cancelled`.

**Admission.** When `management_root` is set and `xylo_resources::capability_demand(name)` returns a demand, a shared lease is acquired under `<management_root>/admission` first. Demands (1.0 CPU share each): `document_open`, `document_save_copy`, `document_export_docx`, `document_export_xlsx` 384 MiB; `helper_plugin_inspect`, `helper_plugin_install`, `plugin_run_installed` 512 MiB. See [resources](resources.md).

**Output budget.** A summary larger than `max_output_bytes` is cut at a UTF-8 boundary and gets `[Capability response truncated at configured byte budget]` appended. **`plugin_*` and `environment_*` responses are never truncated; they fail** with `Structured capability response exceeds the configured byte budget`.

Other entry points: `approval_presentation` (only `plugin_run_installed` returns reviewed package identity and catalog grants), `execute_control` (names `helper_plugin_*` and `helper_environment_*` only; the helper's control channel, absent from advertised tools), `recover_execution` (only `environment_execute`; reconciles from a native receipt and never invents success), `status`, `shutdown`, `shutdown_helper_sessions` (closes only the browser), `prepare_configuration_change` (takes the idle model lease, stops models and browser; busy chats fail before any stop).

### Shared helpers

`definition()` builds a `ToolDefinition` with `additionalProperties:false`; its `writes` flag is what permission policy reads. `output_path` requires a name of at most 120 alphanumeric, `-` or `_` characters and **refuses to overwrite** an existing artifact. `asset_status` reports `configured_unverified`, `asset_missing` or `configuration_needed`.

## Browser (`browser.rs`)

One lazily launched, **isolated** Obscura session shared behind a `Mutex`. Xylo owns the source-built, render-enabled v0.2.6 component, packaged as `xylo-obscura` beside the host executable (`xylo-obscura.exe` on Windows). Communication uses private local IPC (authenticated, dynamically allocated loopback CDP), not a user-configured browser URL, cloud service, MCP connection or public CDP listener. There is no configurable browser path, PATH discovery, installed Chrome/Chromium/Edge fallback or attachment to a personal profile. Missing packaged runtime is an installation/build error, not a request to configure another browser.

`browser_allow_private_network` defaults to `false`: loopback and private-network destinations are blocked. Setting it to `true` explicitly permits broad private-network access, including local development sites; it is not a per-host allowlist or a sandbox grant. Ordinary tool permissions still apply.

| Tool | `writes` | Notes |
| --- | --- | --- |
| `browser_navigate` | yes | `url`; HTTP(S) with a host only, **no URL credentials** |
| `browser_snapshot` | no | Accessibility tree; `max_nodes` default 300, clamped 1..=2000; reports `total_nodes`, `truncated` |
| `browser_click` | yes | CSS `selector`; DOM click dispatch, not a physical pointer click |
| `browser_type` | yes | Focused editable input/textarea `selector`, `text` up to 100,000 bytes |
| `browser_wait` | no | Polls the selector every 100 ms until the operation timeout |
| `browser_screenshot` | no | Viewport-only PNG artifact `browser-<uuid>.png` |
| `browser_close` | yes | Closes the session and deletes the profile |

Exactly seven browser tools are advertised. Downloads are unsupported: legacy `browser_download` calls fail before launching the runtime. There is no substitute unauthenticated download transport.

**Failure behavior.** Each call races cancellation and `operation_timeout_seconds` (`Browser operation cancelled` or `timed out`). **An interrupted or failed in-flight operation drops its owned browser session** instead of replaying an action on an uncertain page. Cancellation/timeout while waiting for the session lock returns without destroying another request's healthy session; legacy-download rejection also leaves a healthy session intact. An idle session closes after `helper_idle_seconds` clamped to 5..=3600 s (checked every at most 10 s). Status distinguishes a running session from a missing bundled runtime; browser readiness never depends on a user-configured executable.

## Desktop (`desktop.rs`)

Runs in `spawn_blocking`. OS permissions are never inferred; `availability()` reports `permission_check_on_use` with the `xcap` capture and `enigo` input backends.

| Tool | `writes` | Behavior |
| --- | --- | --- |
| `desktop_displays` | no | Lists id, name, width, height |
| `desktop_screenshot` | yes | Optional `display_id`; PNG `display-<uuid>.png` in artifacts; fails above 16384 px per side or if no display matches |
| `desktop_click` | yes | Integer `x`, `y` absolute, left click |
| `desktop_type` | yes | `text` at most 32768 bytes |

On Linux with `WAYLAND_DISPLAY` set, click and type **fail** (`Global input is unavailable in this Wayland backend`). Missing macOS Screen Recording or Accessibility permission surfaces as the backend error.

## Process and input confinement (`process.rs`, `input.rs`)

**`process.rs`** runs native helpers (ssh, docker/podman, llama-server, whisper, cmake) with no shell. A helper starts from an **empty environment** plus an allowlist (`PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `TMPDIR`, `LC_*`, plus Windows system variables) and the variables its `ProcessSpec.env` adds; provider API keys are never inherited. `inherit()` copies named variables such as `SSH_AUTH_SOCK`. `validate_executable` requires an absolute path to an executable regular file (canonicalized). `ProcessSpec` is rejected unless the cwd exists, the timeout is 1 s..=3600 s, `output_limit` is 1..=16 MiB and no argument contains NUL. Children use null stdin, piped output, `kill_on_drop`, and the whole process tree is killed on `stop()` (5 s wait). `run()` stops the process on cancellation (`Native command cancelled`) or timeout (`Native command exceeded N seconds`). On Windows a job object isolates the process.

**`input.rs`** `open_input` opens a tool input once, race-free: relative paths resolve under the workspace; absolute ones must lie under artifacts (checked first) or the workspace. Path at most 4096 bytes and 256 components; no `..`; each component opened `O_NOFOLLOW`-style relative to its parent; symlinks are followed only while they stay under the root, **at most 16 links**; Xylo's management directory is refused by identity; the target must be a regular file with a single hard link. Platforms other than Unix and Windows refuse every input.

## Documents (`documents.rs`, `documents/`)

Work runs on `xylo_resources::spawn_blocking`; cancellation or the operation timeout cancels the token and the work stops at its next check (`Document operation cancelled`). Every tool except `document_save_copy` and `document_save_status` first takes a file lock `<artifacts>/.document-admission.lock`, waiting up to **30 s** (`Another native document is still being processed; retry shortly`). Formulas are never evaluated.

| Tool | `writes` | Behavior and limits |
| --- | --- | --- |
| `document_read` | no | Input at most **64 MiB**. PDF (PDFium; `page`, `max_pages` default 10 clamp 1..=20, 200,000 chars per page, optional `password`), `xlsx`/`xlsm`/`xlsb`/`xls`/`ods` (`max_rows` default 200 clamp 1..=2000, `sheet`; at most 128 columns, 1024 sheets listed), `docx` text (`word/document.xml` read up to 8 MiB), text types (`txt md csv json html xml log yaml yml`, cut to `max_output_bytes`, with `truncated`) |
| `document_pdf_preview` | no | PNG `pdf-<uuid>-p<N>.png`; `width` default 1280 clamp 64..=2048, max height 2048 |
| `document_open` | no | Revision-bound page of UTF-8 text (at most 512 KiB, no NUL), DOCX paragraphs or XLSX typed cells; `limit` max 200 |
| `document_save_copy` | yes | Typed edits to a **new copy**; see below |
| `document_save_status` | no | Reconciles a save by `operation_id` without repeating it |
| `document_export_xlsx` | yes | At most 10,000 rows, 128 columns, 32,767 chars per cell; strings stay text, so `=...` is not a formula; default sheet `Sheet1` |
| `document_export_docx` | yes | At most 10,000 paragraphs, args at most 8 MiB, at most 100 HTTP(S) `sources` links |

Office archives are preflighted before decompression: at most **10,000 entries**, **32 MiB** per entry and **128 MiB** total. Legacy `.xls` is read by a bounded native BIFF8 reader (`documents/xls.rs`); encrypted or non-BIFF8 files are rejected with a message to save as `.xlsx`. ODS opens are bounded by a 2,000,000-cell budget (`workbook.rs`). PDF needs `config.pdfium_library` (absolute, existing); **PDFium binds once per process, and a different library path fails** until the helper restarts.

### Transactional copy saves (`editing.rs`, `transaction.rs`)

Editable kinds are DOCX, XLSX and UTF-8 text; other Office formats are extract-only. A save needs a UUID `operation_id`, the `source` revision (SHA-256 hex), a copy `name` (at most 180 chars, no folders or reserved names, same extension as the source) and 1..=4096 edits within 1 MiB (args at most 2 MiB; source at most 64 MiB; saved output at most 128 MiB). Edit kinds: `text` (at most 512 KiB), `paragraph` (at most 65,536), `cell` (address `A1` style; values `blank`, `text`, `number` (decimal string), `boolean`, `formula` (starts `=`, at most 8193), `error`, `date_time`).

**Guarantees:** the copy is staged in a `.document-*.partial` file, synced and published without overwriting (`persist_noclobber`); the original and unrelated Office parts are preserved. A durable receipt `<artifacts>/.document-operations/<uuid>.json` (under 32 KiB, with a request digest) makes retries idempotent; **reusing an ID with different source, edits or name fails**. A stale revision is refused. States (`DocumentSaveState`): `Prepared`, `Committed`, `Cancelled`, `Failed`, `OutcomeUnknown`. `document_save_status` on an ID with no published copy **cancels it** so a delayed request cannot publish later; an unreconcilable output reports `OutcomeUnknown` and asks to keep the ID.

## Environments (`environments.rs`, `environments/`)

Saved **SSH** and **container** (docker/podman) execution targets (types in [core](core.md), `xylo-core::environments`). Lifecycle is split by trust:

- **Agent tools** (`tool_definitions`): `environment_list` (no connect; shows approval-binding revisions), `environment_status` (read-only probe) and `environment_execute` (`writes`). Any other `environment_*` name fails with `lifecycle management is available only in the application`.
- **Application control** (`execute_control`, `helper_environment_*`, never advertised): `clients`, `list` (adds devcontainer offer), `devcontainer_create`, `status`, `save`, `operation`, `operations`, `maintenance`, `resolve`, `connect`, `disconnect`, `create`, `start`, `stop`, `remove_container`, `remove`. The helper process serves these; see [helper](helper.md).

Requests larger than 128 KiB are refused.

### Profiles (`normalize`)

Name 1..=160 bytes without NUL/CR/LF; serialized profile at most 32 KiB; at most 128 profiles. SSH: absolute validated client, host at most 253 bytes of `[A-Za-z0-9.:-]` not starting with `-`, user token at most 64, non-zero port, an absolute existing `known_hosts` file and optional identity file (each 1 byte..=4 MiB, UTF-8 path without NUL/CR/LF/`%`; **the private key must not be group/other-accessible** on Unix). Container: validated client; existing container name/ID (at most 128) or a managed local `image` (at most 512, `[A-Za-z0-9._-/:@]`) with a command. Command: 1..=128 elements, each at most 8192 bytes without NUL/CR/LF, total at most 64 KiB.

### `environment_execute`

Takes `environment_id`, `operation_id`, `expected_revision`, `command` (explicit argv, no shell string) and optional `timeout_seconds`. It fails unless the profile revision equals `expected_revision` ("environment changed after command approval") and the environment is connected. Timeout is `min(requested, operation_timeout_seconds)`, 1..=3600. Output is captured per stream up to **16 MiB** into durable capture files, and an inline budget derived from `max_output_bytes` (bounded 1..=8192 per piece); a total live-capture reservation of **1 GiB** is enforced. A state other than `succeeded` returns an observation with `status` `failed`. If the receipt exceeds `max_output_bytes` the call fails, but the durable receipt remains. SSH uses `BatchMode=yes`, `StrictHostKeyChecking=yes`, a pinned known-hosts file, `IdentitiesOnly=yes` when an identity is set and `ControlPersist=no`; clients get only baseline environment plus agent or daemon variables (such as `SSH_AUTH_SOCK`).

**Failure behavior.** Operations carry a UUID and a durable receipt (`Prepared`, `Running`, `Succeeded`, `Failed`, `Cancelled`, `OutcomeUnknown`); **uncertain commands are never replayed**, and reusing an `operation_id` is only valid for identical arguments. `recover_execution` reconciles a journaled `environment_execute` from an exact terminal inner receipt and never dispatches a command or probe. `CommandNotDispatched` marks failures before any dispatch. Environment states: `Connected`, `Disconnected`, `Stopped`, `Missing`, `OutcomeUnknown` (an operation still holds the profile lease), `Unavailable`. `resolve` lets the user record an observed outcome. Terminal result archives (`archive.rs`, at most 1 GiB, 128 recent) keep operation-ID deduplication. `shutdown` stops only SSH masters created by this process.

### Storage (`storage.rs`)

Lives under `<management_root>/environments`: atomic records (at most 512 records, 1024 operations, database 2 MiB, receipt 256 KiB) guarded by cross-process file leases; no native command runs while the metadata lease is held. Container mutations address an inspected immutable ID and ownership labels (`com.qurkos.xylo.installation`, `.environment`, `.create-operation`). Without `management_root` there are no sessions to stop.

### Devcontainers (`devcontainer.rs`)

Discovery reads `<workspace>/.devcontainer/devcontainer.json` read-only and offers a supported subset; unsupported execution settings are rejected visibly. Setup runs only from the reviewed application action.

## Plugins (`plugins.rs`, `plugins/`)

Reviewed **Wasmtime component** plugins (wasmtime 50, component model, fuel metering, 512 KiB max wasm stack) with a durable catalog under `<management_root>/plugins` (`catalog.json`, `packages/`, `locks/`, `catalog.lock`). Management requires `management_root`; without it every management call fails.

- **Agent tools:** `plugin_list` (no writes) and `plugin_run_installed` (`writes`; args `id`, `package_sha256` (64 hex), optional `input` string up to 1 MiB). **Any other key is rejected**; grants and budgets come only from the catalog, never from the call. The pinned SHA-256 must match the installed package.
- **Application control** (`helper_plugin_*`, never advertised): `list`, `inspect` (`manifest_path`), `install` (requires `expected_package_sha256` matching a fresh inspection, then a trial compile bounded by the manifest timeout), `set_enabled`, `set_grants`, `remove`. Mutations replay by operation identity, and publication is awaited once admitted so a cancel cannot hide a committed catalog. `approval_presentation` shows reviewed identity, effective grants and budgets.

### Manifest and limits

`NativePluginManifest` (`format == 1`, `deny_unknown_fields`): `id`, `name` (1..=120 chars), `version`, `entrypoint`, lowercase `sha256`, `capabilities`, `budgets`. Defaults and accepted ranges for `PluginBudgets` (`security.rs`):

| Budget | Default | Allowed |
| --- | --- | --- |
| `memory_mib` | 32 | 8..=256 |
| `fuel` | 10,000,000 | 1..=100,000,000 |
| `timeout_seconds` | 15 | 1..=60 |
| `max_input_bytes` | 256 KiB | 1..=1 MiB |
| `max_output_bytes` | 256 KiB | 1..=1 MiB |

At most 32 unique grants, each requested by the package. Catalog limits: 64 packages, module at most 16 MiB (`.wat` expands to at most 32 MiB), 1 GiB package storage, 4096 retained operation IDs, 2 MiB registry. Preflight rejects nesting over 32, over 10,000 functions or over 1,000,000 instructions. Output is also capped at `(max_output_bytes - 2048) / 6` of the service budget so JSON escaping cannot force truncation. At most 2 compiled components are cached.

### Host imports and grants

Only three imports exist; any other import fails instantiation.

- `read-text` needs `read_workspace`: regular file, no links, application data files excluded; at most **4 MiB** read per run.
- `write-text` needs `write_artifacts`: a `.txt` artifact via no-clobber publish; at most **4 MiB** reserved per run.
- `http-get` needs `https:<host>`: HTTPS on port 443 only, no credentials or fragment, **redirects are not followed**, 15 s timeout, body capped at the output budget.

### Failure behavior

Errors are `Plugin cancelled`, `Plugin exceeded wall-clock budget`, fuel exhaustion and budget violations. A failure after an artifact commit returns `side_effects: artifacts_committed` with status `failed`. If the run stops while a write is completing, the status is `outcome_unknown` with `side_effects: outcome_unknown`; inspect artifacts before retrying. Updates and permission revocation are fenced by a per-plugin lease while a run is active.

## Local inference (`models.rs`)

A lazily started **llama.cpp server** bound to `127.0.0.1` on a free port. `LlamaConfig`: `executable`, `model` (absolute existing GGUF), `context_tokens` (0 = native fitting, else 512..=1,048,576), `threads` (0 = available parallelism), `gpu_layers` (-1 auto, -2 all, 0 CPU, up to 65,535), `fit_memory` (default true). The server runs with `--parallel 1`. Nothing is downloaded by this crate.

| Tool | `writes` | Behavior |
| --- | --- | --- |
| `local_inference_status` | no | State per helper: `running`, `exited`, `busy`, `configured_unverified`, `asset_missing`, `configuration_needed` |
| `local_inference_start` | yes | Spawns and polls `/health` every 200 ms until success, cancel or `operation_timeout_seconds` (the process is stopped on failure) |
| `local_inference_stop` | yes | Stops the server; **fails if a managed chat or generation holds the activity lease** |
| `local_inference_generate` | yes | `prompt` at most 256 KiB, `max_tokens` default 1024 clamp 1..=4096, non-streaming `/v1/chat/completions`, response at most `max_output_bytes` |

**Concurrency and failure.** Generation first takes an exclusive file lock `local-inference.lock` in the parent of the artifacts directory, waiting up to the operation timeout (`Local inference resource is busy`). The same lock serializes speech helpers. Any generation error **stops the server**. The server is evicted after `helper_idle_seconds` (5..=3600 s), but only when it can take both `local-inference-activity.lock` and `local-inference.lock`, so a running chat or generation is never stopped by eviction. `prepare_configuration_change` holds these locks through a configuration replacement.

### Managed endpoints (`managed_endpoints.rs`)

A running server registers `http://127.0.0.1:<port>/v1` with its PID in `managed-endpoints.json` (beside the artifacts directory, lock `managed-endpoints.lock`, 500 ms lock wait). It is an ownership hint that constrains concurrency, **not proof of health or authorization**. Only explicit loopback HTTP and a non-zero PID are accepted; entries of dead processes are pruned; limits are 256 entries and 256 KiB; the file is `0o600` on Unix. Dropping the registration removes only its own PID's entry.

## Speech (`speech.rs`)

`speech_transcribe` (`engine` `whisper` or `sherpa`, `path`) and `speech_synthesize` (`engine` `sherpa`, `text` 1..=4000 chars) run a configured `SpeechHelper` (`executable`, named `assets`, `args`, optional `synthesis_args`, `output_extension` default `txt`). Both are `writes` tools and take the shared inference lock.

- The argv template is expanded one pass, with no shell; at most 128 arguments. Placeholders: asset keys (alphanumeric or `_`, each an absolute existing file), `input`, `output`, `output_stem`, `threads`, `text`. **`{text}` must come after a `--` argument** so model-provided text cannot become an option.
- Transcription normalizes the input (opened with `open_input`) to a temporary WAV, at most 120 s (`audio::convert::MAX_SECONDS`); the transcript must be at most 256 KiB.
- Output is `speech-<uuid>.<ext>` (extension at most 12 alphanumerics), at most 64 MiB; synthesized WAV must be 8000..=192000 Hz, 1..=8 channels and at most 120 s.
- **Failure removes the output file**: helper failure (`Native speech failed (exit ..)`), timeout, cancel, missing output or an invalid layout. No helper, assets or synthesis adapter configured gives an error that points to Downloads.

## Audio (`audio/`)

`AudioService` (microphone capture and playback) is **not an agent tool**. It is exposed only as private helper controls and capture needs an explicit user action. `new(workspace, artifacts)` validates the two roots and never probes devices; status reports `device_status: not_probed` and `max_seconds` 120. `capture.rs` keeps the input callback to mixing into a lock-free SPSC ring; device creation, file I/O and conversion stay on the owned recording thread. Recording states include `starting`, `recording`, `finalizing`; `RecordingStatus` reports `peak` and `dropped_frames`. Dropping the service discards an active recording and cancels playback. `convert.rs` decodes, mixes to mono and resamples to 16 kHz (`TARGET_RATE`), at most 120 s (`MAX_SECONDS`) and 64 MiB (`MAX_AUDIO_FILE`).

## Assets and downloads (`assets/`)

`AssetManager` provisions native components (catalog ids: `pdfium-runtime`, `llama-runtime`, `whisper-runtime`, `sherpa-runtime`, `onnx-runtime`, `rust-analyzer`, plus the models `qwen3-0.6b-q8`, `qwen3-1.7b-q8`, `whisper-tiny-en`, `vits-ljs-int8`, `bge-small-en-v1.5`) into an application data root (absolute). **An install request names only a reviewed catalog id**; it cannot supply a URL, digest, destination or executable. It is not part of `tool_definitions`; the host calls `install`, `status`, `cancel`, `catalog`, `active_jobs` and `shutdown`.

- **Catalog** (`catalog.rs`, embedded `catalog.json`): at most 64 assets; license metadata required; each variant is keyed by platform (`<os>-<arch>` or `any`); pinned SHA-256 and size (individual asset at most 16 GiB); credential-free HTTPS URLs only; dependency ids must exist and cycles are rejected.
- **Download** (`download.rs`): `reqwest` with connect timeout 15 s, read timeout 30 s, total timeout 3600 s; redirects handled manually, **at most 5**; resumes from partial files with a verified `Content-Range`; the stream may not exceed the pinned size and **a SHA-256 mismatch installs nothing**. A per-asset file lock waits 5 s (`already downloading in another helper`).
- **Extraction** (`archive.rs`): at most 20,000 entries, 2 GiB per entry, 4 GiB expanded; supported formats include zip, tar with gzip or bzip2 and compressed single binaries. Paths, file types, permissions and sizes are never trusted; special entries are refused and links are materialized as bounded regular copies inside the staging root.
- **Activation** (`activation.rs`): re-verifies installed bytes against the receipt before exposing owned paths.
- **Jobs:** one install runs at a time (a semaphore of 1); phases `queued`, `downloading`, `extracting`, `building`, `verifying`; `JobStatus` is persisted with `owner_pid`. A development build can compile whisper.cpp with CMake (parallel jobs bounded by available memory, 30 min limit); distribution packages ship the prebuilt component.

## Scheduler tools

`tool_definitions()` also appends `xylo_scheduler::definitions()` (`schedule_list`, `schedule_create`, `schedule_delete`; timezone UTC). They are dispatched to `Scheduler` over `<artifacts>/schedules.json`; the logic lives in [scheduler](scheduler.md) and is not documented here.

## Data and persistence

| Location | Content |
| --- | --- |
| `<artifacts>/` | Tool outputs (`browser-*.png`, `display-*.png`, `pdf-*`, `speech-*`, exports, plugin `.txt` writes), `schedules.json` |
| OS temporary directory `xylo-obscura-*` | Private throwaway browser session state and readiness record, deleted on close |
| `<artifacts>/.document-operations/`, `.document-admission.lock` | Save receipts, leases, admission lock |
| `<artifacts>/..` (parent) | `local-inference.lock`, `local-inference-activity.lock`, `managed-endpoints.json` and its lock |
| `<management_root>/admission` | Shared resource admission leases |
| `<management_root>/plugins`, `<management_root>/environments` | Plugin catalog; environment records, operations, captures, archives |

Outputs never overwrite: `output_path` refuses an existing name, and document publishing is no-clobber.

## Concurrency and errors

- Constructors are cheap; native resources start on first use and stop on idle (browser and model server, `helper_idle_seconds` clamped 5..=3600).
- Errors are `anyhow` errors surfaced as tool failures; the output-truncation rule differs by prefix (see Dispatch). Cancellation is cooperative through `CancellationToken` in every service.
- Browser errors close the browser; generation errors stop the model server; document, plugin and environment operations use durable receipts and operation UUIDs so uncertain outcomes are reconciled, never silently retried.
- Cross-process coordination uses `fs2` file locks (document admission, inference, plugin catalog, environment leases, managed endpoints, asset locks).

## Security

- **No permission logic here;** the caller approves first. `definition(..., writes)` marks write tools. Agents cannot reach `helper_*` management controls, asset installs or audio capture.
- Native helpers never inherit the app environment (provider keys); executables must be absolute, existing and executable.
- Inputs open race-free beneath the workspace or artifacts root; management storage is refused and must be disjoint from artifacts.
- Browser: isolated bundled component, private local IPC, HTTP(S) only, no URL credentials, private networks blocked by default, no download tool. Process isolation is not a claim of Chromium-equivalent sandboxing or web compatibility.
- Plugins: reviewed and pinned by SHA-256; grants only from the catalog; three host imports; HTTPS-only without redirects.
- Environments: explicit argv, strict SSH host keys, revision-bound approval, owner-only private keys.
- Assets: pinned catalog URL, size and digest; unverified bytes are never installed.
- Desktop: OS permissions are checked on use and never assumed.

## Configuration

`CapabilityConfig` (above) is the only configuration surface. Optional paths (`pdfium_library`, `llama`, `whisper`, `sherpa`) default to `None`; the matching optional tools fail with a configuration message. The browser has no runtime path or URL setting; its sole network-policy flag is `browser_allow_private_network` (default `false`). Legacy persisted `chromium` fields are ignored and disappear on the next save, while unrelated settings are preserved. Helper configure/patch APIs reject `chromium`, `obscura` and `browser_url` explicitly. The host replaces `management_root`.

## Testing

Unit tests sit beside each module (`tests/`, `*/tests.rs`). Examples: `measure_documents`, `measure_plugins`, `validate_plugin_sdk`. Not run for this document; the commands are:

```sh
cargo test --locked -p xylo-capabilities documents
cargo test --locked -p xylo-capabilities environments
cargo test --locked -p xylo-capabilities plugins
cargo test --locked -p xylo-capabilities browser
cargo test --locked -p xylo-capabilities assets
```

## Related crates

- [core](core.md): `ToolDefinition`, `ToolObservation`, document, environment and plugin contracts.
- [engine-tools](engine-tools.md): the broker that approves and dispatches these tools.
- [runtime](runtime.md): process orchestration and execution journal.
- `helper.md`: the helper process serving `execute_control`; `resources.md`: admission leases and `spawn_blocking`; `scheduler.md`: schedule tools. (These three pages were not present in `docs/modules` when this page was written.)
