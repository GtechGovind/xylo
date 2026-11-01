# Application contracts (`xylo-core`)

## Purpose and boundaries

`xylo-core` holds the application-owned data contracts shared by the desktop host, worker, engine, CLI and SDK. Its crate docs state: "Application-owned contracts. Community engine types must not cross this boundary."

Dependencies (`crates/core/Cargo.toml`) are only `serde`, `serde_json`, `uuid`, `async-trait`, `anyhow`, `tokio` and `tokio-util`. **It depends on no other Xylo crate**, performs no file or network I/O and owns no persistence; storage lives in [services](services.md) and orchestration in [runtime](runtime.md).

Direct dependents (from `Cargo.toml` files): `capabilities`, `cli`, `desktop`, `engine`, `evals`, `runtime`, `scheduler`, `services`, `worker` and `sdk/rust`.

## Module map

| Path under `crates/core/src` | Purpose |
| --- | --- |
| `lib.rs` | Provider, settings, permission, worker protocol, engine events, conversation, memory and connector contracts; the `EngineBackend`, `ProviderAdapter` and `MemoryBackend` traits |
| `branding.rs` | `APPLICATION_NAME` (`Xylo`), `APPLICATION_ID` (`com.qurkos.xylo`), `CREDENTIAL_SERVICE` (same as the id) |
| `attachments.rs` | `ImageAttachment` references and limits |
| `documents.rs` | Document snapshot, edit and save-receipt types |
| `environments.rs` | SSH and container execution targets, operations and receipts |
| `image_generation.rs` | Image generation provider, request and receipt types |
| `interaction.rs` | Questions, plan approval and answers |
| `mcp.rs` | MCP elicitation form types and `validate_mcp_form` |
| `plugins.rs` | Native plugin manifest, budgets and catalog types |
| `pricing.rs` | Token price table and cost estimation |
| `provider_controls.rs` | Claude adaptive-thinking and native reasoning-effort rules; `AppSettings::turn_provider` |
| `review_prompts.rs` | `ReviewKind`, `review_prompt` (text in `crates/core/prompts/`) |
| `steering.rs` | Durable mid-turn guidance request and receipt |
| `stdio.rs` | Public headless CLI/SDK wire protocol (`xylo_core::stdio`) |
| `tests/`, `*/tests.rs` | Unit tests |

## Key types and public API

### Provider configuration (`lib.rs`, `provider_controls.rs`)

- `ApiFamily`: `OpenAiChat`, `OpenAiResponses`, `Anthropic`, `Gemini`, `Azure`, `Bedrock`, `Vertex` (serialized `snake_case`).
- `ProviderConfig`: connection name, family, `endpoint`, `model`, `credential_ref` (a reference, not a secret), `context_tokens`, `output_tokens`, `timeout_seconds`, optional `served_context_tokens`, reasoning fields, `discovery_url`, `managed_local`, `headers`, `fallback_model`. `Default` is a local model: `http://127.0.0.1:8080/v1`, family `OpenAiChat`, 8192 context, 2048 output, 120 s timeout. `ProviderConfig::unset()` has empty name and endpoint and is never valid for a turn.
- `validate_connection()` fails unless the context is at least 1024 tokens and `output_tokens < context_tokens`, the timeout is 1..=3600 seconds, and the endpoint starts with `https://`, `http://127.0.0.1:` or `http://localhost:`. `validate()` additionally requires a non-blank model.
- `context_window()` is the smaller of `context_tokens` and a positive `served_context_tokens`.
- `supported_reasoning_efforts()` is conservative: catalog `reasoning_options` are used when present (at most 16, deduplicated, each passing `valid_reasoning_option`: 1..=32 bytes, lowercase ASCII start, then lowercase, digits, `_`, `-`). Otherwise only known OpenAI model names and native Claude/Gemini rules apply; unknown names yield an empty list. `effective_reasoning_effort()` drops a stored selection the model does not support.
- `adaptive_thinking()` and `default_adaptive_thinking()` match Anthropic model-name prefixes (`provider_controls.rs`).
- `AppSettings::turn_provider()` applies the global `fallback_model` without changing the saved choice.

