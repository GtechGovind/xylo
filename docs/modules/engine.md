# Agent engine (`xylo-engine`)

## Purpose and boundaries

`xylo-engine` is the agent engine. Its crate docs say it is "linked only into the task worker": the agent loop, model provider adapters, context budgeting, delegated helper agents and the durable tool broker. `AgentEngine` implements `xylo_core::EngineBackend`, so the host talks to it only through the contracts in [core](core.md) (`TurnRequest` in, `EngineEvent` out).

Dependencies (`crates/engine/Cargo.toml`): `xylo-core`, `xylo-runtime`, `xylo-resources`, `xylo-services`, `xylo-capabilities`, plus HTTP (`reqwest`, rustls), `tiktoken-rs`, `jsonschema` and search/diff helpers. Direct dependents: `worker`, `cli` and `evals` (`evals` enables the `test-support` feature). Persistence (the `Store` that holds checkpoints, actions and receipts) lives in [services](services.md); process orchestration and `AppPaths` live in [runtime](runtime.md).

Public surface (`src/lib.rs`): `AgentEngine`, `ProviderLayer`, `ProviderCredentialResolver`, `RunOptions`, `CustomToolHandler`, `RefusalObserver`, the model types (`Message`, `ModelProvider`, `ModelRequest`, `ModelResponse`, `ModelError`, `StreamEvent`, `Assembler`, …) and `pub mod tools`. The model types are public so `xylo-evals` can script or record a provider through `AgentEngine::with_provider_layer`; **the worker never installs a layer**.

Sibling docs: [engine-tools](engine-tools.md) (tool broker, permissions, built-in tools), [engine-providers](engine-providers.md) (wire adapters), [engine-agents-and-skills](engine-agents-and-skills.md) (helper agents, skills).

## Module map

| Path under `crates/engine/src` | Purpose |
| --- | --- |
| `lib.rs` | Module wiring and public re-exports |
| `engine.rs` (+ `engine/tests*`) | `AgentEngine`: turn admission, checkpoint restore, system prompt, recall, title, completion/cancel/failure settlement, error classification |
| `agent_loop/mod.rs` | The loop: fit, stream, tool rounds, checkpoints, notes, retries, step limit |
| `agent_loop/recovery.rs` | Reply classification: continuations, empty-reply re-ask |
| `agent_loop/stall.rs` | Deterministic stall detector |
| `agent_loop/retry.rs`, `fallback.rs` | Retry delays; durable fallback-model selection |
| `agent_loop/dispatch.rs` (+ `dispatch/`) | `ToolExecutor`, argument validation and aliasing, error results |
| `agent_loop/hooks.rs` | Lifecycle hooks run off the async workers |
| `provider_guard.rs` (+ `window.rs`, `masking.rs`, `record.rs`) | `GuardedProvider` and `ContextBudget`: admission, effective window, masking, summaries |
| `prompts/` (`mod.rs`, `render.rs`, `instructions.rs`, `skills.rs`, `environment.rs`, `output_styles.rs`) | Prompt rendering over `crates/engine/prompts/*.md`; project instructions; skills index |
| `model.rs` | `Message` (also the checkpoint format), `ModelProvider`, `ModelError`, streaming assembler |
| `providers/` | Native HTTP adapters (Chat Completions, Responses, Anthropic, Gemini, Bedrock), credential renewal, search |
| `tools/` | The tool broker and built-in tools; see [engine-tools](engine-tools.md) |
| `subagent.rs` | `agent_delegate` helper agents |
| `broker_tools.rs` | Loop's view of a `ToolBroker`; schedules reads vs writes |
| `steering.rs` | Durable mid-turn guidance applied between tool rounds |
| `attachments.rs` | Hydrates managed images for provider requests (at most 16 images and 16 MiB per request) |
| `usage_meter.rs` | Turn-owned token accounting across main, helper and utility requests |
| `credentials.rs`, `run_options.rs` | Host credential renewal trait; headless `RunOptions` |

## Agent loop (`agent_loop/`)

One step of `AgentLoop::steps`: apply queued guidance and tool notes, announce a permission-mode change, fit the conversation to the window, stream a model call, and finish if the reply asked for no tools. Otherwise **checkpoint the assistant message, run the tool batch, append results in call order, checkpoint again**, then repeat. Recovery is deterministic and driven by what the harness observes, never by the model's own account.

### Limits

- `MAX_MODEL_CALLS = 400` model calls per turn: a runaway guard, not a budget. `RunOptions::max_turns` can lower it (`min(max_turns, 400)`) but never raise it.
- The call that reaches the limit is the *final call*: the `step-limit.md` note is appended to that request only, and the reply is the answer whatever its finish reason (`LoopOutcome::stopped_early`).
- Step-budget notes (`step_budget_warning`) are added at calls `ceil(75% × limit)` and `ceil(90% × limit)`; a threshold equal to the final call is skipped. Each is an application note plus a `NoteKind::StepBudget` event for the user.
- Text deltas are batched: `DELTA_INTERVAL = 32 ms`. A response's text deltas are flushed before any tool event of that response.

