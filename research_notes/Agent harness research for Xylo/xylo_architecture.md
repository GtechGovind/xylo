# Xylo agent harness: architecture notes for research mapping

Repo: `/Volumes/workspace/ai/xylo`, branch `fix/audit-remediation`, read on 2026-10-31. All paths are relative to the repo root. Line numbers are from the working tree at that time.

## 0. Provenance caveat (read this first)

- The native engine described here (`crates/engine/src/agent_loop/`, `model.rs`, `subagent.rs`, `broker_tools.rs`, `prompts/`, `provider_guard/`, `tools/permissions.rs`, `tools/risk.rs`) is **uncommitted/untracked** in the working tree (`git status`: `?? crates/engine/src/agent_loop/` etc.). Some of these files changed while I was reading them, so a few line numbers may drift.
- At `HEAD` (bd5f43c) the loop was the vendored **Hermes** engine (`git show HEAD:Cargo.toml` line 5 excludes `vendor/hermes-ultra`). The working tree has no Hermes code left. The only mention is a test asserting its absence: `crates/engine/src/engine/tests.rs:654`.
- `AUDIT.md` (dated 2026-10-30) describes the **old Hermes-based** loop. Its file and line references point at that code. Section 9 below says which of its findings look fixed in the current tree.

## 1. Component map

| Concern | File(s) |
|---|---|
| Turn admission, restore, settle | `crates/engine/src/engine.rs` (`run_turn` 308–492) |
| Agent loop (one step = fit → stream → tools) | `crates/engine/src/agent_loop/mod.rs` |
| Tool-call validation and parallel dispatch | `crates/engine/src/agent_loop/dispatch.rs` |
| Model retry policy | `crates/engine/src/agent_loop/retry.rs` |
| Provider-neutral message and stream types | `crates/engine/src/model.rs` |
| Context budget, compaction, local-model admission | `crates/engine/src/provider_guard.rs` |
| Wire adapters (Chat Completions, Responses, Anthropic Messages, Gemini, Bedrock) | `crates/engine/src/providers/{mod,request,stream,bedrock}.rs` |
| Prompt templates and builder | `crates/engine/prompts/*.md`, `crates/engine/src/prompts/{mod,environment,instructions,skills}.rs` |
| Tool broker (journal, approvals, hooks, result shaping) | `crates/engine/src/tools/mod.rs` |
| Built-in tool schemas and implementations | `crates/engine/src/tools/builtin.rs`, `code.rs`, `portable.rs`, `image_generation.rs` |
| Permission modes and risk rules | `crates/engine/src/tools/permissions.rs`, `risk.rs` |
| Loop-facing adapter over the broker | `crates/engine/src/broker_tools.rs` |
| Subagents | `crates/engine/src/subagent.rs` |
| Steering (mid-turn user guidance) | `crates/engine/src/steering.rs`, `crates/services/src/steering.rs`, `crates/core/src/steering.rs` |
| Fact memory (FTS5 + optional vectors) | `crates/services/src/memory.rs` |
| Learned procedures ("skills" from past turns) | `crates/services/src/learning/{mod,persistence}.rs` |
| Conversation history search (UI only) | `crates/services/src/history_search.rs` |
| Provider config and context limit | `crates/core/src/lib.rs:52-80` (`ProviderConfig`), `crates/services/src/providers/discovery.rs:190-200` |

## 2. Turn lifecycle (`engine.rs`)

1. **Replay guard.** If a completed receipt exists, the engine re-emits `Completed` without calling the model (`engine.rs:314-325`).
2. **Unresolved actions block resume.** `outcome_unknown` native actions get one recovery attempt. Any action still `outcome_unknown` or `running` fails the turn until the user reconciles it (`engine.rs:362-374`).
3. **Provider stack.** `GuardedProvider(AttachmentProvider(NativeProvider|Bedrock))` (`engine.rs:375-386`). A `ContextBudget` is built from `ProviderConfig` (`engine.rs:387`).
4. **History restore.** The durable per-session checkpoint (a full `Vec<Message>`) is loaded. Pending tool calls from an earlier turn are settled from the action journal, then the new user message is appended (`engine.rs:151-182`, `settle_pending_tools` 601-670). History therefore spans all earlier turns, including every earlier tool call and result, until compaction replaces part of it.
5. **System prompt.** Built **once per turn** (`engine.rs:392`, `system_prompt` 200-219). See section 5.
6. **Delegation.** A delegation handler is wired with up to `min(suggested_parallelism, 2)` slots (`engine.rs:396-414`).
7. **Loop run.** `AgentLoop{max_model_calls: MAX_MODEL_CALLS, journal: Some(..)}` (`engine.rs:421-439`).
8. **Settle.** The checkpoint, reply and receipt are committed atomically (`engine.rs:447-453`). A title is generated after the first turn (`engine.rs:225-291`, 64 tokens, `Reasoning::Lowest`, 10 s deadline). Cancel and failure keep the streamed partial text (`engine.rs:471-490`).
9. **Background learning job.** A drop guard enqueues the procedural-learning job (`engine.rs:45-60`, 349-354).