### Settings (`lib.rs`)

`AppSettings` holds the active `provider`, saved `providers`, `permissions`, `theme` (default `"system"`), `last_workspace`, `local_context`, `telemetry_export` (default `false`), `ui: UiPreferences`, `secret_storage: SecretStorage`, `pricing_overrides`, `output_style` (default `"default"`), `status_line_command`, `fallback_model`, `web_search: WebSearchSettings` and `permission_rules_project`. `model_ready()` is true when the active provider validates. `remember_model()` replaces a saved entry with equal endpoint, family, model, credential reference, headers and discovery URL, otherwise appends; it ignores an empty model.

`WebSearchSettings` stores a `WebSearchBackend` (`Disabled` default, `Brave`, `Tavily`, `Searxng`), an endpoint and a `credential_ref` only, never a credential value. `SecretStorage` is `LocalFile` (default) or `OsKeychain`.

### Permissions (`lib.rs`)

`PermissionMode`: `AskForApproval` (default), `ApproveForMe`, `FullAccess`, `ReadOnly`, `Plan`. `allows_writes()` is false for `ReadOnly` and `Plan`; `sandboxed()` is false only for `FullAccess`. `ALL`, `label()` and `description()` give the single product presentation.

### Worker protocol (`lib.rs`)

Constants: `PROTOCOL_VERSION = 2`, `MAX_PROTOCOL_BYTES = 8 MiB`, `EVENT_QUEUE_CAPACITY = 256`. `now_ms()` returns Unix milliseconds (0 if the clock is before the epoch).

- `Envelope<T>` carries `version`, `request_id`, optional `session_id`, `sequence` and `payload`. `Envelope::validate()` **rejects any version other than `PROTOCOL_VERSION`**.
- `WorkerCommand` (tagged by `command`, `snake_case`) is the host-to-worker set: `Hello`, `Run`, `RunAuthorized`, `ProviderCredentialUpdated`, `Cancel`, `SteerDurable`, `Approve { action_id, allow, remember }`, `SetPermissions`, `Unlock`, `Shutdown`, `KillJob`, `RespondMcpElicitation`, `Answer`.
- `EngineEvent` (tagged by `event`) is the worker-to-host stream: `Ready`, `Started`, `Delta`, `Status`, `HarnessNote`, `Steering`, `ToolStarted`, `ApprovalRequired`, `ToolFinished`, `Phase`, `ContextUsage`, `TitleUpdated`, `Completed`, `Cancelled`, `Error`, `ModelMetadata`, `ThinkingDelta`, `CustomToolRequested`, `ShellJobUpdated`, `WorkspaceChanged`, `McpElicitation`, `McpElicitationClosed`, `QuestionRequired`, `PlanRequired`, plus `ProviderCredentialRequested`, which the source documents as an internal request intercepted by the host and never forwarded to the UI. `ToolStarted`, `ApprovalRequired` and `ToolFinished` carry an optional `parent_id` for helper-agent calls.
- Supporting types: `TurnRequest`, `Usage`, `TurnPhase` (`Preparing`, `Thinking`, `Responding`, `RunningTools`, `WaitingForApproval`, `Compacting`, `Retrying { attempt, delay_ms }`, `Delegating`), `ContextUsage` (`used()` = system + tools + messages + summary), `ErrorKind` (`Authentication`, `RateLimited`, `Provider`, `ContextOverflow`, `Stalled`, `EmptyResponse`, `Internal` default), `NoteKind`, `RateLimits`, `ShellJob` (bounded output tail; the full log is at `log_path`).
- `WorkerCredential(Option<String>)` has a manual `Debug` that prints `WorkerCredential([REDACTED])`. Its doc comment forbids placing it in settings, turns, checkpoints, event payloads, command-line arguments or environment variables.

### Public headless protocol (`stdio.rs`)

