# Rust SDK and CLI protocol (`xylo-sdk`)

## Purpose and boundaries

`xylo-sdk` (`sdk/rust`, description "Stable Rust sessions over the Xylo CLI stdio protocol") is a **process client**: it spawns `xylo-cli` ([cli](cli.md)) and speaks the version-1 JSONL protocol over its stdin and stdout. It does not embed the engine or worker and exposes no credential, unlock or raw worker commands. Its only workspace dependency is `xylo-core` ([core](core.md)); the frame types come from `xylo_core::stdio`.

This page summarises the code and `sdk/protocol-v1.md`. Usage examples (Rust and Python), export JSONL, telemetry and CI notes live in `sdk/README.md` and are not repeated here. The WASM plugin SDK under `sdk/plugins` is a separate thing.

## Module map

| Path | Purpose |
| --- | --- |
| `sdk/rust/src/lib.rs` | `SessionOptions`, `Session`, `Control`, `EventStream`, `RunResult`, callback types, re-exports |
| `sdk/rust/src/shutdown.rs` | Bounded `close` logic |
| `sdk/rust/tests/shutdown.rs` | Shutdown and reaping tests |
| `sdk/protocol-v1.md` | Wire contract |
| `crates/core/src/stdio.rs` | Authoritative schema: `VERSION = 1`, `Input`, `Command`, `Event` |

Re-exports: `Event`, `PROTOCOL_VERSION`, `EngineEvent`, `InteractionAnswer`, `PermissionMode`, `QuestionAnswer`, `QuestionOption`, `ToolDefinition`, `Usage`, `UserQuestion`.

## Wire contract (`protocol-v1.md`, verified against `stdio.rs`)

Start `xylo-cli --input-format stream-json --output-format stream-json`. UTF-8 JSONL, one frame per line, each line newline-terminated and below `MAX_PROTOCOL_BYTES` (8 MiB). Input channel capacity is 32 commands (`session.rs`); output uses the engine's 256-event channel.

**Input** `{"version":1,"payload":{"command":...}}`. `Command` is tagged by `command`, snake_case, `deny_unknown_fields`:

| Command | Fields | Accepted |
| --- | --- | --- |
| `run` | `prompt` | Idle |
| `register_tool` | `definition` (`name`, `description`, `parameters`, `writes`) | Idle |
| `approve` | `action_id`, `allow` | Pending approval |
| `answer` | `action_id`, `answer` (`InteractionAnswer`) | Pending question or plan |
| `tool_result` | `action_id`, `result` (`{"Ok":v}` / `{"Err":"msg"}`) | Pending custom tool request |
| `cancel` | none | Active run |
| `shutdown` | none | Idle or active |

While a run is active, any other command cancels the run and fails the process ("A run is already active; ..."). While idle, only `run`, `register_tool` and `shutdown` are valid.

**Output** `{"version":1,"session_id","turn_id","sequence","payload":<EngineEvent>}`, with `exit_code` only on terminal frames. `Event::validate` requires version 1, an exit code in 0..=2 on terminal frames (`completed`, `error`, `cancelled`) and none on others. Exit codes: 0 success, 1 error or cancel, 2 refused. **There is one terminal frame per admitted run.** `sequence` increases by one per event across runs in a process; `session_id` is stable and `turn_id` is fresh per run.

**Custom tools**: names start with `sdk_`, must declare `writes=true`, unique, max 128 registrations; kept for later turns in the process, not persisted. **`tool_started` is not authorization to run client code**; only `custom_tool_requested` (`action_id`, `name`, validated `arguments`) is, and it is emitted after the broker's approval and journal steps. Read-only mode refuses custom tools, ordinary presets ask, full access allows. The CLI waits at most **5 minutes** for `tool_result`.

**Interactions**: `question_required` carries 1–4 questions, answered with `{"kind":"questions","answers":[{"selected":[0],"other":null}]}`. `plan_required` is answered with `approve_plan` (with `mode`), `keep_planning` (with `feedback`) or `reject`. Invalid, stale or duplicate replies grant no authority.

## Key API (`lib.rs`)

- `SessionOptions`: `executable` (empty means `xylo-cli` on `PATH`), `cwd`, `data_dir`, `model`, `permission_mode`, `allowed_tools`, `disallowed_tools`, `max_turns`, `resume`, `continue_latest`, `append_system_prompt`, `debug`, `environment`. The environment is inherited and extended. Options become separate arguments, **never a shell string**.
- `Session::new` spawns the child with piped stdin/stdout, **inherited stderr** and `kill_on_drop(true)`; dropping a `Session` kills the CLI.
- `Session::run(prompt)` collects a turn into `RunResult { session_id, turn_id, exit_code, text, usage, error, events }`. **It auto-denies approvals and rejects questions and plans**, so it cannot hang.
- `Session::stream(prompt)` returns an `EventStream`; call `next()` until `None`. Pending approvals stay pending until answered through `Control`.
- `Control` (cloneable, from `Session::control()`): `approve`, `answer`, `cancel`. It stays usable while a stream borrows the session. `Session::approve` delegates to it.
- `register_tool(definition, callback)`: only when idle; requires `sdk_` prefix, `writes=true`, no duplicate. Callbacks run via `spawn_blocking`; a panic or `Err` becomes a failed `tool_result`.
- `on_event(hook)`: each hook is called once per event read.
- `close()` returns the CLI's exit code (see below).

## Client-side validation (`EventStream::next`)

Each frame is read with an 8 MiB + 1 byte bound and must end with a newline. It then decodes, passes `Event::validate`, must have `sequence == previous + 1` ("CLI event sequence is out of order") and an unchanged `session_id`. A closed stdout before the terminal event fails with "CLI closed its output before the run finished". Outgoing commands are also bounded below the frame limit. Only one run may be active (`active` flag); prompts must be non-empty.

## Shutdown (`shutdown.rs`)

`close` sends `shutdown`, closes stdin and discards unread output without running hooks or callbacks. After **5 s** (`GRACE_PERIOD`) it calls `start_kill` and waits up to a further **5 s** (`REAP_PERIOD`) to reap. It returns `status.code()` or 1 if signalled. The process exit code is the maximum status across runs.

## Concurrency and errors

All methods are `async` over Tokio. Stdin is behind an `Arc<Mutex<Option<ChildStdin>>>`, so `Control` clones serialise writes. Errors are `anyhow::Result`; protocol violations end the stream with an error rather than being skipped. Stderr from the CLI is not captured.

## Security

- Tools and permissions are fixed by process options; **an explicit approval authorizes one broker action and cannot widen the mode or tool filters**.
- Custom tool callbacks are trusted caller code and run outside the engine sandbox; their results keep untrusted-output provenance.
- Telemetry is configured through `SessionOptions::environment` (see `sdk/README.md`).

## Testing

`cargo test --locked -p xylo-sdk` (not run when writing this page); `cargo test --locked -p xylo-cli` includes `tests/sdk.rs`, which drives the real CLI through the SDK.

## Related

[cli](cli.md), [core](core.md) (`stdio`, `EngineEvent`), [engine](engine.md) (broker and `RunOptions`), [runtime](runtime.md).