## 3. Agent loop (`agent_loop/mod.rs`)

The module doc states the invariants (`mod.rs:1-26`): checkpoint before and after every tool batch, exactly-once dispatch through the journaled broker, guidance only at role-safe boundaries, and "no hidden prompting".

**One step, `steps()` at `mod.rs:192-272`:**
1. **Resume.** A resumed turn first executes unanswered calls from the last assistant message (`mod.rs:197-203`).
2. **Steering.** Queued user guidance is applied (`mod.rs:210`).
3. **Permission change.** If the live permission mode changed, a user-role `<application_note>` is appended. The system prompt is never rebuilt (`mod.rs:211-217`, `application_note` 564-566).
4. **Model call.** `model_call` re-reads tool schemas every call (`mod.rs:344`). It fits the context (section 4), appends a step-limit note on the final call (`mod.rs:347-349`), streams and retries.
5. **Call ids.** Ids are made unique (`unique_call_ids`, `mod.rs:572-591`) because some servers restart ids like `call_0` every response.
6. **No tool calls.** The assistant message is pushed. If guidance arrived during streaming, the loop continues; otherwise it returns the text as the final answer (`mod.rs:224-232`).
7. **Tool calls.** Each call is validated and alias-resolved (`dispatch::prepare`). The assistant message is pushed and checkpointed, then the batch runs, results are appended in call order, and the state is checkpointed again (`mod.rs:254-270`, `run_batch` 317-334).

**Text streaming.** Deltas are batched every 32 ms (`mod.rs:59`, 468-516). A retried attempt does not repeat text the user already saw; a diverging retry is set off by a paragraph break (`mod.rs:483-512`).

**Response finish reason.** The loop never reads `ModelResponse.finish`. A `Length` (max_tokens) cutoff without tool calls is accepted as the final answer. `Assembler::finish` only coerces the reason (`model.rs:473-477`).

### 3.1 Tool dispatch and parallelism (`dispatch.rs`)

- **Validation.** Arguments must parse as a JSON object, otherwise the model gets an error result: "not valid JSON … Send the call again" (`dispatch.rs:68-83`). There is **no JSON-Schema validation** of arguments; each tool parses its own.
- **Aliases.** An exact 9-entry alias table maps names other agents use (`read→file_read`, `bash→shell_command`, `grep→search_project`, …) (`dispatch.rs:40-50`). An unknown name returns "Use search_tools … and call_tool" (`dispatch.rs:103-106`).
- **Parallel reads, serialized writes.** Read-only calls run concurrently, at most `MAX_PARALLEL_READS = 4` at once (`dispatch.rs:16`). A writing call waits for every earlier call and runs alone (`dispatch.rs:131-152`). Results are returned in call order (`dispatch.rs:157-160`).
- **Access classification.** This comes from each tool's static `writes` flag (`broker_tools.rs:57-86`):
  - `call_tool` uses its target's flag; an unknown target counts as a write.
  - `agent_delegate` is `writes:false` (`builtin.rs:74-80`), so two delegations can run in parallel. They are bounded by the 2-slot semaphore.
  - `connector_call` is statically `writes:true`, so MCP calls are always serialized.
- **Per-file locking.** Writes to the same file also take a process-wide per-file `EditLock` from preview to write (`tools/mod.rs:515-520`).
- **Failures become results.** A tool failure becomes `{"status":"error","summary":…}` (`dispatch.rs:62-65`, 143-147) and is returned as a result, never as a turn failure.

### 3.2 Retries and errors (`retry.rs`, `model.rs`, `providers/mod.rs`)

- **Policy.** Up to `MAX_RETRIES = 5` retries. Exponential backoff runs 1 s → 32 s with ±25 % jitter; `Retry-After` is honoured up to 120 s and anything longer fails (`retry.rs:5-33`).
- **Retryable errors.** `RateLimited` and `Timeout` are always retryable; `Provider{retryable}` depends on the flag (`model.rs:328-339`). Not retryable: `Authentication`, `InvalidRequest`, `ContextOverflow`, `Interrupted`, `Io` (`retry.rs:62-72`).
- **HTTP mapping** (`providers/mod.rs:147-219`):
  - 401/403 → `Authentication`
  - 429 → `RateLimited`
  - `context_length_exceeded`, `string_above_max_length`, `request_too_large` → `ContextOverflow`
  - 408/409/5xx → retryable
  - A transport error is retryable only if the connection never opened (`providers/mod.rs:120-125`).
  - Stream error codes such as `overloaded_error` or `RESOURCE_EXHAUSTED` are transient (`stream.rs:14-34`).
  - A stream that ends without a terminal event is transient (`model.rs:453-455`, `stream.rs:200-205`).