One UTF-8 JSON object per line, bounded by `MAX_PROTOCOL_BYTES`; `stdio::VERSION = 1` (separate from the worker protocol). `Input { version, payload: Command }` with `Command` = `Run`, `Approve`, `RegisterTool`, `ToolResult`, `Cancel`, `Shutdown`, `Answer` (`deny_unknown_fields`). No worker or credential commands are exposed. `Event` wraps an `EngineEvent` with `session_id`, `turn_id`, `sequence` and an optional `exit_code`. `Event::validate()` requires the exact version; terminal events (`Completed`, `Cancelled`, `Error`) **must** carry an exit code in 0..=2 (0 success, 1 error or cancellation, 2 refused), and non-terminal events **must not** carry one.

### Conversations and memory (`lib.rs`)

`Conversation` (title, workspace, `pinned`, `archived`, `TitleSource` of `Prompt`/`Generated`/`User`), `ConversationCursor`/`ConversationPage` (keyset pagination on `updated_at` with id tiebreak), `ChatMessage` with `MessageStatus` (`Complete`, `Interrupted`, `Failed`, `Truncated`), `HistoryPage` (opaque `before` cursor in append order). `MemoryRecord` carries a `MemoryOrigin` ordered most to least trusted: `UserTyped`, `UserApproved`, `AgentInferred`, `ToolOutput`, `WebContent`. `untrusted()` is true for the last two; `tier()` returns 0 (both user origins), 1, 2, 3 and is used for corrections. The origin is set by the application, never by the model.

### Traits (`lib.rs`)

- `EngineBackend` (`name`, async `run(request, events: mpsc::Sender<EngineEvent>, cancel: CancellationToken)`).
- `ProviderAdapter` (async `models(&ProviderConfig)`).
- `MemoryBackend` (async `remember`, `recall(scope, query, limit)`, `forget`).

Also present: `ConnectorConfig` (with `trusted` read-only-hint flag), `ToolDefinition`, `ToolObservation`, `TeamTask`.

### Domain contracts in smaller modules

- **Attachments** (`attachments.rs`): `MAX_ATTACHMENTS_PER_TURN = 4`, `MAX_IMAGE_BYTES = 8 MiB`. `ImageAttachment` (`deny_unknown_fields`) holds id, session, name, media type, `sha256`, byte size, dimensions, `AttachmentSource` (`Picker`, `Drop`, `PastedFile`, `Clipboard`, `Generated`) and import time. The module doc: bytes and native paths never cross chat protocols.
- **Documents** (`documents.rs`): `DocumentSource` (path, `revision`, bytes, `DocumentKind` `Text`/`Docx`/`Spreadsheet`), `DocumentSnapshot`/`DocumentContent`, `DocumentEdit` (`Text`, `Paragraph`, `Cell`), `DocumentCellValue`. `DocumentOpenRequest::default()` uses `limit` 100 paragraphs, 64 rows and 16 columns. A save names an `operation_id` and the source revision; `DocumentSaveReceipt` states are `Prepared`, `Committed`, `Cancelled`, `Failed`, `OutcomeUnknown`.
- **Environments** (`environments.rs`): `EnvironmentTarget` is `Ssh` (executable, host, user, port, `known_hosts`, optional identity file) or `Container` (Docker or Podman; `Existing` or `Managed` target). `EnvironmentRecord.revision` and the `expected_revision` on every request bind an action to one stored profile. `connected` is an explicit selection verified by a live probe, not a reachability claim. Receipts report `EnvironmentOperationState` (including `OutcomeUnknown`) and `EnvironmentOutputArtifact` capture metadata (`sha256`, stored vs observed bytes, `EnvironmentCaptureReason`).
- **Image generation** (`image_generation.rs`): protocols `OpenAiImages` and `GeminiGenerateContent`; `ImageGenerationReceipt` states `Prepared`, `Submitted`, `Succeeded`, `Failed`, `Cancelled`, `OutcomeUnknown` and a `dispatch_started` flag.
- **Interaction** (`interaction.rs`): `UserQuestion`, `QuestionOption`, `QuestionAnswer { selected, other }`, `InteractionAnswer` (`Questions`, `ApprovePlan { mode }`, `KeepPlanning { feedback }`, `Reject`). `validate_text()` rejects blank or over 8192-byte `KeepPlanning` feedback and `other` answers over 8192 bytes.
- **MCP forms** (`mcp.rs`): `McpFormRequest`, `McpFormAction` (`Accept`, `Decline`, `Cancel`). `validate_mcp_form(schema, content)` checks a flat primitive form: `properties` must exist, the response must be an object, every `required` key present, no unknown fields, and each value must match `string` (`minLength`/`maxLength` by chars), `number`/`integer` (`minimum`/`maximum`; integers need no fraction) or `boolean`, and any `enum`. Other property types are invalid.
- **Plugins** (`plugins.rs`): `NativePluginManifest` (`deny_unknown_fields`), `InstalledPlugin`, `PluginCatalog`, `PluginInspection`. `PluginBudgets::default()` is 32 MiB memory, 10,000,000 fuel, 15 s timeout, 256 KiB input and 256 KiB output.
- **Pricing** (`pricing.rs`): `ModelPrice` (USD per million tokens: input, output, cache read, cache write). `valid()` requires finite rates in 0..=1,000,000. `builtin(model, input_tokens)` is a match table with some context-tier prices and returns `None` for unknown models (never guesses). `estimate(calls, overrides)` prefers a valid user override, else the built-in price; unpriced calls only increment `CostEstimate.unpriced_calls`. Cache read and write tokens are clamped to the call's input tokens. Tool fees, cache storage and discounts are excluded.
- **Steering** (`steering.rs`): `MAX_STEERING_BYTES = 16 KiB`; `SteeringState` `Queued`, `Applied` (added to agent context, not a claim of model compliance), `Cancelled`.
- **Review prompts** (`review_prompts.rs`): `review_prompt(ReviewKind::General | Security)` returns the embedded `prompts/review.md` or `prompts/security_review.md` text without trailing whitespace; it starts no turn and grants no tool access.

