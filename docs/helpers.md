# Native UI helper

The application can invoke native capabilities directly, before an LLM account or local model is available. `workbench-helper-client` is a small Rust client/protocol library. `workbench-helper` is a separate executable containing Chromium, document, model, speech, desktop, plugin and remote services. The desktop must depend on the client crate only; native engines belong in the packaged helper process.

```rust,ignore
let helper = HelperClient::spawn(helper_executable, paths.data, workspace).await?;
let status = helper.execute("helper_status", &json!({}), cancel.clone()).await?;
let exported = helper.execute("document_export_docx", &json!({
    "name": "report",
    "paragraphs": ["Created by a native UI action."]
}), cancel).await?;
helper.shutdown().await?;
```

Keep one client per canonical workspace and reuse it. Cloning the client shares its process, request limits and lifecycle. A changed workspace requires a new helper; no request can replace its authorized workspace. `pid()`, `is_closed()` and `pending_count()` expose supervision state. The pending count is an idle-eviction hint, not an exclusive ownership lease. Failed calls are never automatically replayed: a cancelled write may already have taken effect.

## Direct controls

| Command | Arguments | Result |
| --- | --- | --- |
| `helper_version` | `{}` | Binary and protocol versions; used by the spawn handshake. |
| `helper_status` | `{}` | Canonical workspace/data/artifacts/models paths and current capability states. |
| `helper_tools` | `{}` | Native tool definitions, including each tool's `writes` flag, and control names. |
| `helper_get_config` | `{}` | `{"config": {...}}` with current persisted/default settings and forced authorized roots. |
| `helper_configure` | `{"config": {...}}` | Validate and replace the complete capability configuration, persist it atomically, and release existing browser/model processes. Assets remain unverified until used. |
| `helper_patch_config` | `{"config": {...}}` | Merge provided settings into current config under a process lock, validate and persist atomically. Nested object fields are preserved; `null` clears optional values. Use this for individual asset pickers. |
| `helper_reload_config` | `{}` | Reload `data/capabilities.json`, validate it and replace the service. |
| `helper_release_resources` | `{}` | Cancel outstanding work and release managed Chromium/llama processes. The next action starts them lazily. |
| Native tool name | Tool's declared arguments | Serialized `ToolObservation`: `status`, JSON `summary`, `next_actions`, and artifact paths. |

UI buttons can configure a locally provisioned llama binary/GGUF through `helper_configure`, then call `local_inference_start` to obtain a health-checked loopback `/v1` endpoint. They can also run document, browser, audio, desktop, plugin and remote tools through their ordinary definitions. No provider call is needed for these controls. See [capabilities.md](capabilities.md) for tools, asset formats, plugin grants, native limits and local model defaults.

Installed extension and environment setup uses a separate application-control dispatcher. `helper_plugin_*` and `helper_environment_*` controls are available to deliberate native UI actions, and are omitted from the agent tool definitions and ordinary capability dispatcher. Agents discover installed packages or saved environments and execute them through their advertised tools, with the normal action journal and approval policy. See [plugin management](native-plugins.md) and [execution environments](native-environments.md) for version pins, grants, operation receipts and recovery behavior.

The host remains responsible for permissions. A deliberate UI action authorizes its concrete native operation; agent-originated calls still go through the application's tool broker and approval policy. The helper is private local transport, not an authentication or authorization service. Do not expose its stdin/stdout as a public socket, or let a workspace document invoke controls by itself.

## Configuration and paths

Launch is `workbench-helper --data-dir ABSOLUTE_PATH --workspace ABSOLUTE_PATH`. The helper canonicalizes both roots, creates `data/artifacts` and `data/models`, and reads at most 1MiB from `data/capabilities.json`. A missing config means explicit `configuration_needed` states. Invalid configuration fails startup or reload visibly; it is not silently reset. Each load/save overrides configuration `workspace`, `artifacts`, and `management_root` with the authorized roots. `management_root` is always `data/native-management`; an imported configuration cannot move the plugin catalog, environment profiles, or lifecycle receipts into an artifact or workspace directory. Tool file inputs exclude that private root, even if the selected workspace contains the application data directory. Native executable/model/library paths are explicit local assets; this helper downloads nothing.

