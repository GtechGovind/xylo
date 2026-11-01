# CLI stream-json protocol, version 1

Start `xylo-cli --input-format stream-json --output-format stream-json`. The session reads and writes UTF-8 JSONL. Every line ends with a newline and must fit the core `MAX_PROTOCOL_BYTES` limit (8 MiB, including newline). Unknown protocol versions, malformed JSON, truncated lines, unsupported commands and misplaced commands are rejected, with an error on stderr and process exit 1. Stdout contains engine event frames only. Inputs are bounded by a 32-command channel and outputs by the engine's 256-event channel; producers receive backpressure.

The authoritative Rust schema is [`xylo_core::stdio`](../crates/core/src/stdio.rs); the payload event schema is [`EngineEvent`](../crates/core/src/lib.rs). Schema 1 is explicit and is independent of the private worker protocol. A breaking frame or semantic change requires a new schema version.

Input frame:

```json
{"version":1,"payload":{"command":"run","prompt":"list files"}}
```

Commands:

| Command | Fields | When accepted |
| --- | --- | --- |
| `run` | `prompt`: nonempty string | Idle |
| `register_tool` | `definition`: `{name, description, parameters, writes}` | Idle, before next run |
| `approve` | `action_id`: UUID, `allow`: boolean | Matching pending approval |
| `answer` | `action_id`: UUID, `answer`: tagged interaction reply | Matching `question_required` or `plan_required` |
| `tool_result` | `action_id`: UUID, `result`: `{"Ok": any JSON}` or `{"Err":"message"}` | Matching `custom_tool_requested` |
| `cancel` | None | Active run |
| `shutdown` | None | Idle or active; cancels active work |

No credential, unlock, raw worker or arbitrary process commands are exposed. Modes and tool constraints are process options. Only an explicit answer to a pending plan may select a continuation mode; tool constraints remain in force. Registered names start with `sdk_`, contain ASCII letters/digits/underscores, are unique, and must declare `writes=true`. JSON Schema parameters are validated by the existing broker. Custom tools are treated as external executable writes: read-only refuses them, ordinary presets ask, and full-access allows them. A maximum of 128 registrations is accepted. Registrations remain available for subsequent turns in the process and are not persisted. Explicit approvals authorize one broker action and cannot widen the mode or tool filters. The CLI denies unanswerable approvals; a custom callback has a five-minute response deadline and observes cancellation.

Output frame:

```json
{"version":1,"session_id":"UUID","turn_id":"UUID","sequence":1,"payload":{"event":"started"}}
```

`sequence` starts at 1 and increases exactly once per event across runs in the process. Conversation identity remains stable; each run gets a fresh `turn_id`. `--resume` or `--continue` opens the durable checkpoint and history; a new process starts its sequence at 1.

The `payload` is exactly one serialized `EngineEvent`, with the discriminant `event`. Deltas use `{"event":"delta","text":"..."}`. Tool lifecycle events are `tool_started`, optional `approval_required`, optional `custom_tool_requested`, and `tool_finished`. **`tool_started` is not authorization to execute external client code**. Only `custom_tool_requested` asks a client to execute its previously registered tool and answer with `tool_result`. It carries `action_id`, `name`, and the full validated `arguments`, and is emitted after the broker's approval and journal boundaries. All external tool results retain the engine's untrusted-output provenance.

Terminal payloads are `completed` (`text` and `usage`), `error` (`message`, `retryable`, `kind`), or `cancelled`. Terminal frames additionally contain `exit_code`: 0 success, 1 error/cancellation, 2 refused. The stream then accepts the next run. There is one terminal frame per admitted run; startup and malformed-input failures are process errors on stderr. Use the terminal status even if a model eventually describes a refused tool call in a completed response. Process exit aggregates the maximum status across runs.

A client must keep stdin open while answering approvals and custom calls, then send `shutdown` and close it when finished. Use `cancel` to settle an active run without closing the session. A one-shot `--print` with stream-json output needs no input frames and auto-denies pending approvals.

Questions emit `question_required` with `action_id` and 1–4 `questions`. Answer with `{"kind":"questions","answers":[{"selected":[0],"other":null}]}` in question order; selected entries are zero-based option indices. Each question automatically accepts Other free text. Plan cards emit `plan_required` with `action_id` and `plan`; replies are `{"kind":"approve_plan","mode":"ask_for_approval"}`, `{"kind":"keep_planning","feedback":"..."}`, or `{"kind":"reject"}`. Rejection is also accepted for questions. Invalid, stale, duplicate, or cancelled replies grant no authority. Cancellation settles pending interactions. The Rust SDK `Control::answer` supports streaming decisions; `Session::run` declines unanswered questions and plans automatically. Non-streaming CLI and default engine RunOptions decline without waiting.
