# Headless CLI (`xylo-cli`)

## Purpose and boundaries

`xylo-cli` is the headless frontend (crate description: "Headless Xylo sessions and versioned JSONL stdio protocol"). It runs prompts through the same profile database, secret vault, permission presets, sandbox and action journal as the desktop app, but **never starts the desktop host, a worker process or a window**. It builds the engine in-process (`AgentEngine`) after opening the profile through `xylo_runtime::headless::Profile` ([runtime](runtime.md)).

- Depends on `xylo-core` ([core](core.md)), `xylo-runtime`, `xylo-engine` ([engine](engine.md)), `xylo-services` ([services](services.md)) and `xylo-evals` (scripted offline provider). `xylo-sdk` is a dev-dependency used by `tests/sdk.rs`.
- **Stdout carries only the requested output format**; diagnostics, refusals and errors go to stderr (`main.rs` module doc).
- The wire contract for `stream-json` is documented in [sdk](sdk.md); user-facing usage examples are in `sdk/README.md`.

## Module map

| Path under `crates/cli` | Purpose |
| --- | --- |
| `src/main.rs` | Entry point: `--help`/`--version`, stdin prompt, profile paths, telemetry init, exit status |
| `src/options.rs` | `Options::parse`, `Format`, the `HELP` text; all validation before any profile is opened |
| `src/session.rs` | `serve`: conversation resolution, turn loop, approvals, custom tools, output rendering, export |
| `src/session/tests.rs` | Unit tests |
| `tests/cli.rs`, `refusal.rs`, `sdk.rs`, `telemetry.rs` | Process-level integration tests |

## Flags (`options.rs`)

Every flag except `--debug` and `--continue` requires a value. Unknown flags fail with "Unknown option ...; use --help".

| Flag | Behavior |
| --- | --- |
| `-p`, `--print PROMPT` | One prompt; only one allowed. Without it (and without stream input) the prompt is read from stdin |
| `--output-format` | `text` (default), `json`, `stream-json` |
| `--input-format stream-json` | Persistent JSONL commands on stdin; the only accepted value |
| `--model MODEL` | Overrides the configured provider's model; must be non-empty |
| `--permission-mode MODE` | Parsed as `PermissionMode` (snake_case): `ask_for_approval`, `approve_for_me`, `full_access`, `read_only`, `plan` |
| `--allowed-tools`, `--disallowed-tools` | Comma-separated nonempty names; disallow wins |
| `--max-turns N` | Integer in **1..=400** |
| `--resume ID` / `--continue` | Resume by UUID / most recent conversation; mutually exclusive |
| `--cwd PATH` | Workspace for a new conversation; canonicalized, must be an existing directory (default: current dir) |
| `--append-system-prompt TEXT` | Appended to the system prompt |
| `--data-dir PATH` | Isolated profile; a relative path is joined to the current directory |
| `--export PATH.md` | Export after each run; the path must have an `.md` extension |
| `--debug` | Debug logging into `<data-dir>/logs` |
| `--help`/`-h`, `--version` | Only recognized as the sole argument. Version prints `xylo-cli <version> (stdio protocol 1)` |

Cross-flag rules: `--input-format stream-json` requires `--output-format stream-json` and cannot be combined with `--print`.

Permission mode: `Profile::permission_mode` returns the flag if given, otherwise the stored `settings.permissions`. `sdk/README.md` and the `HELP` text differ: the README lists four modes, while the parser and `HELP` also accept `plan`.

## Input and output formats

- **`text`**: prints the final answer to stdout (if nonempty); an error message goes to stderr.
- **`json`**: one object per run on stdout: `{"version":1,"session_id","turn_id","text","usage","error","exit_code"}`.
- **`stream-json`**: one JSONL `Event` frame per engine event (`version`, `session_id`, `turn_id`, `sequence`, `payload`, and `exit_code` on terminal frames only). `sequence` is a process-wide counter starting at 1.
- **Prompt input**: `--print`, otherwise all of stdin, capped at `xylo_core::MAX_PROTOCOL_BYTES` (8 MiB); larger input fails with "Prompt exceeds input limit". Whitespace-only prompts fail with "The prompt must not be empty".
- **Stream input**: JSONL `Input` frames (`{"version":1,"payload":{"command":...}}`). Each line must end with a newline and fit 8 MiB. A frame error (bad version, malformed, unknown field, truncated) ends the reader and fails the process with exit 1. Commands and rules: [sdk](sdk.md).
- Frames larger than the limit are refused on output ("Output exceeds protocol frame limit").

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Run completed |
| 1 | Error, cancellation, or a process-level failure (bad options, profile locked, protocol error; printed as `xylo-cli: <error>` on stderr) |
| 2 | **Refused**: a permission was denied (policy refusal via `on_refusal`, denied approval, rejected question or plan, or stream input closed with approvals pending) |