Configuration writes use a unique temporary file, file sync, atomic replacement, parent-directory sync on Unix, and Unix mode `0600`. `helper_configure` replaces the full config. Asset pickers should use `helper_patch_config`, for example `{"config":{"llama":{"model":"/absolute/models/chosen.gguf"}}}`, which preserves the executable and other advanced settings. Patches reread and merge under `data/capabilities.lock`, reject unknown top-level keys, validate the merged config before saving, and cannot replace authorized roots. Concurrent helpers preserve independent patched fields. Normal readers see the prior or replacement file, not a partial write.

## Transport, cancellation and supervision

The protocol is versioned JSONL over private anonymous pipes. Each command has a UUID; each response carries the same UUID. Stderr is inherited for diagnostics and stdout contains protocol only. There is no helper HTTP listener. Native Chromium and llama may create their own loopback control endpoints when explicitly started.

Frames are capped at 8MiB before a terminating newline; reads reject oversized, empty and interrupted frames without allocating an unbounded line. Client request/control queues and the helper input/output queues each hold at most 64 messages. The client admits at most 64 concurrent execution requests. The helper rejects additional active work rather than spawning an unbounded number of tasks. Binary stdout backpressure has a ten-second deadline. Spawn performs a ten-second version handshake.

Cancellation has an independent priority channel. It reaches the request's cancellation token; bounded pending-cancellation records also prevent a cancellation that overtakes a queued request from starting that request afterward. Dropping an in-flight request future makes a best-effort cancellation request. Unknown commands and normal tool failures affect their request only. Broken pipes, invalid protocol and process exit fail pending requests; callers can explicitly spawn a new helper after inspecting uncertain writes.

`shutdown()` requests cancellation and normal capability cleanup, waits for the helper, then kills/reaps it if shutdown exceeds its grace period. Dropping the last client first requests shutdown and schedules a forced stop after eight seconds when a runtime is available. Browser and model services retain their own lazy loading and idle release timers. For host memory pressure, call `helper_release_resources`; shutting down the helper also releases its Wasmtime engine and remaining native process memory. Pressure detection/admission belongs to the application's resource manager.

The managed llama idle timer honors two separate OS leases. The host holds a **shared** `data/local-inference-activity.lock` across the entire managed root turn, including tool execution, and acquires it before ensuring the model is started. The provider's **exclusive** `data/local-inference.lock` spans only a generation request/stream. Idle eviction and explicit model shutdown try both locks exclusively without waiting. An active turn or generation produces an actionable busy error for stop/reconfigure; idle checks refresh the activity time and retain a full grace period afterward. `local_inference_start` can still start a missing server while the root's shared activity lease is held. Before every managed root turn, the host ensures start and refreshes its returned ephemeral endpoint.

Configuration replacement calls `CapabilityService::prepare_configuration_change()`, retaining its typed exclusive idle lease through validation/persistence/service replacement. A busy chat leaves the persisted configuration unchanged. External Ollama/vLLM services manage their own concurrency. Do not create multiple model helpers against one GPU unless the host's resource manager admits that load.

After a model server passes its health check, its owner registers `data/managed-endpoints.json` as `{"http://127.0.0.1:PORT/v1":{"single_slot":true,"pid":SERVER_PID}}`. URL normalization trims a trailing slash. Concurrent owners update this bounded file under an adjacent file lock and atomic replacement. Explicit stop, idle stop and normal owner teardown remove their own marker while preserving other servers. The marker is a concurrency hint; it does not establish server health, and an OS-forced crash can leave a stale entry.