- **No replay after a tool call.** A failed stream that already emitted a tool call is fatal and never replayed (`mod.rs:525-527`).
- **Deadlines.** `GuardedProvider` enforces first-response and idle deadlines equal to `timeout_seconds` (default 120 s, `core/src/lib.rs:91`; `provider_guard.rs:145-170`). It also clamps `max_tokens` to `output_tokens` (`provider_guard.rs:146`).
- **Phases.** The UI receives `TurnPhase` events: `Thinking`, `Responding`, `RunningTools`, `WaitingForApproval`, `Compacting`, `Retrying{attempt, delay_ms}` and `Delegating` (`core/src/lib.rs:513-530`).
- **Turn error classification.** A turn error is classified from the `ModelError` in the cause chain (`engine.rs:530-538`).

### 3.3 Stop conditions

- **Natural stop.** A response with no tool calls ends the turn (`mod.rs:224-232`).
- **Step limit.** `MAX_MODEL_CALLS = 400` per turn, commented as a "Runaway guard … not a budget" (`mod.rs:56-57`).
  - The last call carries the `step-limit.md` note, which says not to call tools and to summarize (`prompts/step-limit.md`, `mod.rs:347-349`).
  - Tool calls made on that last call get "Not run: … step limit" results.
  - The model's text is returned with `stopped_early=true` (`mod.rs:233-253`). The engine only logs a warning (`engine.rs:444-446`).
- **Cancellation** is checked at the top of every step and during retry sleeps (`mod.rs:207-209`, 393-396).
- **Subagent timeout.** 15 min (`subagent.rs:30`).
- **No stall detection.** There is no repeated-call or loop/stall detector (see gaps).

## 4. Context window management (`provider_guard.rs`)

**Where limits come from.** `ProviderConfig.context_tokens` and `output_tokens` per configured connection, defaulting to 8192/2048 (`core/src/lib.rs:59-60, 89-90`). Validation requires context ≥ 1024 and output < context (`core/src/lib.rs:174`). Model discovery fills context from catalog fields `context_window`, `context_length`, `inputTokenLimit` or `metadata/context_length` (`services/providers/discovery.rs:190-200`). The UI applies it and caps output at context/4 (`desktop/src/xylo/providers.rs:146-152`, `desktop/src/xylo.rs:1670-1673`). There is **no built-in per-model limit table** in the engine. Bedrock catalog rows carry no context size (`discovery.rs:309`).

**Token counting.**
- OpenAI-family models get exact `tiktoken` counts when the model's encoding is known. Everything else uses `o200k` × 1.3 (`provider_guard.rs:29-32, 193-216`).
- Each message is costed by serializing the whole `Message` (native blocks and reasoning included) plus image-patch estimates of 32×32 patches + 1024 (`provider_guard.rs:220-241`).
- Tools are costed as schema JSON + 64 per tool. Framing adds 256 (`provider_guard.rs:37-40, 247-266`).
- Provider-reported `input_tokens` is only shown in the UI (`mod.rs:362-375`). It is **not** fed back to calibrate the estimate.

**Assembly per call** (`AgentLoop::fit`, `mod.rs:402-449`): `[system prompt] + history`.

1. **Proactive compaction trigger.** History estimate > allowance at 80 % of context, after subtracting tools, framing and the output reserve (`provider_guard.rs:268-281`).
2. **Summary plan** (`provider_guard.rs:318-361`):
   - Keeps a verbatim tail of about half the remaining allowance.
   - Cuts only at a user message, or at an assistant message after the current request, so a call is never separated from its result.
   - Keeps the current user request (the "anchor") verbatim.
   - Needs at least 4096 tokens of history to condense.
3. **Summarize** (`provider_guard.rs:366-418`):
   - Uses the **same provider and model**, no tools, at most 2048 output tokens.
   - Deadline is min(provider timeout, 90 s).
   - The transcript is rendered with per-message excerpt budgets on the ladder [4000…120] bytes: user ×2, tool ÷2 (`provider_guard.rs:382-390, 532-562`).
   - The summarizer prompt asks for five sections (user requests, decisions, work done, current state, next steps) in under 900 words (`prompts/compaction.md`).
   - An existing summary is *updated* rather than re-summarized (`prompts/compaction-update.md`).
   - The result is a `Role::System` message with `summary: true` (`provider_guard.rs:410-417`). It **replaces history and is checkpointed**, so later steps and turns reuse it (`mod.rs:431-434`).
4. **Fallback when summarizing fails** (`provider_guard.rs:425-528`). These changes affect one request view only; the stored history is untouched.
   - (a) Older messages are replaced by one assistant message of bounded excerpts.
   - (b) **Tool results** are shrunk in place on the ladder 4096 → 2048 → 1024 → 512 bytes, keeping head and tail.
   - (c) Whole completed tool rounds are condensed into one "tool notes" message with 192-byte excerpts, only while over the hard limit.
   - (d) If the request still does not fit → `ContextOverflow` (not retried).
   - The excerpt marker reads "omitted; full content retained in conversation/action history" (`provider_guard.rs:566-581`).
5. **Telemetry.** A `ContextUsage` event (capacity, system, tools, messages, summary, deferred tool count, autocompact threshold) is emitted every call (`provider_guard.rs:284-311`, `core/src/lib.rs:534+`). The UI renders it in `desktop/src/xylo/context_panel.rs`.