### Reply recovery (`recovery.rs`)

Classification uses the finish reason of a reply without tool calls:

- Cut off by the output limit with text: continue. The partial reply stays, a `continue-answer` note is added, and the parts are joined into one answer. At most `MAX_CONTINUATIONS = 3` in a row; after that the answer is returned as it stands and `MessageStatus::Truncated` is stored (`NoteKind::Truncated` shown).
- Empty reply, reply whose reasoning used the whole output limit, or a tool call the provider could not parse: **asked for once more** with a note. A second unusable reply in a row fails the turn with `ModelError::EmptyResponse`.
- A tool call cut off by the output limit is **never dispatched**; it gets an error result asking for smaller arguments, checkpointed with the call before anything runs, so a resume cannot dispatch it either.
- A tool round or user guidance resets this state (`AnswerRecovery::reset`).

### Stall detector (`stall.rs`)

After every tool round the loop records a `Step` of signatures: the calls (canonical arguments, independent of key and call order), their results, and each failed call (call plus error text, durations like `0.52s` normalized). Over the last `WINDOW = 8` steps it looks for:

- **Repeated calls:** `REPEATED_PAIRS = 3` consecutive steps with the same calls and results, or `REPEATED_CALLS = 4` with the same calls whatever the results.
- **Alternation:** A B A B A B (`ALTERNATION = 6` steps, same results).
- **Repeated error:** the same call failing with the same error in `REPEATED_ERRORS = 3` recent steps.

Strikes escalate: **note**, then **warning** that the task stops, then **stop** with `ModelError::Stalled` (the task asks the user to add guidance). After a detection the history restarts from the latest step. `CALM_STEPS = 4` consecutive steps with calls new to the history clear the strikes. **A step that changed a file** (a successful result reporting a diff or created file) clears history and strikes, so an edit-and-test cycle never escalates. The detector is a pure function of the steps: no model call, no clock.

### Retries and fallback (`retry.rs`, `fallback.rs`)

- `MAX_RETRIES = 5` after the first failed attempt, only for `ModelError::retryable()` (`RateLimited`, `Unavailable`, `Timeout`, `EmptyResponse`, and `Provider` when flagged). `Authentication`, `InvalidRequest`, `ContextOverflow`, `Interrupted`, `Io` and `Stalled` are never retried.
- Backoff: base 1 s doubling per attempt (exponent capped at 6), capped at 32 s, with ±25 % jitter. A provider `Retry-After` is honored instead, but **a wait above 120 s (`MAX_RETRY_AFTER`) fails the request** so the user decides. Each wait emits `TurnPhase::Retrying { attempt, delay_ms }` and is cancellable.
- **Only `ModelError::Unavailable` permits the configured fallback model**, and only once per run, after retries are exhausted. The selection is written to conversation metadata (`select_checkpoint_fallback`) before the first alternative request, so a resumed turn restores it from trusted checkpoint metadata (`checkpoint_fallback`), never from prompt content. The user sees `NoteKind::Fallback`.
- Context overflow is recovered **once**: either detected while fitting or reported by the provider (`ModelError::ContextOverflow`, which also calls `ContextBudget::penalize_overflow`), the request is refitted under `Pressure::Overflow` and sent again.

### Lifecycle hooks and guidance

`SessionStart`, `UserPromptSubmit`, `Stop`/`SubagentStop` and `SessionEnd` hooks run on blocking workers (`hooks.rs`); hook output may add application notes, a stop reason ends the turn and a `Deny` decision fails it. User guidance (`steering.rs`) is inserted only after a complete tool round or before the first model call, **never between a tool call and its result**.

## Context and window management (`provider_guard/`)

### `GuardedProvider`

Wraps the real provider (itself wrapped by the usage meter): admits each request through the resource manager (`xylo-resources`), holds the cross-process local-generation lease (`local-inference.lock`) **only while a response streams, never across tool use**, clamps the output limit, and enforces first-response and idle deadlines. Cancellation ends a stream promptly with `ModelError::Interrupted`.

### Effective window (`window.rs`)

`W_eff` is the input tokens one request may fill: `ProviderConfig::context_window()` (the configured window lowered to what a local server serves, see [core](core.md)) minus the output reserve and a safety margin. Reasoning needs no extra reserve. Every size that reaches the model scales from `W_eff` and is a pure function of the provider configuration and served context, so tool presentations and the system prompt stay byte-stable across turns:

| Profile | Condition | Effect |
| --- | --- | --- |
| Large | `W_eff` ≥ `SMALL_WINDOW_TOKENS` (16,384) | Fixed caps are the upper bounds |
| Small | `W_eff` < 16,384 | Fewer resident tools; summary triggers earlier |
| Tiny | `W_eff` under the usual output reserve < `TINY_WINDOW_TOKENS` (5,632) | Output reserve drops to a quarter of the window (at least 512); compact system prompt (`system-tiny.md`) and minimal resident tools; thresholds are shares of `W_eff` |

Derived budgets: inline tool result about 12 % of `W_eff` (1,024 to 25,000 tokens; tiny about 16 %, at most 1,024), helper answer about 6 % (768 to 8,192 tokens), project instructions about 8 % and skills index about 4 % (never above the 24 KiB and 8 KiB caps), `file_read` default lines one per 64 tokens (100 to 2,000).

### Tiers of compaction

1. **Token estimates** use tiktoken where the encoding is known, otherwise o200k counts scaled by 1.3 (`FOREIGN_TOKEN_SCALE`). They are calibrated against the input tokens the provider reports (bounded 0.85 to 2.0; requests under 1,024 tokens do not calibrate).
2. **Observation masking** (`masking.rs`): once history passes `MASK_SHARE_PERCENT = 55` % of the window, old **successful** tool results become one-line stubs (tool, key argument, call id, original size, how to get it back). **User messages, assistant text, tool calls, failed results and the newest tool round are never masked.** Batches must free at least 20 % of the input allowance (`BATCH_SHARE_PERCENT`), results under 1,024 bytes are kept, and retention is per tool in rounds: edits/helper reports/todos 10, reads and skills 6, shell and diff 4, searches and listings 3. Superseded or changed reads are masked first. Tiny windows use 60 % of `W_eff`, 384-byte minimum, 10 % batches and retention of at most 2 rounds. Stubs are written into the checkpointed conversation, so resumes replay identical bytes.
3. **Summary** (`record.rs`): past the summary threshold (80 % of the window; for small windows an eighth of `W_eff` below the hard limit), older history becomes a model-written summary (up to `SUMMARY_TOKENS = 2048`, floor 192, about 900 words, deadline at most 90 s) plus a **harness record** that never passes through the summarizer: the **pinned first user message and the last `RECENT_MESSAGES = 6`** earlier user messages verbatim (later ones counted as omitted, longest excerpted first if over budget, never cut below 256 bytes) and an artifact ledger (up to 40 files, 20 commands, 30 todos, 200 characters per item). The record is parsed back and extended at the next summary. Summary and record are durable in the checkpoint.
4. **Excerpts** only if no summary is possible; they affect one request.
5. **Overflow** (`Pressure::Overflow`): estimates are raised by 1.25 per overflow, every eligible result is masked (only the newest round kept) and the summary targets 60 % of a large window. Only once per model call (see the loop section).

`ContextUsage` events report the fitted request (system, tools, messages, summary) plus the provider's reported input and cache-read tokens.

## Prompts (`prompts/` and `crates/engine/prompts/`)

All model-facing instruction text lives in Markdown under `crates/engine/prompts/` (and built-in skills under `crates/engine/skills/`), compiled in with `include_str!` and reached only through `crate::prompts`. `crates/engine/prompts/README.md` lists every template and its API.

- **Renderer** (`render.rs`): `{{snake_case}}` placeholders substituted in **one pass**; inserted values are never rescanned, so project text containing `{{` is inserted verbatim. A placeholder without a value (`MissingValue`), a value without a placeholder (`UnusedValue`) and a malformed `{{` (`Malformed`) are all rejected. Tests render every template, so a bad edit fails `cargo test -p xylo-engine prompts`, not a user's turn.
- **System prompt** (`system(&PromptContext)`): identity, environment, permission paragraph for the mode, then optional project instructions and skills index. It depends only on `PromptContext` (the date has day granularity), so it is byte-identical across calls and prompt caches stay warm. A permission-mode change mid-turn appends a `permission-change` note instead of rewriting it. A tiny window uses `system-tiny.md`; sections that do not fit whole are left out and the prompt says how to reach them (`system-omitted*.md`).
- **Helper agents** (`subagent(ctx, task)`) reuse the parent's environment, permissions and project instructions, without skill or fact indexes.
- **Application notes**: recovery, stall, step-budget, step-limit and permission-change notes are user-role messages at their place in the conversation, so the cached system block holds only the system prompt.
- **Recalled context** is a per-turn note after the user's message, not part of the system prompt (up to 8 facts and 2 procedures per the prompts README); in a tiny window a note over about 6 % of `W_eff` becomes a pointer to `memory_search`.
- **Project instructions** (`instructions.rs`): `INSTRUCTION_BYTES = 24 KiB` maximum (scaled down by window). From the repository root down to the workspace each directory is read for `AGENTS.md`, `CLAUDE.md`, `AGENTS.local.md`, `CLAUDE.local.md`; root first, nearest last; the budget is spent nearest first, so outer files are truncated or omitted first. Duplicate text (for example a symlinked `CLAUDE.md`) is skipped; `@` imports are followed to depth 5 with cycle detection; rule files under `.claude/rules` and `.xylo/rules` carry `paths:` globs in frontmatter. **`prompts/README.md` describes only `AGENTS.md`/`CLAUDE.md`; the code is broader** (local variants, imports, rules).
- **Skills index** (`skills.rs`): one `name: description` line per skill the model may use on its own, at most `SKILLS_INDEX_BYTES = 8 KiB` (scaled by window). Bodies load only through `extension_skill` and resources through `extension_resource`; see [engine-agents-and-skills](engine-agents-and-skills.md).
- **Titles**: only while the conversation's `title_source` is still `Prompt`, after `Completed`: output limit `TITLE_TOKENS = 64`, deadline 10 s, reply text sent capped at 2,000 bytes, stored title at most `TITLE_CHARS = 80`. A title failure is logged and **can neither delay nor fail the turn**.

