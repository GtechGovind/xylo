# Native capability services

`workbench-capabilities` implements application-owned tools without a Python or Node runtime. It is linked into the execution worker, outside the desktop UI. `CapabilityService::new(config)` validates roots and creates the artifact directory; it does **not** start a browser, load a model, load PDFium or compile WebAssembly.

The broker must apply its permission decision to the complete tool name and arguments before calling `execute`. Definitions mark external actions and exports as writes. Reading/snapshot tools can still expose private data; workspace access is bounded to the canonical selected workspace and capability artifact roots. Native executable and model/library paths are provisioned by the reviewed native asset manager; explicit imports remain available to internal integrations. See [assets.md](assets.md).

```rust,ignore
let service = CapabilityService::new(CapabilityConfig {
    workspace: project_path,
    artifacts: app_data.join("artifacts"),
    chromium: Some(local_chromium_executable),
    pdfium_library: Some(local_pdfium_dynamic_library),
    ..Default::default()
})?;
let definitions = service.tool_definitions();
let observation = service.execute("browser_navigate", &json!({"url":"https://example.com"}), cancel).await?;
service.shutdown().await?;
```

## Availability contract

`status()` distinguishes `configuration_needed`, `asset_missing`, `configured_unverified`, `running` and actual completion. A path existing on disk does not establish that its helper, model, OS permissions or account works. Browser, PDF, model and speech tool errors retain that distinction. Nothing silently downloads a binary, model, voice, browser or library. The setup layer supplies verified assets for each OS/architecture, with their upstream notices and licenses.

## Implemented tools

| Area | Tools | Implementation and limits |
| --- | --- | --- |
| Browser | `browser_navigate`, `browser_snapshot`, `browser_click`, `browser_type`, `browser_wait`, `browser_screenshot`, `browser_download`, `browser_close` | `chromiumoxide` CDP against an explicitly configured or detected installed Chromium browser; isolated temporary profile; HTTP(S) navigation; real Accessibility.getFullAXTree; CSS input; bounded viewport screenshots; authenticated profile downloads with size limits and source URL. Idle release closes/reaps the browser and removes its profile. Cancellation/error closes the session so uncertain clicks are not replayed. |
| Document intake | `document_read`, `document_pdf_preview` | PDFium text/page preview through an explicitly configured native library; calamine XLS/XLSX/XLSM/XLSB/ODS value extraction; DOCX XML text; bounded UTF-8 text. Source path, page and sheet are preserved in observations. PDF previews are at most 2048px per axis. Office ZIP metadata is inspected before parsing, limiting total decompressed sizes and entry counts. |
| Office exports | `document_export_xlsx`, `document_export_docx` | Native `rust_xlsxwriter` and `docx-rs`; bounded rows/cells/paragraphs; DOCX source references become clickable hyperlinks. Spreadsheet strings starting with `=` remain text. Outputs use validated basenames in artifacts and do not intentionally overwrite existing documents. |
| Local inference | `local_inference_status`, `local_inference_start`, `local_inference_generate`, `local_inference_stop` | Managed native `llama-server`, explicit GGUF; loopback-only ephemeral port; `/health` readiness; OpenAI-style `/v1` endpoint; context/thread/GPU-layer configuration; one generation slot; bounded response; idle unload and shutdown. No embedded model is preloaded at app startup. |
| Speech | `speech_transcribe`, `speech_synthesize` | Native whisper.cpp or sherpa-onnx executables with explicit model files and argv templates. One-shot processes have bounded I/O, timeout/cancellation and cleanup. Templates preserve argument boundaries. Synthesis uses a separately configured template. |
| Desktop | `desktop_displays`, `desktop_screenshot`, `desktop_click`, `desktop_type` | Native xcap/enigo. Actual OS permission failures are returned. Wayland global input is explicitly unavailable in this backend rather than pretending X11 injection works. No automatic permission bypass. |
| Plugins | `plugin_list`, `plugin_run_installed`, legacy `plugin_run` | Reviewed, hash-pinned component packages; fresh Wasmtime stores, explicit user grants, atomic no-clobber artifacts and bounded compilation/execution/I/O. Installation and grant changes use separate native UI controls. See [installed plugins](native-plugins.md). |
| Remote | `remote_execute` | Direct native SSH or existing docker/podman container execution; argv-based local invocation; shell-quoted remote argv; SSH BatchMode and StrictHostKeyChecking. Does not create machines/containers or change host-key policy. |
| Saved environments | `environment_list`, `environment_status`, `environment_execute` | Revision-pinned native SSH/container targets, verified connection state, bounded argv/output and durable operation receipts. Explicit UI controls manage profiles and owned container lifecycle; unknown effects require reconciliation. See [native environments](native-environments.md). |
| Scheduling | `schedule_list`, `schedule_create`, `schedule_delete` | Lightweight `workbench-scheduler` crate, shared by host and worker; stable cross-process file lock, fresh read per operation, atomic synced writes, bounded payload, retained workspace and five-minute leases. Application claims due tasks, renews active leases and acknowledges completion. The scheduler does not silently execute shell commands. |