**Large tool outputs** (`tools/mod.rs`):
- **Model-facing cap** is 64 KiB per result (`model_result`, `tools/mod.rs:1235-1285`).
  - Over the cap, long strings are middle-truncated on the ladder 16 KiB → 256 B and arrays are capped at 400 items (`shorten_strings` 1379-1400).
  - The complete result is rendered as readable text to `artifacts/tool-<id>.txt`. The result gets `truncated:true`, a note, and `full_result:<path>`, which the model can page with `file_read` offset/limit (`builtin.rs` `resolve` allows reading artifacts).
- **UI cap** is 8 KiB with a spill file `tool-<id>.json` (`bound_result`, `tools/mod.rs:1135-1200`).
- **Source-level caps:**
  - `file_read`: 2000 lines by default (max 10 000), 40 KiB, 2000 chars per line (`code.rs:16-21`).
  - Subprocess stdout/stderr: 512 KiB each (`runtime/src/process.rs:20`).
  - Subagent answer: 64 KiB (`subagent.rs:32`).
- **Note for small windows.** 64 KiB is about 16 k tokens, which is larger than the default 8 k local context. Small local windows depend entirely on the `fit` fallback.

**Prompt caching:**
- **Anthropic.** Three `cache_control: ephemeral` breakpoints: the last system block, the last tool definition, and the last content block of the newest message (`providers/request.rs:286-319`; test `providers/tests.rs:482-483`). Usage totals add cache read and creation tokens (`stream.rs:380-383`).
- **OpenAI and Gemini.** Implicit caching only; no `prompt_cache_key` or `cachedContent` is sent.
- **Bedrock.** No `cachePoint` (searched `cache` in `bedrock.rs`: no hits).
- **Cache-friendly design.** The system prompt stays byte-identical within a turn (`prompts/mod.rs:13-15`). Mid-turn changes are appended as user-role notes, not edits to the system block (`mod.rs:22-26`).
- **Caveat.** After compaction, the summary is a second system block (`request.rs:241-243`), so the system breakpoint moves onto the summary.

## 5. System prompt (`prompts/mod.rs`, `crates/engine/prompts/`)

**Composition** (`prompts/mod.rs:105-123`):
- `system.md`: identity "You are Xylo, a coding agent…", working method, communication rules (3083 B).
- Environment: workspace, OS/arch, shell, git branch, **UTC date at day granularity** (`environment.md`, `environment.rs:50-66`).
- Permission-mode policy (`permissions.md` + `permissions/*.md`).
- Project instructions: `AGENTS.md` then `CLAUDE.md` in each directory from the repo root to the workspace. Nearest-first budget of 24 KiB, duplicates removed (`instructions.rs:18-76`).
- **Skills index**: `- name: description` lines, 8 KiB budget, descriptions shortened then omitted (`skills.rs:96-120`).
- **Fact titles**: first line of each recalled fact, ≤ 24 titles (`prompts/mod.rs:270-283`).

**Size.** Roughly 1 k tokens fixed, about 6 KB with the six built-in skills, per the README budget (`crates/engine/prompts/README.md:48`). A test enforces ≤ 12 KiB excluding project instructions (`prompts/tests.rs:156-174`). Tool schemas are separate. My rough estimate from source bytes is ~8–9 KB for the ~22 always-offered tools (`builtin.rs:72-228`, `portable.rs:22-27`).

**Stability:**
- **Within a turn:** byte-identical, built once (`engine.rs:392`).
- **Across turns: not guaranteed.** The fact and skill indexes are **retrieved using the current user prompt as the query**:
  - `project_facts` → `recall_sync(workspace, prompt, 8)` (`tools/mod.rs:247-253`)
  - learned procedures → `relevant_skills(workspace, prompt, 2)` (`tools/mod.rs:260-287`)
  - portable skills are included only if automatic or explicitly invoked in the prompt (`portable.rs:216-225`)
  - project-instruction files and the git branch are re-read every turn.
- **Consequence.** A new turn with a different prompt can change the system block and invalidate the cached prefix for the whole history. `crates/engine/prompts/README.md:34` says "nothing else varies", which is accurate only within a task (turn).
- **Doc mismatch.** The README says "at most 24 titles", but the engine recalls 8.

**Subagent prompt** (`prompts/mod.rs:129-140`, `prompts/subagent.md`): same environment, permissions and project instructions, plus `# Task`. No skill or fact indexes.

## 6. Memory