## Conversation checkpoints and turn lifecycle (`engine.rs`, `agent_loop/mod.rs`)

`AgentEngine::run_turn` admits a turn and settles it with **exactly one terminal event**:

1. **Replay.** If the store already holds a receipt for `(session, turn)` (`completed_turn`), `Completed` is re-sent with the stored text and usage and **no model call is made**. A cancelled token before start yields `Cancelled`.
2. **Admission.** `TurnPhase::Preparing`; the provider config, attachments and absolute existing workspace directory are validated; a `LearningTurnGuard` guarantees terminal procedural evidence is queued on any error, cancellation or drop.
3. **Live activity.** Tool execution has bounded in-memory tracking for current work; no durable action journal, reconciliation gate, or manual review form is consulted. Ordinary permission checks and in-process duplicate-call guards still apply.
4. **Restore** (`restored_messages`). Saved conversation messages remain the source of chat context. Available live results can settle unanswered calls; when no live result is available, the call becomes an interrupted tool result on both same-turn and new-turn resumes. It is never automatically executed from an old checkpoint. The model is told to inspect current state before deciding on another action. The settled conversation is checkpointed before a new user message is appended.
5. **Run** the loop (system prompt and recalled-context note built on blocking workers).
6. **Settle.**
   - *Completed*: checkpoint, reply, usage and receipt commit **atomically** (`complete_turn_with_reasoning`), then `Completed` is sent, then the best-effort title. Status is `Truncated` if the answer was cut off at the output limit, else `Complete`.
   - *Cancelled*: the streamed text is stored as an `Interrupted` reply, then `Cancelled`.
   - *Failed*: the streamed text is stored as a `Failed` reply and the error returns; `classify` maps `ModelError` to `ErrorKind` and a retryable flag (anything else is `Internal`, not retryable).

**Checkpoints.** The root loop writes the conversation (`Message` is the durable format, including provider-native thinking/signature content) through `ConversationState::checkpoint`, which sends only the changed suffix relative to its previous revision. Writes occur before every tool batch and after its results, after masking or summarising, and when a fallback model is chosen. A crash between them leaves unanswered calls recorded as interrupted on resume. The broker deduplicates calls only within live in-memory tracking; no action journal is saved. Child loops report through tool events with a `parent_id`.


## Tests

Unit tests are colocated, with no integration-test directory under `crates/engine`:

- `engine/tests.rs` and `engine/tests/` (`headless`, `interaction`, `provider_fallback`), plus instruction-provenance tests for startup and child agents.
- `agent_loop/tests/` (`mod.rs`, `hooks.rs`, `provider_fallback.rs`) and the `recovery`, `retry` and `stall` test modules.
- `provider_guard/tests.rs` (masking, summaries, calibration, overflow).
- `prompts` tests (every template rendered with exactly its placeholders; skills reject unknown tool names).
- `model/tests.rs`, `attachments/tests.rs`, `subagent/tests.rs`, `usage_meter/tests.rs`, and `tools/**/tests` ([engine-tools](engine-tools.md)).

Run with `cargo test -p xylo-engine` (for example `cargo test -p xylo-engine prompts`). The `test-support` feature is enabled by `xylo-evals`; dev-dependencies enable `xylo-services/test-support`.

## Related documents

[core](core.md) (contracts: `TurnRequest`, `EngineEvent`, `ErrorKind`) · [services](services.md) (`Store`: checkpoints, actions, receipts, steering) · [runtime](runtime.md) (`AppPaths`, worker hosting) · [engine-tools](engine-tools.md) · [engine-providers](engine-providers.md) · [engine-agents-and-skills](engine-agents-and-skills.md)
