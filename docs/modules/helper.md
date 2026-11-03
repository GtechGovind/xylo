# Native capability process (`xylo-helper` and `xylo-helper-client`)

## Purpose and boundaries

`xylo-helper` is a separate native binary that hosts heavy capabilities (documents, audio, assets, plugins, environments and the other `xylo-capabilities` tools) outside the desktop process. `xylo-helper-client` is the small crate the host links to start, talk to and supervise that process. The process boundary is deliberate: `main.rs` notes that native parsers may sit in an uninterruptible call, so shutdown can release those threads rather than hang the UI.

- **Stdout carries protocol frames only; diagnostics go to stderr** (`main.rs` routes `tracing` to stderr).
- The transport is stdin/stdout of a child process. It is never a network listener (`protocol.rs` docs).
- `helper-client` depends only on `anyhow`, `serde`, `serde_json`, `tokio`, `tokio-util` and `uuid`. Its docs state it has no dependency on browser engines, document engines, Wasmtime or model runtimes. `helper` adds `xylo-capabilities`, `tracing-subscriber` and `fs2`.
- The workspace dependent of `xylo-helper-client` found in `Cargo.toml` files is `runtime`; the helper binary is bundled as `Contents/MacOS/xylo-helper` per `crates/distribution` tests and `packager.json`.

## Module map

| Path | Purpose |
| --- | --- |
| `crates/helper/src/main.rs` | CLI parsing, stderr logging, multi-thread Tokio runtime, 2 s runtime shutdown timeout |
| `crates/helper/src/server.rs` | `serve`: reader, dispatcher, writer tasks; `State::execute` routing; configuration persistence |
| `crates/helper/tests/` | `native_rpc.rs`, `asset_install.rs`, `speech_pipeline.rs` integration tests |
| `crates/helper-client/src/protocol.rs` | Wire types, constants, `read_frame`, `encode`, `write_frame` |
| `crates/helper-client/src/lib.rs` | `HelperClient`: spawn, execute, cancel, shutdown, supervision |

## Command line

`xylo-helper --version` prints `Xylo native helper <version>`. Otherwise exactly `--data-dir ABSOLUTE_PATH --workspace ABSOLUTE_PATH` is required; any other arguments or a relative path fail with a usage error. `serve` creates the data directory, canonicalizes both roots, requires the workspace to be a directory and creates `<data>/models`.

## Protocol (`protocol.rs`)

JSON Lines: one JSON object per line, UTF-8, `\n` terminated. `PROTOCOL_VERSION = 1`, `MAX_FRAME_BYTES = 8 MiB` (excluding the newline), `QUEUE_CAPACITY = 64`. All message structs use `deny_unknown_fields`.

- `Request { protocol_version, id: Uuid, command }`, with `Request::new` generating a v4 id. `Command` is tagged by `kind` (`snake_case`): `execute { name, args }`, `cancel { request_id }`, `shutdown`.
- `Response { protocol_version, id, outcome, background_jobs }` (`background_jobs` defaults to 0). `Outcome` is tagged by `status`: `completed { value }` or `failed { code, message }`.
- `Response::failed` truncates the message to 16 Ki characters so a diagnostic cannot become a second oversized frame.
- `read_frame` is bounded even for a newline-free stream: it errors on more than 8 MiB, on an empty frame, and on EOF inside a partial frame (**a partial final frame is rejected, never interpreted**). `encode` and `write_frame` refuse frames over 8 MiB.

Failure codes emitted by the server: `protocol_version`, `duplicate_request`, `cancelled`, `invalid_command`, `busy`, `operation_failed`, `operation_panicked`, `response_too_large`. The client surfaces a failure as the error string `"<code>: <message>"`.

## Server behavior (`server.rs`)

### Tasks and dispatch

`serve` runs three cooperating pieces:

- A **reader task** reads frames from stdin into a 64-entry channel, so a partial frame is never lost when a tool completion wins a `select`. An invalid request or oversized frame ends the reader with an error.
- A **dispatcher loop** validates and routes requests, and runs each `execute` as a task in a `JoinSet`.
- A **writer task** owns stdout (`BufWriter`) behind a 64-entry response channel. Each write has a **10 s timeout**; a stuck pipe stops the writer and then the dispatcher. Every response is stamped with the current `background_jobs` (asset jobs + audio jobs + active environment sessions). A 1 s tick emits an unsolicited `completed` frame with the nil UUID and `{"event":"background_jobs_changed"}` when that count changes. Responses that cannot be encoded within 8 MiB are replaced with `response_too_large`.

Dispatch rules for each request:

- A `protocol_version` other than 1 gives `protocol_version`; an id that is already active gives `duplicate_request`.
- `execute` names must be 1..=128 ASCII alphanumeric or `_` characters, else `invalid_command`.
- At most **64 active requests**, plus 8 extra for the audio stop controls `helper_audio_record_stop`, `helper_audio_record_cancel` and `helper_audio_play_stop`; beyond that the reply is `busy`.
- `cancel` cancels the request's token and replies `{cancellation_requested, request_id}`. If the target has not been dispatched yet (the control channel can overtake a queued execute), a **tombstone** is kept (bounded to 128 entries, oldest evicted) and the later execute fails with `cancelled` and never starts.
- `helper_configure`, `helper_patch_config`, `helper_reload_config` and `helper_release_resources` first cancel every active request.
- `shutdown` replies `{"state":"stopping"}` and leaves the loop.
- A panicking operation yields `operation_panicked` ("inspect artifacts before retrying any write"); an error yields `operation_failed` with the full error chain.

### Routing of `execute` names (`State::execute`)

- `helper_audio_*` goes to `AudioService`; `helper_plugin_*` and `helper_environment_*` go to `CapabilityService::execute_control`.
- Built-in controls: `helper_version` (name, version, `protocol_version`), `helper_status` (workspace, data dir, artifacts, models, capability status), `helper_tools` (tool definitions and control names), `helper_asset_catalog`, `helper_asset_install`, `helper_asset_status`, `helper_asset_cancel`, `helper_asset_activate`, `helper_get_config`, `helper_configure`, `helper_patch_config`, `helper_reload_config`, `helper_release_resources`.
- Any other name is passed to `CapabilityService::execute` and its observation is returned as JSON.

### Configuration

- Config lives at `<data>/capabilities.json`, **at most 1 MiB**. Invalid configurations reset to defaults. Unknown legacy fields such as `chromium` are ignored without deleting a valid file or resetting unrelated settings, and disappear on the next save.
- **A file cannot move the permission boundary**: `workspace`, `artifacts` (`<data>/artifacts`) and `management_root` (`<data>/native-management`) are always overwritten after loading.
- `helper_configure` replaces; `helper_patch_config` merges recursively and rejects unknown top-level keys; `helper_reload_config` reloads from disk. Each takes a write lock on the service, a 5 s `capabilities.lock` file lock (`Capability configuration is busy` on timeout), and `prepare_configuration_change()`, so a busy chat leaves configuration intact. The new service is built (validating budgets) before an atomic save: temp file with mode `0600`, `sync_all`, rename, directory sync.
- Both configure and patch reject `chromium`, `obscura` and `browser_url` explicitly: there is no custom executable, browser endpoint or backend selector. `browser_allow_private_network` is an explicit broad boolean opt-in (default `false`) for loopback/private-network access, not a per-host grant.
- The render-enabled Obscura v0.2.6 component is source-built and packaged as the sibling `xylo-obscura` executable (`.exe` on Windows), launched on use through private local IPC. It is not an asset download, cloud service, MCP connection or installed-browser fallback. Seven browser tools are available; `browser_download` is unsupported before launch.
- Construction starts no heavy native service. `helper_release_resources` shuts down the managed browser and llama server; the process exiting releases the rest.

### Shutdown order

On exit the dispatcher cancels all active tokens, drains the job set for up to 3 s then aborts the remainder, shuts down audio, assets and the capability service (3 s limit), aborts the reader, and waits up to 3 s for the writer to flush. Errors are reported in this order: writer stopped, reader error, capability cleanup timeout.

## Client API (`HelperClient`)

`HelperClient` is `Clone`; clones share one supervised child and its bounded queues. **A helper serves exactly one canonical workspace**; create another client to change it.

- `HelperClient::spawn(executable, data_dir, workspace)`: the executable must be absolute (it is canonicalized and must be a file), the data dir must be absolute (created if missing), the workspace is canonicalized and must be a directory. It then performs a handshake: `helper_version` with a 10 s timeout, requiring `protocol_version == 1` (`Native helper handshake version mismatch` otherwise).
- `execute(name, args, cancel) -> Result<Value>`: validates `name` with the same rule as the server (1..=128 ASCII alphanumeric or `_`) and returns the `completed` value or an error `"<code>: <message>"`.
- `shutdown()`, `pid()`, `is_closed()`, and `pending_count()` (in-flight calls plus helper-reported background jobs capped at 64; a poisoned lock reports 64). The host uses `pending_count() == 0` to decide which helpers are idle.

## Process launch and environment

The child runs with `--data-dir <dir> --workspace <dir>`, current directory set to the workspace, piped stdin and stdout, **inherited stderr**, and `kill_on_drop(true)`. On Windows `CREATE_NO_WINDOW` is set.