| Kind | Storage and retrieval | How it reaches context |
|---|---|---|
| **Project facts** (`MemoryRecord{scope=workspace path, content, source, supersedes}`) | SQLite `memories` + FTS5 `memories_fts` (bm25) + optional `sqlite-vec` `vec0` cosine index with local ONNX embeddings via fastembed, e.g. `bge-small-en-v1.5` from an optional asset (`services/memory.rs:1-120, 574-590`; `capabilities/src/assets/activation.rs:99`). Hybrid recall fuses lexical and semantic lists with **Reciprocal Rank Fusion, k=60** (`memory.rs:453-482`). Lexical-only when embeddings are off; failures back off 10 min (`memory.rs:158`). | (1) **Always-on, retrieved:** titles (first line) of the top 8 facts matched against the turn's prompt go into the system prompt (`engine.rs:212-217`, `tools/mod.rs:247-253`). (2) **On demand:** `memory_search` tool, top 12 (`builtin.rs:158-163, 494-498`). |
| **Fact writes** | `memory_remember` tool (`builtin.rs:165-171, 499-509`) always creates a new record with `supersedes: None`. Correcting or superseding is only possible from the host UI (`runtime/src/host.rs:1758`; `memory_facts.rs`). | The tool returns the record. In Ask mode it is auto-allowed (`permissions.rs:322-323`). |
| **Learned procedures** | Deterministic, no LLM. Completed turns' action journals become "cases" (tool-step sequences + terms). A candidate is promoted only after independent later evidence and suspended on counter-evidence. Stored in `learning_candidates` + `learning_fts` (`services/learning/mod.rs:1-80`, `persistence.rs:299-360`). Retrieval: FTS5 bm25 over prompt terms (`persistence.rs:411-431`). | Top 2 relevant are listed in the skills index as "Learned procedure: … read with skill_read" (`tools/mod.rs:266-283`). Bodies via `skill_read` and `skill_list`. `skill_propose` queues evaluation. |
| **Built-in and installed skills** | 6 compiled-in Agent-Skills folders (`prompts/skills.rs:64-71`) plus portable packages. | Index only. Body via `extension_skill`, resources via `extension_resource`. Forked or tool-restricted skills run as a subagent (`portable.rs:196-212`). |
| **Conversation history search** | FTS over the `messages` table (`services/history_search.rs:1-60`). | **UI only**: used by `runtime/src/history.rs`, `runtime/src/host.rs` and `desktop/src/xylo/history_search.rs`. **No agent tool**; `engine/src` has no history reference. |
| **Within-task working memory** | The checkpoint plus the durable compaction summary (section 4). `todo_write` gives a visible checklist (`builtin.rs:151-157, 416-440`). | In history. |

**No automatic memory extraction or consolidation.** Facts are written only by the model's tool or by the user. There is no LLM summarization of facts and no decay. Embeddings are used only for facts, not for tools, skills or history.

## 7. Tools

**Inventory.** I counted 59 registered tool definitions on macOS/Linux:
- 22 built-in (`builtin.rs:72-227`). Windows drops `run_command` and `shell_command` (`builtin.rs:230`).
- 3 portable: `extension_skill`, `extension_resource`, `extension_component` (`portable.rs:22-27`).
- 1 `image_generate`.
- 33 capability tools (`capabilities/src/lib.rs:125-135`): browser 8, documents 7, local inference and speech 6, desktop 4, plugins 2, environments 3, schedule 3.

**Schema format.** Hand-written `ToolDefinition{name, description, parameters, writes}`. Parameters are JSON Schema with `additionalProperties:false` (`builtin.rs:236-250`). Responses-API functions are sent with `strict:false` (`request.rs:43`).

**Deferred loading (tool search).** Two lists are offered each call (`initial_definitions`, `tools/mod.rs:290-319`; `CORE_TOOLS` 55-77):
- the 19 `CORE_TOOLS`, filtered by the live mode (read-only mode hides writes, `tools/mod.rs:243-245`);
- capability families the user configured: browser if Chromium is set up, `local_inference_*`, `speech_*`, a configured `image_generate`, and `connector_*` if any connectors exist.

Everything else is reached through `search_tools`:
- Case-insensitive substring match: **any** query word against name or description, first 12 matches in registration order (no ranking). Returns full schemas (`tools/mod.rs:941-961`).
- The model then runs a tool through `call_tool{name, arguments}`, which recurses into the target (`tools/mod.rs:406-421`).
- Deferred tools are **not** added to the native `tools` array after discovery.
- A direct call to a deferred tool by its exact name is also accepted, because `access`/`effective_definition` look up all definitions (`broker_tools.rs:75-85`, `tools/mod.rs:1061-1067`).
- `ContextUsage.deferred_tool_count` reports the gap (`broker_tools.rs:52-55`).

**MCP.** MCP server tools are **never** put in the tool list. The model goes `connector_list` → `connector_tools{connector_id}` (discovers and caches schemas) → `connector_call{connector_id, tool, arguments}` (`builtin.rs:179-199`, `tools/mod.rs:1099-1129`). The effective write classification comes from MCP annotations: read-only only if `readOnlyHint` and not `destructiveHint` (`services/connectors/mcp.rs:700-706`, `tools/mod.rs:1068-1091`).

**Result format.** The tool's JSON, serialized compactly as the tool-message content. Most results look like `{"status":"success","summary":…,"artifacts":[…],"next_actions":[…]}`; built-ins return tool-specific JSON such as `file_read` lines, `truncated` and `next_offset`.
- Failures: `{"status":"error","summary":"…"}`.
- Observed unsuccessful results are wrapped as "Tool reported an unsuccessful result: {json}" (`tools/mod.rs:1401-1406`; `observation_failed` 1409-1430).
- Gemini parses the JSON into `functionResponse.response` (`request.rs:341-353`).