## Data and persistence

None. The crate defines serde shapes only (`snake_case` enums, internally tagged `command`/`event`/`kind` enums, `deny_unknown_fields` on most request types, `#[serde(default)]` on later-added fields so older stored settings still load). Stores and schemas are in [services](services.md).

## Concurrency and error behavior

- Only `EngineBackend::run` is concurrent: it streams `EngineEvent`s into a `tokio::sync::mpsc::Sender` and observes a `tokio_util` `CancellationToken`. Everything else is synchronous and pure.
- Validation returns `anyhow::Result` or `Result<(), String>` / `Result<(), &'static str>`; failures are explicit and nothing is silently coerced: wrong protocol versions are refused (`Envelope::validate`, `Input::validate`, `Event::validate`), unknown models get no price and no reasoning options, and unsupported reasoning selections are not sent.

## Security and privacy notes

- `WorkerCredential` never prints its value in `Debug` (tested in `crates/core/src/tests/mod.rs`).
- Settings and web-search configuration hold credential **references** (`credential_ref`), not values.
- Provider endpoints must be HTTPS or loopback HTTP.
- Attachments, generated images and documents are referenced by id, hash and revision; bytes and native paths stay out of chat protocols.
- `MemoryOrigin` records how much untrusted content shaped a fact and is set by the application, never by a model.
- `telemetry_export` defaults to `false`.

## Configuration

No environment variables are read. Defaults are in `AppSettings::default()`, `ProviderConfig::default()`, `UiPreferences::default()` and `PluginBudgets::default()`.

## Testing

Unit tests sit in `crates/core/src/tests/` (credential redaction, protocol version, provider validation, permission modes, reasoning efforts, provider controls, workspace panel preferences) and in per-module `tests.rs` files (`mcp`, `pricing`, `stdio`).

```sh
cargo test --locked -p xylo-core
```

## Related crates

- [Services](services.md): stores, vault, providers and connectors that persist and execute these contracts.
- [Runtime](runtime.md): the desktop host that drives workers with `WorkerCommand` and `EngineEvent`.
- Engine and worker crates (`crates/engine`, `crates/worker`) implement `EngineBackend` and the worker side; `crates/cli` and `sdk/` consume `xylo_core::stdio`. See also [`sdk/README.md`](../../sdk/README.md).