Native parsers and OS permission dialogs may enter calls that cannot safely be interrupted from a Rust thread. Native process isolation and bounded helper shutdown keep these calls outside the UI process. Cancellation remains an uncertain completion boundary for writes; it does not undo clicks, remote commands or partially completed exports. An OS-forced helper crash needs separate descendant-process acceptance on each platform; graceful native cleanup is the tested route.

## Verification

Run `cargo test --locked -p workbench-helper-client -p workbench-helper`. Integration tests launch the real compiled helper, use private pipes and temporary local directories, verify handshake/status/concurrent UUID correlation, export and read a real DOCX, save/reload configuration, reject oversized requests, and prove that an early priority cancellation prevents dispatch. On Unix, a local executable fixture verifies cancellation terminates and reaps the native command while the helper remains usable. No external website, account, model download, Python or Node is required.

These helper/client tests passed on 25 October 2026 on macOS arm64 / Rust 1.98. They also verify a shared root-activity lease rejects patch/reload without changing the persisted file. The capability lifecycle regression uses a real native process to verify a root turn doing tools survives idle eviction and rejects shutdown, then stops after activity releases. The existing native capability suite and separate real Chromium acceptance are recorded in `capabilities.md`.

Windows/Linux packaging and native asset availability remain separate acceptance work. An unconfigured status is a truthful setup state, not evidence that PDFium, a speech model, GPU inference or desktop permissions were exercised.


The native asset controls are documented in [assets.md](assets.md). Their catalog is reviewed and pinned; Install and Use are explicit separate actions. Protocol responses include serde-default `background_jobs`, with unsolicited nil-ID updates on background completion; clients update idle ownership and ignore unmatched request IDs. `pending_count()` includes native asset jobs.

## Native speech and audio controls

Capture and playback are private product controls, absent from the LLM tool definitions. Opening the Voice card, helper startup, status requests, and asset setup never open or probe an audio device. Only an explicit Record or Play action supplies `user_initiated:true`. The helper validates this before touching native audio APIs. Device enumeration is a separate explicit request; it is not part of startup/status.

| Command | Arguments | Behavior |
| --- | --- | --- |
| `helper_audio_status` | `{}` | Recording state, playing flag, 120-second bound; `device_status:"not_probed"` |
| `helper_audio_devices` | `{}` | Up to 32 input IDs/names and current default device metadata |
| `helper_audio_record_start` | `{user_initiated:true,max_seconds?:120,device_id?}` | Opens the selected/system input only after the user action; returns recording status after startup |
| `helper_audio_record_status` | `{}` | Current/latest recording metadata; never starts capture |
| `helper_audio_record_stop` | `{recording_id}` | Stops capture and saves a 16kHz mono PCM16 WAV artifact |
| `helper_audio_record_cancel` | `{recording_id?}` | Stops and discards capture; omitted ID allows cancelling permission/device startup |
| `helper_audio_play` | `{path,user_initiated:true,volume?:0.8}` | Plays an app artifact using the native output device; bounded/cancellable |
| `helper_audio_play_stop` | `{}` | Requests immediate playback stop |

Recording metadata contains `recording_id`, `state`, `sample_rate`, `channels`, `max_seconds`, `elapsed_ms`, `peak`, `dropped_frames`, `path`, `duration_seconds`, and `message`. States are `starting`, `recording`, `finalizing`, `recorded`, `cancelled`, and `failed`; an unused recorder returns `{state:"idle"}`. During capture rate/channels describe the device. A finalized recording reports 16kHz/mono and its exact output duration. Peak is a 0–1 input meter. A failed or cancelled recording is not a usable artifact.

The host uses one stable app-data helper for audio controls, independent of the selected project. Project-file transcription still uses the project helper. This makes Stop/Cancel reach the original microphone if the project changes. `SpeechUi::stop_speech` also cancels active UI operations before project replacement and shutdown. Startup cancellation sends both cancellation of the original request UUID and a recording-cancel control, covering an already completed startup response. Native audio Stop/Cancel has eight reserved client/dispatcher slots and uses the priority control channel so normal operations cannot occupy all stopping capacity.