**Execution pipeline** (`execute_scoped`, `tools/mod.rs:394-790`):
1. Journal lookup by idempotency key `root:<call_id>` or `child:<len>:<ns>:<call_id>`; an existing entry is replayed (`tools/mod.rs:429-433`, 321-326).
2. Classify `writes` and reach (inside or outside the project).
3. `permissions::decide(mode, call)` → Allow, Ask or Refuse (`tools/mod.rs:477`; `permissions.rs:234-340`). Remembered "always allow" rules skip Ask.
4. Per-file `EditLock` (515-520).
5. `begin_action` writes a durable intent (552).
6. `ToolStarted` event, then the approval wait (`await_approval` 884+). The mode is re-checked after approval.
7. PreToolUse hooks (620), execute, `finish_action`, PostToolUse hooks.
8. `ToolFinished` event, then `model_result`.
9. Writes with an uncertain outcome become `outcome_unknown` and block resume.

**Permission modes** (`permissions.rs:1-21, 234-336`):
- **AskForApproval**: in-project reads, edits, sandboxed commands and memory writes run. Outside paths, connectors, plugins, browser, environments, schedules and image generation ask.
- **ApproveForMe**: asks only when `risk::assess` flags the call. Rules are deterministic tables over parsed shell words: destructive commands, privilege escalation, curl|sh, secret paths, auto-run config files, uploads (`risk.rs:1-60`).
- **FullAccess**: allows everything; commands run unsandboxed.
- **ReadOnly**: refuses writes.
- **Network access** must be requested explicitly with a justification (`builtin.rs:37-60`; `permissions.rs:285-304`).
- **Live mode.** The mode lives in an atomic `PermissionCell`, read at every call and updated by `WorkerCommand::SetPermissions` (`permissions.rs:39-60`; `worker/src/main.rs:322`).

## 8. Subagents (`subagent.rs`)

- **Spawn.** The `agent_delegate{task, context}` tool (`builtin.rs:74-80`) or a forked skill (`portable.rs:196-212`) calls `Delegation::run`.
  - Waits for one of ≤ 2 semaphore slots (`subagent.rs:28, 68-71`).
  - Creates a `scoped_child` broker that shares the journal, mode, approved rules and memory, with its own cancel token (`tools/mod.rs:181-199`).
  - Forked skills can restrict tools (`subagent.rs:77-90`).
- **Context received.** A fresh conversation with:
  - the subagent system prompt (environment, permissions, project instructions, `# Task`);
  - one user message: `task` + `"\n\n"` + `context` (`subagent.rs:93-119`).
  - **No parent history.** The task text appears twice (system `# Task` and user message).
- **Limits.** No further delegation (depth 1; `broker_tools.rs:32-35`, `tools/mod.rs:402-405`). Same provider and model as the parent, and the same 400-call cap and compaction policy.
- **Not persisted or streamed.** No journal, checkpoint, steering or text streaming (`journal: None`, `subagent.rs:115`). The child's tool events carry `parent_id` for the UI.
- **Return.** Only the **final answer text**, bounded at 64 KB, as `{"status":"completed","result":…}` (or `timeout`/`error`). It becomes the `agent_delegate` tool result (`subagent.rs:131-164`). The child transcript is discarded.
- **Timeout.** 15 min; on timeout the child is cancelled and its in-flight tools settle (`subagent.rs:121-136`).

## 9. Steering (`engine/src/steering.rs`, `services/steering.rs`, `core/src/steering.rs`)

**What it is.** Durable mid-turn user guidance ("steer while it works").

**Flow:**
1. The UI sends `WorkerCommand::SteerDurable` (`worker/src/main.rs:347-357`).
2. A `user_guidance` row is queued: max 16 KiB each, 64 per turn, 128 KiB per turn (`core/src/steering.rs:4`, `services/steering.rs:12-15`).
3. The engine is notified (`engine.rs:88-90`).
4. At the next role-safe boundary (top of each step, or right after a no-tool answer, which then continues), `SteeringControl::apply` runs one transaction (`services/steering.rs:276-345`). It inserts up to 8 guidance items as plain `{"role":"user","content":…,"name":"guidance-…"}` messages into the checkpoint, records them as chat messages, and marks them applied.
5. A `Steering{receipt}` event is emitted (`engine/src/steering.rs:40-73`; `mod.rs:210, 228`).

**Not applied mid-call.** Guidance is never inserted during a model stream, a long tool, or an approval wait.

## 10. Providers and local models