## Local inference configuration

```json
{
  "llama": {
    "executable": "/absolute/native/llama-server",
    "model": "/absolute/models/model.gguf",
    "context_tokens": 0,
    "threads": 0,
    "gpu_layers": -1,
    "fit_memory": true
  },
  "helper_idle_seconds": 60,
  "operation_timeout_seconds": 120,
  "max_output_bytes": 1048576
}
```

The defaults let the native backend fit available hardware: zero threads resolves the process's available CPU parallelism when the server starts; GPU layers `-1` selects native `auto`, `-2` selects explicit `all`, and `0` requests CPU only. Context zero leaves `--ctx-size` unset so `--fit on` can choose context/KV memory. Positive thread/context values and explicit GPU counts remain overrides; fitting can be disabled with `fit_memory: false`. This behavior is checked against the packaged [llama.cpp argument parser](https://github.com/ggml-org/llama.cpp/blob/78a8ae14be559b45f5f13ddee82c188b683d5d8d/common/arg.cpp) and [memory fitter](https://github.com/ggml-org/llama.cpp/blob/78a8ae14be559b45f5f13ddee82c188b683d5d8d/common/fit.cpp). An explicit native `--ctx-size 0` has different semantics and is intentionally omitted.

Memory use depends on weights, quantization, context/KV cache, backend and GPU; configuration alone makes no hardware performance claim. The managed server has one generation slot. Each `local_inference_generate` request acquires an OS file lock at `artifacts.parent()/local-inference.lock`, shared with the host's managed local provider path; it holds that lock only for the request, never for the service lifetime. External Ollama/vLLM/provider services manage their own concurrency. Lock acquisition is cancellable and deadline-bounded. Calls made through this tool also hold the server's active-use lease during generation. A consumer using the returned HTTP endpoint directly must hold the shared file lease; idle cleanup checks that lease before stopping the server. A health-checked server is registered in `managed-endpoints.json` for the host's managed-provider detection and removed on normal lifecycle cleanup. Prefer the service's generation path for managed lifetime.

Managed root turns additionally hold a shared `local-inference-activity.lock` across provider calls and tools. Idle cleanup, shutdown and configuration replacement try exclusive ownership without blocking; active chats reject stop/reconfigure with a busy error. Configuration changes retain the exclusive lease until the replacement is installed. Starting a missing server is allowed while the root's shared activity lease is held. The host must refresh the managed endpoint before each root turn because an idle unload can release its previous ephemeral port.

## Speech argv configuration

`SpeechHelper` accepts `executable`, a map of named `assets`, a transcription `args` vector, an optional `synthesis_args` vector and `output_extension`. Asset placeholders, `{input}`, `{text}`, `{output}`, `{output_stem}` and adaptive `{threads}` are expanded **inside individual argv values**, never evaluated as shell source. Native binary versions differ; configuration must match the selected helper and model format.

Example for a local whisper.cpp CLI that supports `-otxt`:

```json
{
  "executable": "/absolute/native/whisper-cli",
  "assets": {"model": "/absolute/models/ggml-base.en.bin"},
  "args": ["-m", "{model}", "-f", "{input}", "-of", "{output_stem}", "-otxt"],
  "output_extension": "txt"
}
```

For sherpa-onnx, map its encoder/decoder/tokens/voice files into `assets` and use the actual native executable's supported flags. Do not reuse a transcription model as a TTS voice. Audio codecs/sample rates must be supported by the configured helper; this layer does not silently install FFmpeg or convert unsupported input.

## Component plugin ABI

The [permission-free echo package](../examples/plugins/echo/README.md) can be installed and run without a provider account or external compiler. Installed-package management, immutable identity checks and limits are described in [native plugins](native-plugins.md). Saved SSH/container profiles, lifecycle controls and interrupted-operation recovery are described in [native environments](native-environments.md).

Components export:

```wit
package workbench:plugin;
world tool {
    import read-text: func(path: string) -> result<string, string>;
    import write-text: func(name: string, text: string) -> result<string, string>;
    import http-get: func(url: string) -> result<string, string>;
    export run: func(input: string) -> result<string, string>;
}
```