Capture/playback ownership is included in protocol `background_jobs`; the client preserves the helper between status calls. The capture thread has a 65536-frame lock-free SPSC ring. Its realtime callback only converts/mixes samples, pushes bounded samples and updates atomics. It performs no file I/O, locks, allocations, model work or JSON encoding. File writing, finalization and conversion happen on the owned recording thread. Any ring overflow aborts and removes the incomplete recording; drops are never silently accepted. Recordings stop after at most 120 seconds. Device loss, permission denial, startup timeout, cancellation and helper shutdown release streams and remove temporary files.

Speech files are limited to 64MiB, 120 seconds, 8–192kHz and 1–8 input channels. Native decoding supports WAV, MP3, AAC/M4A, FLAC and Ogg/Vorbis. Rubato's band-limited FFT conversion mixes to mono, converts to 16kHz, removes resampling latency and checks cancellation between bounded chunks. The whisper adapter receives that normalized WAV, then removes it after native completion/failure/cancellation. Transcript text is returned directly in the result's `text` field and preserved as a text artifact. Native synthesis validates its resulting WAV and refuses an output longer than 120 seconds. Its output is not automatically played.

Speech model execution holds the same cross-process local-inference request lease as managed generation, without taking the root-chat activity lock. Native CLI stdout/stderr, duration and cancellation retain the existing process-supervision bounds. CPAL streams and native models remain unloaded until requested. Rodio keeps the output stream alive until completion/cancel and reports unavailable/disconnected devices with actionable errors.

The desktop speech module renders Record/Stop/Discard, an elapsed-time/input-level indicator, project-audio transcription, a transcript review/Add-to-message action, explicit native synthesis, Play and Stop playback. It does not automatically send a transcript or play generated audio. Request UUIDs correlate results; cancelled and superseded results cannot overwrite a newer speech operation.

Primary APIs: [CPAL 0.18.2](https://docs.rs/cpal/0.18.2/cpal/), [Rodio 0.23.0](https://docs.rs/rodio/0.23.0/rodio/), [Rubato 1.0.1](https://docs.rs/rubato/1.0.1/rubato/), [sherpa native LJSpeech/VITS examples](https://k2-fsa.github.io/sherpa/onnx/tts/pretrained_models/vits.html). These are native libraries/helpers; built-in execution needs no Node/Python.

### Headless speech acceptance

Offline tests cover the user-action gate without touching devices, 48kHz stereo→16kHz mono conversion/duration/RMS, anti-alias attenuation, cancellation cleanup, in-memory playback decoding/mixing/Stop without an output device, and priority cancellation while all 64 normal client slots are held. Helper protocol tests verify idle audio status and refusal of capture/playback without a user action.

The explicit native asset test installed/activated the pinned VITS int8 voice, sherpa runtime, Whisper Tiny English and bundled whisper CLI in an isolated test data directory. It generated an actual WAV and transcribed it using whisper.cpp. The output included “native speech test”; Tiny English rendered the initial “Hello” as “Although”. This is an integration check, not a transcription quality benchmark. The test verified no input/output device was probed, no microphone was opened and no playback occurred; temporary normalized audio was cleaned up.

Repeat explicitly with `WORKBENCH_SPEECH_SMOKE_DATA=/absolute/isolated/speech-test-data cargo test --locked -p workbench-helper --test speech_pipeline -- --ignored --nocapture`. This command selects/downloads only the catalog test voice/transcription models and changes setup only under that isolated directory. It does not affect the user's active models.

The Mac remained locked. Live microphone permission/recording, physical speaker playback, native GUI interaction, Linux audio backend prerequisites and Windows privacy/device behavior require separate unlocked/platform acceptance. CPAL's Linux build needs ALSA development headers and runtime audio access. The app bundle includes the microphone purpose string; hardened macOS distribution also needs its audio-input entitlement. Do not invoke the capture test unattended to bypass OS permission.