- **Protocols.** Five native adapters: Chat Completions, Responses, Anthropic Messages, Gemini generateContent, and Bedrock Converse (`providers/mod.rs:1-36`).
- **Local servers.** Ollama, LM Studio, llama.cpp and vLLM use the **OpenAI Chat Completions** adapter with native `tools`/`tool_calls` (`request.rs:97-158`, `stream.rs:225-262`). The only local-specific handling is resource admission and a single-slot cross-process lease for app-managed loopback servers (`provider_guard.rs:54-133`, `engine.rs:558-576`).
- **No fallback for models without native function calling.** There is no text or ReAct tool-call parser, no `<tool_call>` extraction, no grammar or JSON mode, and no JSON repair. I searched for `<tool_call>`, `repair`, `json5`, `supports_tools`, `tool_choice`, `response_format` and `json_schema`; nothing relevant turned up in engine or services.
- **Malformed tool JSON** becomes an error result so the model can retry (`dispatch.rs:68-83`). Truncated Anthropic tool input is tolerated only when the finish reason is `Length` (`stream.rs:454-497`).
- **Reasoning:**
  - OpenAI families send `reasoning_effort` (capability-checked) (`request.rs:66-95`; `core/src/lib.rs:101-160`).
  - Anthropic and Gemini request bodies set no `thinking`/`thinkingConfig`. Signed thinking blocks returned by the provider are still replayed verbatim through `Message.native` (`model.rs:54-67`, `request.rs:254-266`).
  - Bedrock sends the `interleaved-thinking` and `context-1m` beta headers (`bedrock.rs:21-25`).
- **Parallel tool calls.** No `parallel_tool_calls` parameter is set, so provider defaults apply.
- **Images.** Hydrated per request: at most 16 images and 16 MiB (`attachments.rs:14-16`).

## 11. AUDIT.md: what still applies to the current tree

AUDIT.md audits the old Hermes loop. Comparing its loop, context and memory findings with the current code:

| AUDIT finding (old line refs) | Current tree |
|---|---|
| C: Hermes identity prepended | Looks fixed: `prompts/system.md:1`; test `engine/tests.rs:654` |
| C: per-minute timestamp busts cache | Looks fixed: day-granularity date (`environment.rs:50-66`). Cross-turn variation from retrieved facts and skills remains (section 5) |
| C: parallel edits race | Looks fixed: writes serialized (`dispatch.rs:131-150`) + `EditLock` (`tools/mod.rs:515-520`) |
| H: permission mode frozen per turn | Looks fixed: `PermissionCell` + `SetPermissions` + application note |
| H: 200-call cap, closing summary discarded | Changed: 400 cap with a final no-tools call; summary returned as success (`mod.rs:233-253`) |
| M: fuzzy `repair_tool_call` | Replaced by an exact 9-alias table (`dispatch.rs:40-50`) |
| M: shallow retry (2 retries, 30 s) | Now 5 retries, Retry-After ≤ 120 s, replay allowed until a tool call is seen |
| M: summarizer inline, full provider timeout | Still inline on the call path, now capped at 90 s with a `Compacting` phase (`provider_guard.rs:400-405`) |
| M: summaries per-run, not durable | Looks fixed: summary is checkpointed (`mod.rs:431-434`) |
| M: steering only at loop boundaries | Still true by design (section 9) |
| M: child events lack a parent id | Looks fixed: `parent_id` on `ToolStarted`/`ToolFinished` |
| M: `initial_definitions` keyword-picks tools from the prompt | Now config-based (`tools/mod.rs:290-319`) |
| H: context ring shows summed prompt tokens | `Usage.context_tokens` = last call's input (`core/src/lib.rs:505-507`, `mod.rs:365`) + `ContextUsage` event |
| M: every MCP tool classed as a write | Annotation-based now (`mcp.rs:700-706`), but Ask mode still asks for every `connector_call` (`permissions.rs:314-317`) |
| M: embedding reload every call | 10-min failure backoff (`memory.rs:158`) |
| AUDIT 6 row "Memory: … not human-editable; degrades silently" | Facts are editable in the host UI (`memory_facts.rs`, `memory_forms.rs`); embedding status is surfaced (`host.rs:1979-1980`) |

I did not re-verify the rest of AUDIT (UI, host, secrets, extension hooks) against the current tree.

## 12. Gaps compared with a modern harness

"Confirmed absent" means I searched for it and read the relevant code path. "Not found" means it may exist somewhere I did not look.