Imports are optional: components need declare only those used. The host exposes no WASI. Read paths must remain inside canonical approved roots. Writes create artifact `.txt` files using validated names; arbitrary workspace writes are not granted. HTTPS redirects are not followed, and each host requires its own exact grant. Each plugin invocation's grant list belongs in the application's approval UI.

Default reviewed guest budgets are 32MiB, 10 million fuel and 15 seconds, with separate input/output limits. Fuel yields make cancellation observable during guest computation. The Wasmtime engine initializes on first use. A bounded in-process component cache reuses validated compilation; untrusted serialized native code is never deserialized. Compilation admission and validation are bounded, and cancellation retains compile/run leases until admitted native work actually ends. Native compiler calls are not forcibly preempted; the supervised process is the final containment boundary. See the plugin guide for exact current quotas and the performance report for measured workload scope.

## Scheduler coordination

The host can depend on `workbench-scheduler` without linking Chromium/PDFium/Wasmtime. `Scheduler::new(store_path, workspace)` returns a cheap handle; `list`, `save`, `create`, `remove`, `claim_due`, `complete`, `renew` and `release` are async. `ScheduledTask` retains its canonical workspace, prompt, cron, next run, enabled state and lease expiry. Dispatch into that retained workspace. Renew once per minute while active, complete only after success, and release a failed/cancelled claim according to the coordinator's retry policy. Prevent immediate repeated retries after a permanent failure.

Each operation takes the stable adjacent `.lock` file and rereads the data store while locked. Lock acquisition has a five-second deadline and happens off the async runtime. Writes sync a unique temporary file before atomic replacement and sync the parent directory on Unix. Unchanged polls do not rewrite/fsync the store. The store is limited to 128 tasks and 2MiB; invalid state is reported, never silently reset.

## Process lifecycle and OS boundaries

Native helpers use explicit absolute executable paths, no local command shell, null stdin, continuously drained bounded stdout/stderr, deadlines and cancellation. Unix helpers get their own process group; the group is killed on timeout/cancel/drop and after normal parent exit. Windows helpers are assigned to a kill-on-close job object. Browser lifecycle is owned by chromiumoxide, which kills/reaps its launched browser on drop; this service also requests graceful close and bounds its wait.

The native capture/input paths require platform acceptance: Screen Recording and Accessibility on macOS; interactive desktop/session access on Windows; X11/Wayland compositor support on Linux. Wayland capture may require a portal/session grant. The current backend deliberately rejects Wayland global input. Keyboard layout, monitor scaling and permission dialogs need real device tests. Native service code compiling on macOS does not prove Windows/Linux packaging or permissions.

PDFium and office parsers run in a blocking worker task with file/page/row/byte budgets and cooperative cancellation between operations. A single native library parse/render call cannot be interrupted safely from a Rust thread. For hostile/untrusted documents, the application should execute the capability worker as an isolated process and terminate that process on deadline; async timeout alone does not forcibly stop an in-progress native parser. This constraint must remain visible in deployment review.

SSH host keys/credentials, provider accounts, browser logins, configured model files, PDFium symbols, speech assets and OS permissions all require separate integration tests. The offline suite tests process truncation/termination, document round trips, Wasmtime component execution/fuel exhaustion, scheduler persistence/leases, path/URL validation and template argument boundaries without accounts or downloaded models.

## Acceptance evidence

On 25 October 2026, macOS arm64 / Rust 1.98:

- `cargo +stable check -p workbench-capabilities` passed.
- Offline capability tests passed, including XLSX/DOCX round trips, component echo execution, fuel exhaustion, explicit read grant/escape rejection, bounded child output/timeouts, automatic model configuration/explicit overrides and shared inference lock cancellation. Lightweight scheduler tests passed for two independently constructed handles competing for a lease, renewal/release/completion, and two simultaneous OS processes saving without lost updates.
- The separate real Chromium test passed using the machine's existing Google Chrome binary against a temporary loopback HTTP server: navigate, selector wait, focus/type, click, changed AX snapshot, PNG screenshot, authenticated-profile CDP download with exact content check, and browser shutdown. It required no external site, account or browser download. This tests the browser service; it does not assert GUI routing or Windows/Linux acceptance.

To repeat that opt-in platform test, set `WORKBENCH_CHROMIUM` to an absolute installed browser executable and run `cargo +stable test -p workbench-capabilities browser::tests::managed_chromium -- --ignored --nocapture`. The normal offline suite intentionally leaves it ignored so a missing native asset cannot masquerade as a pass.