Per run, `refused` turns a would-be 0 or 1 into **2**. In a persistent session the process exit status is the **maximum** run status. A run that ends without a terminal event is reported as an internal error. Cancellation caused by a refusal reports "An action required permission and was refused".

## Permission behavior

- Mode comes from `--permission-mode`, else saved settings, and is passed to the `ToolSession`'s `PermissionCell` and to the turn request.
- **A non-stream run never approves silently**: on `ApprovalRequired` it calls `decide_with(action_id, false, false)`, prints `Refused <tool>: <reason>. Use a permitted mode, saved approval rule, or a stream-json approval client.` to stderr, cancels the run and exits 2. Saved standing approvals still apply at admission.
- With stream input, approvals wait for an `approve` command. After `shutdown` or EOF, pending approvals and interactions are denied/rejected and the run is cancelled.
- `question_required` and `plan_required` wait for `answer` in stream mode; otherwise the engine's non-interactive default declines (`RunOptions.interactive` is true only with stream input).

## Sessions, resume and export

- `Profile::open` takes an exclusive lock on `<data>/host.lock`; if the desktop or another CLI owns the profile it fails with "Xylo is already running for this profile; choose another --data-dir". It then opens the store and runs `recover_interrupted`.
- The data directory is `AppPaths::discover()` or, with `--data-dir`, `AppPaths::at(...)`.
- A new conversation is titled "Headless chat" and stores the canonical `--cwd`. `--resume ID` fails with "The requested conversation does not exist"; `--continue` takes the most recent conversation (`store.conversations(1)`) or fails with "There is no conversation to continue".
- **A resumed conversation keeps its original workspace**: if `--cwd` is passed explicitly and differs, the CLI bails.
- One conversation per process. Each run gets a fresh `turn_id`, the user turn is registered with `register_user_turn`, and history persists in the store, so a new process resumes with `sequence` restarting at 1.
- `--export PATH.md` calls `xylo_runtime::conversation_export::export` after each run (Markdown plus adjacent JSONL; schema in `sdk/README.md`).
- Custom tools (`register_tool`) are accepted only while idle, are limited to **128**, names must be unique, and persist for later turns in the process only. The callback reply wait is **300 s** and observes cancellation (`custom_handler`).

## Offline eval script

If env `XYLO_EVAL_SCRIPT` names a JSON `xylo_evals::scripted::Script`, the CLI replaces the provider with `ScriptedProvider` (endpoint `https://scripted.invalid/v1`, model `scripted`, no credential lookup, no network) under the **real engine, broker and journal**. The script is read per run. CI runs `.github/fixtures/cli-list-files.json` with `-p "list files" --output-format json` and asserts `exit_code == 0` and the expected text (`.github/workflows/ci.yml`). Script semantics: `crates/evals/src/scripted.rs`.

## Telemetry and logs

`Observability::init(<data>/logs, --debug)` runs before the profile opens; each engine event is passed to `telemetry.event(...)`. OTLP export is opt-in via `OTEL_*` variables (details in `sdk/README.md`).

## Testing

`cargo test --locked -p xylo-cli` (not run when writing this page). Integration tests cover JSON output and identity, option rejection before the profile opens, protocol version rejection, stream EOF with export, stdin prompt with `--continue`, relative `--data-dir`, refusal exit 2 for read-only and non-interactive approvals, SDK interoperability and OTLP export.

## Related

[core](core.md) (`stdio`, `PermissionMode`), [engine](engine.md), [runtime](runtime.md) (`headless::Profile`), [services](services.md), [sdk](sdk.md), [worker](worker.md) (the desktop's separate process path, not used by the CLI).