| Capability | Status |
|---|---|
| Reactive compaction on provider `context_length_exceeded` | **Confirmed absent.** Mapped to `ContextOverflow` (`providers/mod.rs:200-204`), non-retryable (`retry.rs:66`); `model_call` has no compact-and-retry path (`mod.rs:338-398`). The turn fails. |
| Proactive pruning or clearing of stale tool outputs below the 80 % threshold | **Confirmed absent.** `fit` returns early under the threshold (`provider_guard.rs:435-437`). Old file reads and command outputs from earlier steps and turns stay verbatim (up to 64 KiB each) until compaction. |
| Token-count calibration from provider usage | **Confirmed absent.** `provider_input` only goes to the UI (`mod.rs:372-374`). Non-OpenAI models use o200k × 1.3. |
| Built-in per-model context-limit table | **Confirmed absent in engine.** Limits are user or catalog supplied; the 8192 default can be wrong for local servers whose catalog omits it. |
| Tool-output budget scaled to the context window | **Confirmed absent.** Fixed 64 KiB (`tools/mod.rs:1236`), whatever the window size. |
| Max-tokens (`Length`) continuation or recovery | **Confirmed absent.** `finish` is never read in the loop (searched `FinishReason::Length`/`.finish` in `agent_loop/`). |
| Empty-response handling | **Confirmed absent.** An empty text answer with no tools ends the turn as-is (`mod.rs:224-232`). |
| Loop, stall or repetition detection (identical calls, repeated failures) | **Confirmed absent.** Searched `stall`, `repeated`, `consecutive`, `doom`, `identical`, `loop guard`. Only the 400-call cap. |
| Semantic or ranked tool retrieval | **Absent.** `search_tools` is an unranked OR-substring match, first 12 (`tools/mod.rs:941-961`). Discovered schemas are not promoted into the native tools array. |
| MCP tools as first-class tools | **Absent by design.** Indirect via `connector_tools`/`connector_call`. `connector_tools` for stdio servers is classed as a write (`tools/mod.rs:1092-1093`), so it asks in Ask mode. |
| Prompt caching on Bedrock; explicit OpenAI `prompt_cache_key`; Gemini `cachedContent` | **Confirmed absent** (searched `cache` and `cachePoint` in `providers/`). |
| Cross-turn system-prompt stability | **Partial.** Stable within a turn; fact and learned-skill indexes are re-retrieved per turn from the user prompt (`tools/mod.rs:247-287`), which can bust the cache prefix. |
| Native tool-call fallback for local models without function calling; JSON repair; constrained decoding | **Confirmed absent** (section 10). |
| Anthropic and Gemini extended thinking requests | **Not found** in request bodies (`request.rs:230-394`); returned thinking is only replayed. |
| Agent-accessible conversation history search ("episodic memory") | **Confirmed absent** for the agent. FTS exists for the UI only. |
| Automatic memory extraction, consolidation or decay; model-driven fact updates | **Confirmed absent.** `memory_remember` cannot supersede (`builtin.rs:499-509`). There is no LLM-based memory writing. |
| Embedding retrieval for anything besides facts | **Confirmed absent** (learning uses FTS5 bm25; tools use substring). |
| Subagent types or roles, per-subagent model choice, depth > 1, transcript access | **Confirmed absent** (`subagent.rs`). One generic helper on the parent's model, depth 1, returning final text only. |
| Cheaper or separate model for summaries, titles and subagents | **Confirmed absent.** All use the turn's provider. Titles use `Reasoning::Lowest` (`engine.rs:253`). |
| User-triggered manual compaction (`/compact`) or context clearing | **Not found** (searched desktop, runtime and worker for `compact` and `clear context`). |
| JSON-Schema validation of tool arguments before dispatch | **Confirmed absent** (only an object check; `jsonschema` is not a dependency). Each tool validates its own arguments. |
| Parallel tool execution | **Present:** read-only calls up to 4 concurrent, writes serialized (`dispatch.rs`). |
| Context compaction | **Present:** LLM summary at 80 %, durable, incremental update, plus an excerpt fallback (`provider_guard.rs`). |
| Planning | **Present** only as `todo_write` (a checklist echoed to the UI) plus prompt instructions (`prompts/system.md:10`). No separate plan mode or planner. |

## 13. Quick map: research topic → code

- **Agent loop / ReAct step:** `agent_loop/mod.rs:192-272`
- **Tool-call parsing and error feedback:** `agent_loop/dispatch.rs:62-108`; `model.rs:418-484`
- **Parallel tool execution:** `agent_loop/dispatch.rs:110-161`
- **Retries and backoff:** `agent_loop/retry.rs`; `providers/mod.rs:112-219`
- **Context budgeting and token estimates:** `provider_guard.rs:183-311`
- **Summarization/compaction:** `provider_guard.rs:313-418`; `prompts/compaction*.md`
- **Observation truncation / spill-to-file:** `tools/mod.rs:1135-1285`; `provider_guard.rs:463-519`
- **Prompt caching:** `providers/request.rs:286-319`
- **System prompt / progressive disclosure:** `prompts/mod.rs:105-123`; `prompts/skills.rs:96-120`; `prompts/instructions.rs`
- **Long-term memory (hybrid RRF):** `services/memory.rs:405-482`; injection at `engine.rs:200-219`
- **Procedural memory:** `services/learning/persistence.rs:299-431`; `tools/mod.rs:260-287`
- **Tool retrieval:** `tools/mod.rs:290-319, 941-961`; `broker_tools.rs:38-86`
- **Human-in-the-loop approvals:** `tools/permissions.rs:234-336`; `tools/risk.rs`; `tools/mod.rs:472-605`
- **Multi-agent / subagents:** `subagent.rs`; `prompts/subagent.md`
- **Interrupt / steer:** `engine/src/steering.rs`; `services/steering.rs:276-345`
- **Durability / exactly-once:** `agent_loop/mod.rs:1-26`; `tools/mod.rs:429-433, 545-571`; `engine.rs:601-670`
