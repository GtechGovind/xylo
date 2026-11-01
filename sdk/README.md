# Headless sessions

Build `cargo build --locked -p xylo-cli`, then run:

```sh
xylo-cli -p "list files" --output-format json --cwd /path/to/project
printf 'Explain this project' | xylo-cli --output-format text
xylo-cli -p "continue" --resume CONVERSATION_UUID --output-format stream-json
```

Configure a provider in the desktop first and close it before using the same profile from the CLI. The CLI uses the same profile database, secret vault, permission presets, standing project approvals, sandbox and journal as the desktop. Only one process may own a profile; use `--data-dir` for concurrent isolated profiles. `--resume` and `--continue` preserve the saved workspace. `--model` changes the configured provider's model, and `--append-system-prompt` appends instructions after the normal system prompt. `--allowed-tools` restricts advertised **and dispatched** tools; `--disallowed-tools` takes precedence, including through `call_tool` and delegation. Lists are comma-separated exact names or the documented Read/Write/Edit/Glob/Grep/Bash aliases. `--max-turns` bounds root model rounds to 1–400. An exhausted budget uses the engine's final-round behavior and never silently starts another round.

`--permission-mode` accepts `ask_for_approval`, `approve_for_me`, `full_access`, or `read_only`. The broker decides each action, with saved standing approvals applied at admission. A one-shot CLI never silently approves a pending request: it denies, explains the refusal on stderr and exits **2**. Other errors or cancellation exit **1**, successful runs exit **0**. In persistent sessions, each terminal event carries the turn's exit code; process exit is the maximum status of its runs. `Session::run` also denies pending approvals by default; use `Session::stream` and `Control::approve` for explicit decisions.

`--export /path/chat.md` writes the full persisted conversation as Markdown and adjacent `chat.jsonl`, using paginated history rather than only the visible UI page. Desktop Export writes the same pair. Export JSONL is a separate version-1 message schema: `{ "version": 1, "type": "message", "session_id": "UUID", "message": ChatMessage }`. Embedded newlines remain escaped within each record.

## Rust SDK

The public `xylo-sdk` crate in `sdk/rust` is a process client. It does not embed the worker or expose private worker commands or credential material. The API consists of `Session::new`, `Session::run`, `Session::stream`, `Session::approve`, `Session::register_tool`, `Session::on_event`, and `Session::close`. A cloned `Control` can answer approvals or cancel while the session is borrowed by a stream. Consume each stream through its terminal event before starting another run. After abandoning a stream, `close` drains unread output without invoking hooks or tool callbacks, requests shutdown, and waits up to five seconds before a forced stop with a further five-second reap deadline. Dropping the session kills its child. Environment and process configuration are explicit in `SessionOptions`. Supply `executable` when the CLI is not on PATH.

```rust,no_run
use xylo_sdk::{Session, SessionOptions};

# async fn example() -> anyhow::Result<()> {
let mut session = Session::new(SessionOptions::default()).await?;
let result = session.run("list files").await?;
println!("{}", result.text);
assert_eq!(result.exit_code, 0);
session.close().await?;
# Ok(())
# }
```

Custom tools register a JSON Schema plus a callback, require an `sdk_` name, and declare `writes=true`: external client code cannot claim the engine's sandbox guarantees. Callbacks execute only upon `custom_tool_requested`, after authorization, and their results pass through the ordinary journal. Tool errors are returned to the model; a denied call never invokes the callback. Hooks observe every event once. A panicking callback becomes a failed tool result. Callback implementations are trusted caller code and must bound their own work.

The WASM plugin SDK under `sdk/plugins` is unchanged.

## Python over the same protocol

```python
import json
import subprocess

process = subprocess.Popen(
    ["xylo-cli", "--input-format", "stream-json",
     "--output-format", "stream-json"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
)

def send(command):
    process.stdin.write(json.dumps({"version": 1, "payload": command}) + "\n")
    process.stdin.flush()

send({"command": "run", "prompt": "list files"})
for line in process.stdout:
    frame = json.loads(line)
    assert frame["version"] == 1
    event = frame["payload"]
    if event["event"] == "approval_required":
        send({"command": "approve", "action_id": event["action_id"], "allow": False})
    if "exit_code" in frame:
        print(event)
        break
send({"command": "shutdown"})
process.stdin.close()
raise SystemExit(process.wait())
```

See [the wire contract](protocol-v1.md) for all commands and lifecycle rules. Offline automation can explicitly set `XYLO_EVAL_SCRIPT` to a JSON `xylo_evals::scripted::Script`; the CLI swaps in that existing deterministic provider under the real engine guards. This opt-in never contacts the scripted endpoint. CI runs the checked-in list-files script.

## Logs and telemetry

CLI and desktop `--debug` write daily owner-only files in `<data-dir>/logs`, retaining seven UTC calendar days. `RUST_LOG` overrides the default `info`/`debug` filter. Standard output remains reserved for CLI results.

Telemetry is **off by default**. Opt in independently with `OTEL_TRACES_EXPORTER=otlp` and `OTEL_METRICS_EXPORTER=otlp`; `OTEL_SDK_DISABLED=true` overrides both. The supported OTLP transport is HTTP/JSON (`OTEL_EXPORTER_OTLP_PROTOCOL=http/json`), with a default collector at `http://localhost:4318`. Unsupported transports fail explicitly at startup. Generic and per-signal `OTEL_EXPORTER_OTLP[_TRACES|_METRICS]_{ENDPOINT,HEADERS,TIMEOUT}` are honored; a generic endpoint gets `/v1/traces` or `/v1/metrics` appended, a signal endpoint is used verbatim. Timeouts are milliseconds, bounded to 1–30000. `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES` set resource attributes. Export is bounded and never blocks model/tool execution; unavailable collectors may drop telemetry, and shutdown waits at most five seconds.

Exported spans contain operation names and timing, without prompts, tool arguments, results or credentials. Metrics are `xylo.tokens` with direction, `xylo.tool.duration` in seconds, and `xylo.cost` in USD. Providers currently do not report billed cost through `Usage`; cost is an **estimate**, emitted only when both `XYLO_OTEL_INPUT_USD_PER_MILLION` and `XYLO_OTEL_OUTPUT_USD_PER_MILLION` are explicitly configured to nonnegative rates. Token counts remain real provider usage (or deterministic scripted usage). Rates apply to the process's selected provider; use a separate process for differently priced models.

## CI and releases

[CI](../.github/workflows/ci.yml) runs formatting, strict Clippy, workspace tests, evaluation tests, the scripted CLI acceptance command and RustSec audit on macOS and Linux. [Release packaging](../.github/workflows/release.yml) builds the binaries and resources listed in `packager.json`, including `xylo-cli`, for native macOS, Linux and Windows runners. It uploads packages for review; it does not publish an update feed.

Apple signing and notarization require `APPLE_CERTIFICATE` (base64 PKCS#12), optional `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` (app-specific), and `APPLE_TEAM_ID`. Complete credentials configure a temporary signing keychain and let cargo-packager sign and notarize. Incomplete credentials produce an explicit warning and unsigned packages. `MINISIGN_KEY` is the base64 key generated by `cargo packager signer generate`; `MINISIGN_PASSWORD` is its password, or empty for an unencrypted key. When supplied, packager generates update signatures; when absent, the workflow warns and skips them. Tagged builds require the tag version to match `packager.json`. Owner-supplied signing keys and a trusted update-feed host remain release configuration decisions.