**The environment is cleared.** Only these parent variables pass through: `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `TMPDIR`, `SSH_AUTH_SOCK`, `DOCKER_HOST`, `CONTAINER_HOST`, `RUST_LOG`, any `LC_*`, and on Windows `SYSTEMROOT`, `WINDIR`, `TEMP`, `TMP`, `USERPROFILE`, `LOCALAPPDATA`, `APPDATA`, `PATHEXT`, `COMSPEC`. Provider API keys and other application secrets therefore never reach the helper or its native tools. The last three socket-style variables exist so the helper can hand them to ssh and docker/podman.

## Client supervision

Three tasks are spawned per client, all tied to one `stop` token:

- **Writer**: takes from the control queue first (`biased`), then the request queue, writing with a **10 s timeout**. A failed or timed-out write fails all pending calls and stops the client.
- **Reader**: parses responses, requires protocol version 1, stores `background_jobs`, and completes the matching pending `oneshot`. A closed pipe, invalid frame, version mismatch or stop token fails all pending calls.
- **Monitor**: waits for child exit, or kills and reaps the child when `stop` is cancelled. It then fails all pending calls ("Native helper exited") and signals the `exited` watch.

`Shared::fail` marks the client closed, zeroes the background count and sends the reason to every pending caller. **After any failure the client is permanently closed; there is no automatic restart.** The host replaces it: in `crates/runtime/src/host.rs` (`helper_for_workspace`) a closed client is shut down and re-spawned, keeping a map keyed by canonical workspace.

## Calls, queues and limits

| Item | Value |
| --- | --- |
| Request queue / execution slots | 64 / 64 (`QUEUE_CAPACITY` semaphore) |
| Priority control queue and slots | 64 channel / 8 slots (audio stop controls only) |
| Handshake | 10 s |
| Per-write timeout | 10 s |
| Per-request deadline | 3605 s, then the call fails and a cancel is sent |
| Shutdown request | 5 s; graceful exit wait 5 s; forced-kill wait 2 s |
| Drop without `shutdown()` | `Shutdown` is queued; the child is killed after 8 s |
| Frame size | 8 MiB each way |

Call flow: reject if already cancelled or closed; acquire a slot (cancellable); encode once to check the size; register a pending reply; send on the queue; wait for the response. If the caller's `cancel` fires, or the deadline passes, a `cancel` command goes over the **control channel** so it cannot queue behind new tool calls (1 s send timeout). It requests cleanup and never replays the request. If the future is dropped, a `PendingGuard` attempts the same cancel and always removes the pending entry. If cancellation fires before dispatch, the helper's tombstone also prevents a late start.

`shutdown()` does not use an execution slot, because all 64 may be busy with the work it must cancel. It errors with `Native helper did not exit after shutdown` if the process is still alive after the kill path.

## Host lifecycle and admission (`runtime`)

From `crates/runtime/src/host.rs`: the host canonicalizes the workspace (`helper_audio_*` calls use the app data directory instead) and keeps one client per workspace, with at most **16** open sessions ("Finish current project actions before opening another tool session"). When a new workspace is requested and no local model activity is busy, helpers with `pending_count() == 0` are shut down first. All helpers are shut down concurrently when the application stops.

For operations with a `capability_demand` the host first takes a local `ResourceManager` lease, and the native side takes the shared pool exactly once; see [resources](resources.md).

## Security

- Absolute, canonicalized roots only; the config file cannot change workspace, artifacts or management roots.
- Cleared child environment (above); stdout is never used for diagnostics, so a stray log line cannot forge a response.
- Strict frames: `deny_unknown_fields`, version checks, 8 MiB cap, bounded diagnostics, bounded queues, a bounded cancellation tombstone set.
- Command names are restricted to `[A-Za-z0-9_]{1,128}` on both sides. The helper has no network listener.
- Configuration files are written with mode `0600`.

## Testing

`cargo test --locked -p xylo-helper -p xylo-helper-client` (not run while writing this page). Integration tests in `crates/helper/tests/`: `native_rpc.rs` (priority cancel overtaking an execute, concurrent request correlation with document export, cancellation reaching the native child while the helper stays available), `asset_install.rs`, `speech_pipeline.rs`. Unit tests: `crates/helper/src/server/tests.rs`, `crates/helper-client/src/tests/mod.rs` and `src/protocol/tests.rs`.

## Related crates

[capabilities](capabilities.md) supplies `CapabilityService`, `AssetManager` and `AudioService`; [runtime](runtime.md) owns the client instances; [resources](resources.md) provides admission; [core](core.md) and [engine](engine.md) are upstream callers of the tools.

## Gaps

I did not verify which capability tools exist beyond routing; see [capabilities](capabilities.md). I did not read the test bodies in detail.
